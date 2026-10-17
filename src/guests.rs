//! Several sessions on one desktop (parsec-comparison G15/G16,
//! docs/guests.md): what each session's events need, keyboard/mouse
//! arbitration, live approval of guests and the session list the host
//! settings window and `host-admin` show.
//!
//! Authorization is decided here, on the host, for every event before it is
//! injected; clients only display what they are told.
use crate::protocol::{ControlHolder, Event, Permissions, Role};
use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::{
    collections::BTreeMap,
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};
use tokio::sync::{mpsc, oneshot};

/// One connected session (direct, relayed or peer-to-peer).
pub type SessionId = u64;

/// Most sessions at once (each may attach [`crate::protocol::MAX_GAMEPADS`]
/// pads, so the host has `MAX_SESSIONS * MAX_GAMEPADS` pad slots).
pub const MAX_SESSIONS: usize = 8;

/// A guest may take keyboard and mouse from a holder idle this long (unless
/// an owner or the host locked control).
pub const HANDOVER: Duration = Duration::from_secs(3);

/// Session policy (host options, NixOS module).
#[derive(Clone, Debug)]
pub struct Policy {
    pub max_sessions: usize,
    /// When the last owner session leaves, disconnect the guests.
    pub kick_guests_with_owner: bool,
    /// `loginctl lock-session` when the last session leaves.
    pub lock_on_last_disconnect: bool,
    /// Disconnect a guest that sent no input for this long.
    pub guest_idle_kick: Option<Duration>,
    /// Total video bitrate for all sessions (kbit/s): the shared encoder runs
    /// at most at `cap / sessions`.
    pub bandwidth_cap: Option<u32>,
    /// A guest not approved within this time is denied.
    pub approval_timeout: Duration,
}

impl Default for Policy {
    fn default() -> Self {
        Self {
            max_sessions: MAX_SESSIONS,
            kick_guests_with_owner: true,
            lock_on_last_disconnect: false,
            guest_idle_kick: None,
            bandwidth_cap: None,
            approval_timeout: Duration::from_secs(30),
        }
    }
}

impl Policy {
    /// The encoder bitrate allowed with `sessions` connected.
    pub fn share(&self, sessions: usize) -> Option<u32> {
        self.bandwidth_cap
            .map(|cap| (cap / sessions.max(1) as u32).max(crate::protocol::MIN_BITRATE))
    }
}

/// What an event needs from the session that sent it.
#[derive(Debug, PartialEq, Eq, Clone, Copy)]
pub enum Need {
    /// Heartbeats, probes, rate feedback, keyframe requests, features and
    /// release: every session.
    Nothing,
    /// Injected keyboard or mouse input: permission plus control.
    KeyboardMouse,
    /// The session's own virtual pads.
    Gamepad,
    /// Explicit clipboard transfer.
    Clipboard,
    /// Changes the stream every session shares, or the host's displays:
    /// only the stream leader (the first owner, else the first session).
    Stream,
    /// Owner-only controls.
    Owner,
}

/// Classify `event`; exhaustive so a new event must be classified.
pub fn need(event: &Event) -> Need {
    match event {
        Event::Ping
        | Event::Probe { .. }
        | Event::Feedback { .. }
        | Event::NetworkFeedback { .. }
        | Event::MonitorFeedback { .. }
        | Event::RequestKeyframe { .. }
        | Event::ClientFeatures { .. }
        | Event::ReleaseAll => Need::Nothing,
        Event::Motion { .. }
        | Event::MonitorMotion { .. }
        | Event::RelativeMotion { .. }
        | Event::Button { .. }
        | Event::Key { .. }
        | Event::Scroll { .. } => Need::KeyboardMouse,
        Event::GamepadConnected { .. }
        | Event::GamepadDisconnected { .. }
        | Event::GamepadButton { .. }
        | Event::GamepadAxis { .. }
        | Event::GamepadTouch { .. }
        | Event::GamepadMotion { .. } => Need::Gamepad,
        Event::Clipboard { .. } | Event::ClipboardRequest => Need::Clipboard,
        Event::ConfigureVideo { .. }
        | Event::SelectMonitor { .. }
        | Event::SelectCodec { .. }
        | Event::ClientDisplay(_)
        | Event::MonitorStream { .. } => Need::Stream,
        Event::TakeControl { .. } | Event::ApproveGuest { .. } => Need::Owner,
    }
}

/// Whether a session with `role` and `permissions` may send `event` at all
/// (keyboard and mouse also need control, see [`Arbiter`]; stream changes
/// need the leader).
pub fn permitted(role: Role, permissions: Permissions, event: &Event) -> bool {
    if role == Role::Owner {
        return true;
    }
    match need(event) {
        Need::Nothing | Need::Stream => true,
        Need::KeyboardMouse => permissions.keyboard_mouse,
        Need::Gamepad => permissions.gamepad,
        Need::Clipboard => permissions.clipboard,
        Need::Owner => false,
    }
}

/// A guest's gamepad event as the host injects it: PlayStation pads become
/// Xbox pads and touchpad contacts are dropped. Linux desktops (libinput)
/// drive the pointer with a DualShock 4 or DualSense touchpad, which would
/// let a guest move and click past keyboard/mouse permission and control.
/// Owners keep their PlayStation pads.
pub fn guest_pad(role: Role, event: Event) -> Option<Event> {
    if role == Role::Owner {
        return Some(event);
    }
    match event {
        Event::GamepadConnected { pad, name, .. } => Some(Event::GamepadConnected {
            pad,
            name,
            kind: crate::protocol::GamepadType::Xbox,
        }),
        Event::GamepadTouch { .. } => None,
        event => Some(event),
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Holder {
    Session(SessionId),
    /// Someone at the host took control (settings window or `host-admin`).
    Host,
}

/// Who drives keyboard and mouse: one session at a time.
///
/// * Owners always get it on input (the host's owner can always take control
///   back), and may lock it against guests (`take`).
/// * A guest with the keyboard-mouse permission gets it when nobody holds it
///   or the holder has been idle for [`HANDOVER`], unless it is locked.
/// * Someone at the host can take it (`take_for_host`): guests are refused
///   until it is released; owners may still take it.
#[derive(Debug)]
pub struct Arbiter {
    holder: Option<Holder>,
    locked: bool,
    last_input: Instant,
}

/// The outcome of [`Arbiter::admit`].
#[derive(Debug, PartialEq, Eq)]
pub enum Admission {
    /// Inject it. `taken_from` lost control to this session (release what
    /// it held first).
    Allowed { taken_from: Option<SessionId> },
    /// Refused: another session or the host holds control.
    Busy,
}

impl Default for Arbiter {
    fn default() -> Self {
        Self::new(Instant::now())
    }
}

impl Arbiter {
    pub fn new(now: Instant) -> Self {
        Self {
            holder: None,
            locked: false,
            last_input: now,
        }
    }

    /// Keyboard/mouse input from session `id` (already [`permitted`]).
    pub fn admit(&mut self, id: SessionId, role: Role, now: Instant) -> Admission {
        let current = self.holder;
        let idle = now.saturating_duration_since(self.last_input) >= HANDOVER;
        let allowed = match (role, current) {
            (_, Some(Holder::Session(holder))) if holder == id => true,
            (Role::Owner, _) => true,
            (Role::Guest, _) if self.locked => false,
            (Role::Guest, None) => true,
            (Role::Guest, Some(_)) => idle,
        };
        if !allowed {
            return Admission::Busy;
        }
        self.holder = Some(Holder::Session(id));
        self.last_input = now;
        Admission::Allowed {
            taken_from: match current {
                Some(Holder::Session(previous)) if previous != id => Some(previous),
                _ => None,
            },
        }
    }

    /// An owner session locks control for itself (`Event::TakeControl`).
    /// Returns the session that lost it.
    pub fn take(&mut self, id: SessionId, now: Instant) -> Option<SessionId> {
        let previous = self.holder_session().filter(|previous| *previous != id);
        self.holder = Some(Holder::Session(id));
        self.locked = true;
        self.last_input = now;
        previous
    }

    /// Someone at the host takes control: every session is refused until
    /// [`Self::release`], except owners, who may take it again.
    pub fn take_for_host(&mut self, now: Instant) -> Option<SessionId> {
        let previous = self.holder_session();
        self.holder = Some(Holder::Host);
        self.locked = true;
        self.last_input = now;
        previous
    }

    /// Guests may take control again (after [`HANDOVER`] of idleness).
    pub fn release(&mut self) {
        self.locked = false;
        if self.holder == Some(Holder::Host) {
            self.holder = None;
        }
    }

    /// The session left or lost its keyboard-mouse permission.
    pub fn forget(&mut self, id: SessionId) -> bool {
        if self.holder == Some(Holder::Session(id)) {
            self.holder = None;
            true
        } else {
            false
        }
    }

    pub fn holder_session(&self) -> Option<SessionId> {
        match self.holder {
            Some(Holder::Session(id)) => Some(id),
            _ => None,
        }
    }

    /// The holder as session `viewer` sees it; `role_of` names other
    /// sessions' roles.
    pub fn view(
        &self,
        viewer: SessionId,
        role_of: impl Fn(SessionId) -> Option<Role>,
    ) -> ControlHolder {
        match self.holder {
            None => ControlHolder::Nobody,
            Some(Holder::Host) => ControlHolder::Host,
            Some(Holder::Session(id)) if id == viewer => ControlHolder::You,
            Some(Holder::Session(id)) => match role_of(id) {
                Some(Role::Owner) => ControlHolder::Owner,
                Some(Role::Guest) => ControlHolder::Guest,
                None => ControlHolder::Nobody,
            },
        }
    }
}

/// A connected session as the host's settings window and `host-admin` list it.
#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct SessionSummary {
    pub id: SessionId,
    pub name: String,
    /// Managed device id (revoke or change it); none for the host pairing
    /// file and Linux login.
    pub device: Option<String>,
    pub role: Role,
    pub permissions: Permissions,
    /// Holds keyboard and mouse.
    pub control: bool,
    /// `direct`, `relay` or `p2p`.
    pub transport: String,
    pub connected_s: u64,
    pub idle_s: u64,
}

/// A guest waiting for approval.
#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct ApprovalSummary {
    pub id: SessionId,
    pub name: String,
    pub device: Option<String>,
    /// Seconds until it is denied.
    pub remaining_s: u64,
}

/// Requests to the running session engine.
#[derive(Debug, Clone)]
pub enum Command {
    /// Disconnect a session.
    Kick(SessionId),
    /// Re-read every session's role and permissions now (revocation,
    /// permission change, trusted flag).
    Refresh,
    /// Someone at the host takes keyboard and mouse control.
    TakeControl,
    /// Guests may take control again.
    ReleaseControl,
    /// The pending approvals changed (prompt owners' clients).
    Approvals,
    /// The host is stopping: end every session.
    Shutdown,
}

struct Pending {
    summary: ApprovalSummary,
    deadline: Instant,
    answer: Option<oneshot::Sender<bool>>,
}

#[derive(Default)]
struct State {
    next_id: SessionId,
    sessions: Vec<SessionSummary>,
    approvals: BTreeMap<SessionId, Pending>,
    engine: Option<mpsc::UnboundedSender<Command>>,
    /// Guest devices refused for a while (denied, timed out or kicked).
    blocked: std::collections::HashMap<String, Instant>,
}

/// Most guests waiting for approval at once; more are refused.
pub const MAX_PENDING: usize = MAX_SESSIONS;
/// A guest denied (or not answered) may ask again only after this long.
pub const DENIED_FOR: Duration = Duration::from_secs(30);
/// A kicked guest may ask again only after this long.
pub const KICKED_FOR: Duration = Duration::from_secs(60);

/// Shared between the accept loop, the session engine and the admin socket.
pub struct Hub {
    pub policy: Policy,
    state: Mutex<State>,
}

impl Hub {
    pub fn new(policy: Policy) -> Arc<Self> {
        Arc::new(Self {
            policy,
            state: Mutex::default(),
        })
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, State> {
        self.state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// A fresh session id (never reused while the host runs).
    pub fn next_id(&self) -> SessionId {
        let mut state = self.lock();
        state.next_id += 1;
        state.next_id
    }

    fn notify(state: &State, command: Command) {
        if let Some(engine) = &state.engine {
            let _ = engine.send(command);
        }
    }

    /// Ask for approval of guest session `id`; the receiver gets the answer
    /// (dropped on timeout or withdrawal, which callers treat as denial).
    /// One request per device: a new one replaces (denies) the older. `None`
    /// when [`MAX_PENDING`] guests are already waiting.
    pub fn request(
        &self,
        id: SessionId,
        name: &str,
        device: Option<String>,
    ) -> Option<oneshot::Receiver<bool>> {
        let (sender, receiver) = oneshot::channel();
        let mut state = self.lock();
        if device.is_some() {
            state
                .approvals
                .retain(|_, pending| pending.summary.device != device);
        }
        if state.approvals.len() >= MAX_PENDING {
            return None;
        }
        state.approvals.insert(
            id,
            Pending {
                summary: ApprovalSummary {
                    id,
                    name: name.into(),
                    device,
                    remaining_s: self.policy.approval_timeout.as_secs(),
                },
                deadline: Instant::now() + self.policy.approval_timeout,
                answer: Some(sender),
            },
        );
        Self::notify(&state, Command::Approvals);
        Some(receiver)
    }

    /// Refuse guest `device` for `duration` (denied, not answered, kicked).
    pub fn block(&self, device: &str, duration: Duration) {
        let mut state = self.lock();
        let now = Instant::now();
        state.blocked.retain(|_, until| *until > now);
        if state.blocked.len() < 1024 {
            state.blocked.insert(device.into(), now + duration);
        }
    }

    pub fn blocked(&self, device: &str) -> bool {
        self.lock()
            .blocked
            .get(device)
            .is_some_and(|until| *until > Instant::now())
    }

    /// Whether an owner's session is connected now.
    pub fn owner_connected(&self) -> bool {
        self.lock()
            .sessions
            .iter()
            .any(|session| session.role == Role::Owner)
    }

    /// Whether a guest's session is connected now.
    pub fn guest_connected(&self) -> bool {
        self.lock()
            .sessions
            .iter()
            .any(|session| session.role == Role::Guest)
    }

    /// Answer a pending approval (host settings, `host-admin`, an owner's
    /// client). Errors if it is no longer pending.
    pub fn answer(&self, id: SessionId, allow: bool) -> Result<()> {
        let mut state = self.lock();
        let mut pending = state
            .approvals
            .remove(&id)
            .context("no such guest request (answered, timed out or disconnected)")?;
        if let Some(answer) = pending.answer.take() {
            let _ = answer.send(allow);
        }
        Self::notify(&state, Command::Approvals);
        Ok(())
    }

    /// The request ended without an answer (timeout, guest disconnected).
    pub fn withdraw(&self, id: SessionId) {
        let mut state = self.lock();
        if state.approvals.remove(&id).is_some() {
            Self::notify(&state, Command::Approvals);
        }
    }

    pub fn approvals(&self) -> Vec<ApprovalSummary> {
        let now = Instant::now();
        self.lock()
            .approvals
            .values()
            .map(|pending| ApprovalSummary {
                remaining_s: pending.deadline.saturating_duration_since(now).as_secs(),
                ..pending.summary.clone()
            })
            .collect()
    }

    pub fn sessions(&self) -> Vec<SessionSummary> {
        self.lock().sessions.clone()
    }

    /// The engine publishes its session list.
    pub fn set_sessions(&self, sessions: Vec<SessionSummary>) {
        self.lock().sessions = sessions;
    }

    /// The engine starts (`Some`) or stops (`None`) taking commands.
    pub fn attach(&self, engine: Option<mpsc::UnboundedSender<Command>>) {
        let mut state = self.lock();
        if engine.is_none() {
            state.sessions.clear();
        }
        state.engine = engine;
    }

    /// Send a command to the running engine; fails when no session runs.
    pub fn command(&self, command: Command) -> Result<()> {
        let state = self.lock();
        let engine = state.engine.as_ref().context("no session is connected")?;
        engine
            .send(command)
            .map_err(|_| anyhow::anyhow!("the session is ending"))
    }

    /// [`Self::command`] where no running session is fine (refresh after a
    /// revocation).
    pub fn poke(&self, command: Command) {
        Self::notify(&self.lock(), command);
    }
}

/// Wait for approval of guest session `id` (or deny on timeout, or when
/// `gone` resolves: the guest disconnected while waiting).
pub async fn await_approval(
    hub: &Hub,
    id: SessionId,
    name: &str,
    device: Option<String>,
    gone: impl std::future::Future<Output = ()>,
) -> Approval {
    let Some(receiver) = hub.request(id, name, device) else {
        tracing::warn!(
            session = id,
            "too many guests waiting for approval; refused"
        );
        return Approval::Busy;
    };
    let outcome = tokio::select! {
        answer = tokio::time::timeout(hub.policy.approval_timeout, receiver) => match answer {
            Ok(Ok(true)) => Approval::Allowed,
            // Denied, or replaced by a newer request of the same device.
            Ok(Ok(false)) | Ok(Err(_)) => Approval::Denied,
            Err(_) => Approval::TimedOut,
        },
        _ = gone => Approval::Gone,
    };
    // Answered requests are gone already; anything else is withdrawn.
    hub.withdraw(id);
    if outcome == Approval::TimedOut {
        tracing::info!(session = id, "guest approval timed out; denied");
    }
    outcome
}

/// How a guest's request for approval ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Approval {
    Allowed,
    Denied,
    TimedOut,
    /// The guest left while it waited.
    Gone,
    /// Too many guests are waiting.
    Busy,
}

/// Lock the host's graphical session (`--lock-on-last-disconnect`): the
/// caller's session when `XDG_SESSION_ID` is set (a terminal in the
/// session), else the user's display session from logind (user service).
pub async fn lock_session() -> Result<()> {
    let session = match std::env::var("XDG_SESSION_ID") {
        Ok(id) if !id.is_empty() => id,
        _ => {
            let output = tokio::process::Command::new("loginctl")
                .args([
                    "show-user",
                    &unsafe { libc::getuid() }.to_string(),
                    "--property=Display",
                    "--value",
                ])
                .output()
                .await
                .context("run loginctl")?;
            String::from_utf8_lossy(&output.stdout).trim().to_owned()
        }
    };
    let mut command = tokio::process::Command::new("loginctl");
    command.arg("lock-session");
    if !session.is_empty() {
        command.arg(&session);
    }
    let output = command
        .output()
        .await
        .context("run loginctl lock-session")?;
    anyhow::ensure!(
        output.status.success(),
        "loginctl lock-session failed: {}",
        String::from_utf8_lossy(&output.stderr).trim()
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::{Event, Permissions, Role};

    fn key() -> Event {
        Event::Key {
            code: 30,
            down: true,
        }
    }

    fn pad() -> Event {
        Event::GamepadButton {
            pad: 0,
            button: 0,
            down: true,
        }
    }

    #[test]
    fn guests_get_only_what_they_were_granted() {
        let guest = Permissions::GUEST_DEFAULT;
        assert!(permitted(Role::Guest, guest, &pad()));
        assert!(!permitted(Role::Guest, guest, &key()));
        assert!(!permitted(
            Role::Guest,
            guest,
            &Event::Motion { x: 0.5, y: 0.5 }
        ));
        assert!(!permitted(
            Role::Guest,
            guest,
            &Event::RelativeMotion { dx: 1, dy: 1 }
        ));
        assert!(!permitted(
            Role::Guest,
            guest,
            &Event::Scroll { x: 0, y: 1 }
        ));
        assert!(!permitted(Role::Guest, guest, &Event::ClipboardRequest));
        assert!(!permitted(
            Role::Guest,
            guest,
            &Event::MonitorMotion {
                index: 1,
                x: 0.0,
                y: 0.0
            }
        ));
        // Owner-only controls, whatever was granted.
        for event in [
            Event::TakeControl { take: true },
            Event::ApproveGuest { id: 1, allow: true },
        ] {
            assert!(!permitted(Role::Guest, Permissions::ALL, &event));
            assert!(permitted(Role::Owner, Permissions::default(), &event));
        }
        let view_only = Permissions::default();
        assert!(!permitted(Role::Guest, view_only, &pad()));
        assert!(!permitted(
            Role::Guest,
            view_only,
            &Event::GamepadConnected {
                pad: 0,
                name: "pad".into(),
                kind: Default::default()
            }
        ));
        // Heartbeats and feedback always pass (they keep the session alive).
        assert!(permitted(Role::Guest, view_only, &Event::Ping));
        assert!(permitted(Role::Guest, view_only, &Event::ReleaseAll));
        let keyboard = Permissions {
            keyboard_mouse: true,
            ..view_only
        };
        assert!(permitted(Role::Guest, keyboard, &key()));
        assert!(!permitted(Role::Guest, keyboard, &pad()));
        assert!(permitted(Role::Owner, view_only, &key()));
        assert_eq!(need(&Event::SelectMonitor { index: 0 }), Need::Stream);
        assert_eq!(
            need(&Event::Clipboard { text: "x".into() }),
            Need::Clipboard
        );
    }

    #[test]
    fn guests_get_xbox_pads_without_a_touchpad_pointer() {
        use crate::protocol::GamepadType;
        let dualsense = Event::GamepadConnected {
            pad: 1,
            name: "DualSense".into(),
            kind: GamepadType::DualSense,
        };
        let touch = Event::GamepadTouch {
            pad: 1,
            finger: 0,
            down: true,
            x: 0.5,
            y: 0.5,
        };
        assert!(matches!(
            guest_pad(Role::Guest, dualsense.clone()),
            Some(Event::GamepadConnected {
                pad: 1,
                kind: GamepadType::Xbox,
                ..
            })
        ));
        assert!(guest_pad(Role::Guest, touch.clone()).is_none());
        assert!(guest_pad(Role::Guest, pad()).is_some());
        assert!(matches!(
            guest_pad(Role::Owner, dualsense),
            Some(Event::GamepadConnected {
                kind: GamepadType::DualSense,
                ..
            })
        ));
        assert!(guest_pad(Role::Owner, touch).is_some());
    }

    #[test]
    fn one_session_drives_keyboard_and_mouse_and_owners_take_it_back() {
        let start = Instant::now();
        let at = |ms| start + Duration::from_millis(ms);
        let mut arbiter = Arbiter::new(start);
        // The first guest takes free control; another guest waits.
        assert_eq!(
            arbiter.admit(2, Role::Guest, at(0)),
            Admission::Allowed { taken_from: None }
        );
        assert_eq!(arbiter.admit(3, Role::Guest, at(100)), Admission::Busy);
        assert_eq!(
            arbiter.admit(2, Role::Guest, at(200)),
            Admission::Allowed { taken_from: None }
        );
        // An idle holder hands over.
        assert_eq!(
            arbiter.admit(3, Role::Guest, at(200) + HANDOVER),
            Admission::Allowed {
                taken_from: Some(2)
            }
        );
        // The owner always takes it, at once.
        assert_eq!(
            arbiter.admit(1, Role::Owner, at(200) + HANDOVER),
            Admission::Allowed {
                taken_from: Some(3)
            }
        );
        assert_eq!(
            arbiter.admit(3, Role::Guest, at(300) + HANDOVER),
            Admission::Busy
        );
        // Owner lock: no guest gets it, however long the owner idles.
        assert_eq!(arbiter.take(1, at(400) + HANDOVER), None);
        assert_eq!(arbiter.admit(3, Role::Guest, at(60_000)), Admission::Busy);
        arbiter.release();
        assert_eq!(
            arbiter.admit(3, Role::Guest, at(60_000)),
            Admission::Allowed {
                taken_from: Some(1)
            }
        );
        // Someone at the host takes control: guests are refused until it
        // is released; owners may still take it.
        assert_eq!(arbiter.take_for_host(at(60_100)), Some(3));
        assert_eq!(arbiter.view(3, |_| Some(Role::Guest)), ControlHolder::Host);
        assert_eq!(arbiter.admit(3, Role::Guest, at(120_000)), Admission::Busy);
        assert_eq!(
            arbiter.admit(1, Role::Owner, at(120_000)),
            Admission::Allowed { taken_from: None }
        );
        assert_eq!(arbiter.view(3, |_| Some(Role::Owner)), ControlHolder::Owner);
        assert_eq!(arbiter.view(1, |_| Some(Role::Owner)), ControlHolder::You);
        // A holder that leaves frees control.
        assert!(arbiter.forget(1));
        assert!(!arbiter.forget(1));
        arbiter.release();
        assert_eq!(arbiter.view(3, |_| None), ControlHolder::Nobody);
        assert_eq!(
            arbiter.admit(3, Role::Guest, at(120_001)),
            Admission::Allowed { taken_from: None }
        );
    }

    #[tokio::test]
    async fn approvals_are_answered_once_or_denied_on_timeout() {
        let hub = Hub::new(Policy {
            approval_timeout: Duration::from_millis(200),
            ..Policy::default()
        });
        // Allowed.
        let id = hub.next_id();
        let waiting = {
            let hub = hub.clone();
            tokio::spawn(async move {
                await_approval(&hub, id, "Guest", None, std::future::pending()).await
            })
        };
        tokio::time::sleep(Duration::from_millis(20)).await;
        assert_eq!(hub.approvals().len(), 1);
        hub.answer(id, true).unwrap();
        assert_eq!(waiting.await.unwrap(), Approval::Allowed);
        // Answers are single use.
        assert!(hub.answer(id, false).is_err());
        assert!(hub.approvals().is_empty());
        // Denied.
        let id = hub.next_id();
        let waiting = {
            let hub = hub.clone();
            tokio::spawn(async move {
                await_approval(&hub, id, "Guest", None, std::future::pending()).await
            })
        };
        tokio::time::sleep(Duration::from_millis(20)).await;
        hub.answer(id, false).unwrap();
        assert_eq!(waiting.await.unwrap(), Approval::Denied);
        // Timed out: denied and withdrawn.
        let id = hub.next_id();
        assert_eq!(
            await_approval(&hub, id, "Guest", None, std::future::pending()).await,
            Approval::TimedOut
        );
        assert!(hub.approvals().is_empty());
        assert!(hub.answer(id, true).is_err());
        // The guest left while waiting.
        let id = hub.next_id();
        assert_eq!(
            await_approval(&hub, id, "Guest", None, async {}).await,
            Approval::Gone
        );
        assert!(hub.approvals().is_empty());
    }

    #[tokio::test]
    async fn guests_cannot_flood_the_approval_prompt() {
        let hub = Hub::new(Policy::default());
        // One pending request per device: the newer replaces (denies) the older.
        let first = hub.request(1, "Mallory", Some("device".into())).unwrap();
        let second = hub.request(2, "Mallory", Some("device".into())).unwrap();
        assert!(first.await.is_err());
        assert_eq!(hub.approvals().len(), 1);
        // A cap on everyone waiting.
        let mut held = vec![second];
        for id in 3..(2 + MAX_PENDING as u64) {
            held.push(hub.request(id, "Guest", Some(format!("d{id}"))).unwrap());
        }
        assert_eq!(hub.approvals().len(), MAX_PENDING);
        assert!(hub.request(99, "Guest", Some("d99".into())).is_none());
        // Blocks expire.
        hub.block("device", Duration::from_millis(50));
        assert!(hub.blocked("device") && !hub.blocked("other"));
        tokio::time::sleep(Duration::from_millis(60)).await;
        assert!(!hub.blocked("device"));
        drop(held);
    }

    #[test]
    fn bandwidth_cap_is_split_between_sessions() {
        let policy = Policy {
            bandwidth_cap: Some(30_000),
            ..Policy::default()
        };
        assert_eq!(policy.share(1), Some(30_000));
        assert_eq!(policy.share(3), Some(10_000));
        assert_eq!(policy.share(0), Some(30_000));
        // Never below a usable floor.
        assert_eq!(policy.share(100), Some(crate::protocol::MIN_BITRATE));
        assert_eq!(Policy::default().share(4), None);
    }
}
