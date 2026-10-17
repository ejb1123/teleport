//! Virtual outputs through the ScreenCast portal (headless hosting, extra
//! monitors, privacy mode) and KDE output control with `kscreen-doctor`.
//!
//! The portal's VIRTUAL source type makes the compositor create an output
//! that lives as long as the portal session: xdg-desktop-portal-kde asks KWin
//! for `Virtual-virtual-xdp-kde-<app id>[-N]` at 1920x1080 (KWin's
//! `zkde_screencast_unstable_v1.stream_virtual_output`), GNOME's portal uses
//! Mutter's `RecordVirtual`. Closing the session, or the host's D-Bus
//! connection dropping, removes the output. On KDE the output takes custom
//! modes, so it is resized here to the client's display. See
//! docs/virtual-display.md.
use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};
use std::{
    process::{Command, Stdio},
    time::{Duration, Instant},
};

/// `--virtual-display`: when the host streams a virtual output.
#[derive(clap::ValueEnum, Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum VirtualDisplay {
    /// Never create virtual outputs (no headless fallback, no extra monitor).
    Never,
    /// Stream the physical monitors; a virtual output when there is none
    /// (headless, or every monitor disconnected while asleep). Clients may
    /// add an extra virtual monitor.
    #[default]
    Fallback,
    /// Always stream a virtual output sized to the client (headless server).
    Always,
}

/// What the primary portal session captures.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Plan {
    Monitors,
    /// Monitors, or a virtual output when the portal returns none.
    MonitorsOrVirtual,
    Virtual,
}

/// The primary session's source. `privacy`: the physical monitors are off for
/// this session, so only a virtual output can be streamed.
pub fn plan(mode: VirtualDisplay, virtual_available: bool, privacy: bool) -> Result<Plan> {
    if privacy || mode == VirtualDisplay::Always {
        ensure!(
            virtual_available,
            "the desktop portal offers no virtual display (needs xdg-desktop-portal-kde or -gnome with VIRTUAL screencast)"
        );
        return Ok(Plan::Virtual);
    }
    Ok(match mode {
        VirtualDisplay::Fallback if virtual_available => Plan::MonitorsOrVirtual,
        _ => Plan::Monitors,
    })
}

/// Whether turning privacy mode `from` → `to` needs a new portal session:
/// not when a virtual output is already streamed and stays (privacy on over
/// a virtual display, or off with `--virtual-display always`).
pub fn privacy_reopens(mode: VirtualDisplay, primary_virtual: bool, from: bool, to: bool) -> bool {
    if from == to {
        return false;
    }
    !(primary_virtual && (to || mode == VirtualDisplay::Always))
}

/// Why a monitor session could not be used.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MonitorFailure {
    /// The portal shared nothing, or only virtual outputs.
    NoMonitor,
    /// The portal backend failed the request (KDE does this when it has no
    /// screen at all).
    PortalError,
    /// The local user dismissed the dialog.
    Cancelled,
    /// Anything else (permissions, D-Bus).
    Other,
}

/// Whether `Plan::MonitorsOrVirtual` falls back to a virtual output.
/// A dismissed dialog is a refusal, not a missing monitor.
pub fn falls_back(failure: MonitorFailure) -> bool {
    matches!(
        failure,
        MonitorFailure::NoMonitor | MonitorFailure::PortalError
    )
}

/// KWin names portal virtual outputs `Virtual-` + xdg-desktop-portal-kde's
/// `virtual-xdp-kde-<app id>[-N]`; physical (or, in a nested KWin, the
/// backend's own `Virtual-0`) outputs never start with `Virtual-virtual-`.
pub fn is_portal_virtual(name: &str) -> bool {
    name.starts_with("Virtual-virtual-")
}

/// A virtual output's mode, from the client's display.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Size {
    pub width: u32,
    pub height: u32,
    pub refresh_mhz: u32,
}

impl Size {
    /// The portal's default (xdg-desktop-portal-kde creates 1920x1080).
    pub const DEFAULT: Self = Self {
        width: 1920,
        height: 1080,
        refresh_mhz: 60_000,
    };

    /// The client's video area (physical pixels) and refresh, clamped to
    /// sizes a compositor and encoder take; unknown values keep the default.
    pub fn for_client(display: &crate::protocol::ClientDisplay) -> Self {
        let (width, height) = if display.width >= 320 && display.height >= 200 {
            (
                display.width.min(7680) / 2 * 2,
                display.height.min(4320) / 2 * 2,
            )
        } else {
            (Self::DEFAULT.width, Self::DEFAULT.height)
        };
        let refresh_mhz = if display.refresh_mhz == 0 {
            Self::DEFAULT.refresh_mhz
        } else {
            display
                .refresh_mhz
                .clamp(30_000, crate::protocol::MAX_FPS * 1000)
        };
        Self {
            width,
            height,
            refresh_mhz,
        }
    }
}

/// One output in `kscreen-doctor --json`.
#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Output {
    pub id: u32,
    pub name: String,
    pub enabled: bool,
    pub connected: bool,
    #[serde(default)]
    pub pos: Point,
    #[serde(default)]
    pub current_mode_id: String,
    #[serde(default)]
    pub modes: Vec<Mode>,
    #[serde(default = "one")]
    pub scale: f64,
    #[serde(default)]
    pub priority: u32,
}

fn one() -> f64 {
    1.0
}

#[derive(Clone, Copy, Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
pub struct Point {
    pub x: i32,
    pub y: i32,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Mode {
    pub id: String,
    pub name: String,
    pub size: ModeSize,
    /// Hz.
    pub refresh_rate: f64,
}

#[derive(Clone, Copy, Debug, Deserialize)]
pub struct ModeSize {
    pub width: u32,
    pub height: u32,
}

impl Output {
    pub fn is_physical(&self) -> bool {
        self.connected && !is_portal_virtual(&self.name)
    }

    fn current_mode(&self) -> Option<&Mode> {
        self.modes
            .iter()
            .find(|mode| mode.id == self.current_mode_id)
    }

    /// A mode of this size within 1 Hz of `refresh_mhz`.
    fn mode_matching(&self, size: Size) -> Option<&Mode> {
        self.modes.iter().find(|mode| {
            mode.size.width == size.width
                && mode.size.height == size.height
                && (mode.refresh_rate * 1000.0 - f64::from(size.refresh_mhz)).abs() <= 1000.0
        })
    }
}

pub fn parse_outputs(json: &[u8]) -> Result<Vec<Output>> {
    #[derive(Deserialize)]
    struct Config {
        outputs: Vec<Output>,
    }
    Ok(serde_json::from_slice::<Config>(json)
        .context("parsing kscreen-doctor --json")?
        .outputs)
}

/// The `kscreen-doctor` arguments that resize a virtual output, if it already
/// has a matching mode (selected by id: output names with dots, like the
/// portal's, cannot be addressed by name).
fn select_mode_args(output: &Output, size: Size) -> Option<Vec<String>> {
    let mode = output.mode_matching(size)?;
    Some(if mode.id == output.current_mode_id {
        Vec::new()
    } else {
        vec![format!("output.{}.mode.{}", output.id, mode.id)]
    })
}

/// Adds the custom mode; KWin keeps at most a few per output name, so the
/// oldest goes first once there are four.
fn add_mode_args(output: &Output, size: Size, custom_modes: usize) -> Vec<String> {
    let mut args = Vec::new();
    if custom_modes >= 4 {
        args.push(format!("output.{}.removeCustomMode.0", output.id));
    }
    args.push(format!(
        "output.{}.addCustomMode.{}.{}.{}.full",
        output.id, size.width, size.height, size.refresh_mhz
    ));
    args
}

/// What to disable for privacy: every enabled physical output.
pub fn privacy_targets(outputs: &[Output]) -> Vec<&Output> {
    outputs
        .iter()
        .filter(|output| output.is_physical() && output.enabled)
        .collect()
}

pub fn disable_args(targets: &[&Output]) -> Vec<String> {
    targets
        .iter()
        .map(|output| format!("output.{}.disable", output.id))
        .collect()
}

/// A physical output as it was before privacy mode turned it off.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq)]
pub struct Saved {
    pub name: String,
    pub pos: Point,
    /// Mode name (`2560x1440@144`), matched again at restore time.
    #[serde(default)]
    pub mode: Option<String>,
    pub scale: f64,
    pub priority: u32,
}

impl Saved {
    pub fn of(output: &Output) -> Self {
        Self {
            name: output.name.clone(),
            pos: output.pos,
            mode: output.current_mode().map(|mode| mode.name.clone()),
            scale: output.scale,
            priority: output.priority,
        }
    }
}

/// `kscreen-doctor` arguments re-enabling the saved outputs that are still
/// connected and disabled, where and how they were. Outputs addressed by id
/// (current ids: they change when outputs come and go).
pub fn restore_args(saved: &[Saved], current: &[Output]) -> Vec<String> {
    let mut args = Vec::new();
    for saved in saved {
        let Some(output) = current
            .iter()
            .find(|output| output.name == saved.name && output.connected && !output.enabled)
        else {
            continue;
        };
        let id = output.id;
        args.push(format!("output.{id}.enable"));
        args.push(format!(
            "output.{id}.position.{},{}",
            saved.pos.x, saved.pos.y
        ));
        if let Some(mode) = saved
            .mode
            .as_ref()
            .and_then(|name| output.modes.iter().find(|mode| &mode.name == name))
        {
            args.push(format!("output.{id}.mode.{}", mode.id));
        }
        if saved.scale > 0.0 {
            args.push(format!("output.{id}.scale.{}", saved.scale));
        }
        if saved.priority > 0 {
            args.push(format!("output.{id}.priority.{}", saved.priority));
        }
    }
    args
}

/// Timeout for one `kscreen-doctor` run (it talks to the compositor).
const KSCREEN_TIMEOUT: Duration = Duration::from_secs(10);

fn kscreen_doctor(args: &[String]) -> Result<Vec<u8>> {
    let program =
        std::env::var_os("TELEPORT_KSCREEN_DOCTOR").unwrap_or_else(|| "kscreen-doctor".into());
    let mut child = Command::new(&program)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .context("kscreen-doctor is unavailable (KDE output control)")?;
    let started = Instant::now();
    let status = loop {
        if let Some(status) = child.try_wait()? {
            break status;
        }
        if started.elapsed() > KSCREEN_TIMEOUT {
            let _ = child.kill();
            let _ = child.wait();
            anyhow::bail!("kscreen-doctor timed out");
        }
        std::thread::sleep(Duration::from_millis(20));
    };
    let mut stdout = Vec::new();
    let mut stderr = Vec::new();
    use std::io::Read;
    if let Some(mut pipe) = child.stdout.take() {
        pipe.read_to_end(&mut stdout)?;
    }
    if let Some(mut pipe) = child.stderr.take() {
        pipe.read_to_end(&mut stderr)?;
    }
    ensure!(
        status.success(),
        "kscreen-doctor {} failed: {}",
        args.join(" "),
        String::from_utf8_lossy(&stderr).trim()
    );
    Ok(stdout)
}

/// The compositor's outputs (KDE only).
pub fn outputs() -> Result<Vec<Output>> {
    parse_outputs(&kscreen_doctor(&["--json".into()])?)
}

/// The only output named `name`. xdg-desktop-portal-kde gives every virtual
/// output of an app the same name, and KWin derives the same UUID from it,
/// so with two of them neither can be told apart (and none is resized).
fn only_named(outputs: Vec<Output>, name: &str) -> Result<Output> {
    let mut named: Vec<_> = outputs
        .into_iter()
        .filter(|output| output.name == name)
        .collect();
    ensure!(
        named.len() == 1,
        "{} outputs are named {name}; cannot tell them apart",
        named.len()
    );
    Ok(named.remove(0))
}

/// Apply `args` in one atomic `kscreen-doctor` configuration.
pub fn apply(args: &[String]) -> Result<()> {
    if args.is_empty() {
        return Ok(());
    }
    tracing::debug!(?args, "kscreen-doctor");
    kscreen_doctor(args).map(drop)
}

/// Whether KDE output control works here (privacy mode needs it).
pub fn kscreen_available() -> bool {
    outputs().is_ok_and(|outputs| outputs.iter().any(Output::is_physical))
}

/// Resize the virtual output `name` (the portal stream's `mapping_id`) to
/// `size`. Returns the size it now has.
pub fn resize(name: &str, size: Size) -> Result<Size> {
    let find = |outputs: Vec<Output>| only_named(outputs, name);
    let output = find(outputs()?)?;
    let args = match select_mode_args(&output, size) {
        Some(args) => args,
        None => {
            // The portal's own mode plus the custom ones.
            let custom = output.modes.len().saturating_sub(1);
            apply(&add_mode_args(&output, size, custom))?;
            let output = find(outputs()?)?;
            select_mode_args(&output, size).context("the compositor did not add the custom mode")?
        }
    };
    apply(&args)?;
    Ok(size)
}

/// After creating a virtual output for anything but privacy mode: KWin
/// applies the layout it stored for this set of outputs, which is "physical
/// monitors off" if an earlier privacy session ended without restoring them
/// (host killed). Turn back on any physical output that was on before.
pub fn keep_enabled(before: &[String]) -> Result<()> {
    let current = outputs()?;
    let args: Vec<_> = current
        .iter()
        .filter(|output| output.is_physical() && !output.enabled && before.contains(&output.name))
        .map(|output| format!("output.{}.enable", output.id))
        .collect();
    if !args.is_empty() {
        tracing::warn!(
            ?args,
            "re-enabling physical monitors the compositor turned off with the virtual display"
        );
    }
    apply(&args)
}

/// Names of the enabled physical outputs (KDE); empty elsewhere.
pub fn enabled_physical() -> Vec<String> {
    outputs()
        .map(|outputs| {
            outputs
                .into_iter()
                .filter(|output| output.is_physical() && output.enabled)
                .map(|output| output.name)
                .collect()
        })
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    const KSCREEN: &str = r#"{"features":255,"outputs":[
      {"id":1,"name":"DP-1","enabled":true,"connected":true,"pos":{"x":0,"y":0},
       "currentModeId":"3","scale":1.25,"priority":1,
       "modes":[{"id":"3","name":"2560x1440@144","refreshRate":143.97,"size":{"width":2560,"height":1440}},
                {"id":"4","name":"1920x1080@60","refreshRate":60,"size":{"width":1920,"height":1080}}]},
      {"id":2,"name":"HDMI-A-1","enabled":false,"connected":true,"pos":{"x":2048,"y":0},
       "currentModeId":"9","scale":1,"priority":0,
       "modes":[{"id":"9","name":"1920x1080@60","refreshRate":60,"size":{"width":1920,"height":1080}}]},
      {"id":5,"name":"Virtual-virtual-xdp-kde-dev.teleport.Host","enabled":true,"connected":true,
       "pos":{"x":4096,"y":0},"currentModeId":"11","scale":1,"priority":3,
       "modes":[{"id":"11","name":"1920x1080@60","refreshRate":60,"size":{"width":1920,"height":1080}},
                {"id":"12","name":"2560x1600@120","refreshRate":119.99,"size":{"width":2560,"height":1600}}]},
      {"id":6,"name":"DP-2","enabled":false,"connected":false,"modes":[]}
    ]}"#;

    #[test]
    fn no_monitor_falls_back_to_a_virtual_output_only_when_allowed() {
        assert_eq!(
            plan(VirtualDisplay::Fallback, true, false).unwrap(),
            Plan::MonitorsOrVirtual
        );
        assert_eq!(
            plan(VirtualDisplay::Fallback, false, false).unwrap(),
            Plan::Monitors
        );
        assert_eq!(
            plan(VirtualDisplay::Never, true, false).unwrap(),
            Plan::Monitors
        );
        assert_eq!(
            plan(VirtualDisplay::Always, true, false).unwrap(),
            Plan::Virtual
        );
        assert!(plan(VirtualDisplay::Always, false, false).is_err());
        // Privacy streams a virtual output whatever the fallback setting.
        assert_eq!(
            plan(VirtualDisplay::Never, true, true).unwrap(),
            Plan::Virtual
        );
        assert!(plan(VirtualDisplay::Fallback, false, true).is_err());
        assert!(falls_back(MonitorFailure::NoMonitor));
        assert!(falls_back(MonitorFailure::PortalError));
        assert!(!falls_back(MonitorFailure::Cancelled));
        assert!(!falls_back(MonitorFailure::Other));
    }

    #[test]
    fn privacy_switches_capture_only_when_the_source_changes() {
        use VirtualDisplay::*;
        // Monitors → virtual output, and back.
        assert!(privacy_reopens(Fallback, false, false, true));
        assert!(privacy_reopens(Fallback, true, true, false));
        // Already a virtual output (headless fallback, `always`): kept.
        assert!(!privacy_reopens(Fallback, true, false, true));
        assert!(!privacy_reopens(Always, true, false, true));
        assert!(!privacy_reopens(Always, true, true, false));
        assert!(!privacy_reopens(Never, false, true, true));
    }

    #[test]
    fn portal_virtual_outputs_are_told_from_physical_ones() {
        assert!(is_portal_virtual(
            "Virtual-virtual-xdp-kde-dev.teleport.Host"
        ));
        assert!(is_portal_virtual(
            "Virtual-virtual-xdp-kde-dev.teleport.Host-1"
        ));
        // A nested KWin's own outputs count as physical.
        assert!(!is_portal_virtual("Virtual-0"));
        assert!(!is_portal_virtual("DP-1"));
        let outputs = parse_outputs(KSCREEN.as_bytes()).unwrap();
        let physical: Vec<_> = outputs
            .iter()
            .filter(|o| o.is_physical())
            .map(|o| o.name.as_str())
            .collect();
        assert_eq!(physical, ["DP-1", "HDMI-A-1"]);
    }

    #[test]
    fn privacy_disables_enabled_physical_outputs_and_restores_them_as_they_were() {
        let outputs = parse_outputs(KSCREEN.as_bytes()).unwrap();
        let targets = privacy_targets(&outputs);
        assert_eq!(targets.iter().map(|o| o.id).collect::<Vec<_>>(), [1]);
        assert_eq!(disable_args(&targets), ["output.1.disable"]);
        let saved: Vec<_> = targets.iter().map(|o| Saved::of(o)).collect();
        assert_eq!(saved[0].mode.as_deref(), Some("2560x1440@144"));
        // Nothing to restore while it is still on.
        assert!(restore_args(&saved, &outputs).is_empty());
        // Off, and renumbered after a hotplug: matched by name.
        let mut off = outputs.clone();
        off[0].enabled = false;
        off[0].id = 7;
        assert_eq!(
            restore_args(&saved, &off),
            [
                "output.7.enable",
                "output.7.position.0,0",
                "output.7.mode.3",
                "output.7.scale.1.25",
                "output.7.priority.1"
            ]
        );
        // Gone (unplugged): skipped, the compositor enables it on return.
        off[0].connected = false;
        assert!(restore_args(&saved, &off).is_empty());
        // The state file round-trips.
        let json = serde_json::to_string(&saved).unwrap();
        assert_eq!(serde_json::from_str::<Vec<Saved>>(&json).unwrap(), saved);
    }

    #[test]
    fn virtual_outputs_resize_by_mode_id_with_bounded_custom_modes() {
        let outputs = parse_outputs(KSCREEN.as_bytes()).unwrap();
        let virtual_output = &outputs[2];
        let size = |width, height, refresh_mhz| Size {
            width,
            height,
            refresh_mhz,
        };
        // Existing modes (refresh within 1 Hz) are selected, by id.
        assert_eq!(
            select_mode_args(virtual_output, size(2560, 1600, 120_000)),
            Some(vec!["output.5.mode.12".to_owned()])
        );
        assert_eq!(
            select_mode_args(virtual_output, size(1920, 1080, 60_000)),
            Some(vec![])
        );
        assert_eq!(
            select_mode_args(virtual_output, size(2560, 1600, 60_000)),
            None
        );
        assert_eq!(
            add_mode_args(virtual_output, size(3024, 1964, 120_000), 1),
            ["output.5.addCustomMode.3024.1964.120000.full"]
        );
        assert_eq!(
            add_mode_args(virtual_output, size(3024, 1964, 120_000), 4),
            [
                "output.5.removeCustomMode.0",
                "output.5.addCustomMode.3024.1964.120000.full"
            ]
        );
    }

    #[test]
    fn same_named_virtual_outputs_are_never_guessed_between() {
        let outputs = parse_outputs(KSCREEN.as_bytes()).unwrap();
        let name = "Virtual-virtual-xdp-kde-dev.teleport.Host";
        assert_eq!(only_named(outputs.clone(), name).unwrap().id, 5);
        let mut twice = outputs.clone();
        twice.push(outputs[2].clone());
        assert!(only_named(twice, name).is_err());
        assert!(only_named(outputs, "DP-9").is_err());
    }

    #[test]
    fn client_display_sizes_are_clamped() {
        let display = |width, height, refresh_mhz| crate::protocol::ClientDisplay {
            width,
            height,
            refresh_mhz,
            stream_width: None,
            max_fps: None,
            bitrate: None,
        };
        assert_eq!(
            Size::for_client(&display(3025, 1964, 120_000)),
            Size {
                width: 3024,
                height: 1964,
                refresh_mhz: 120_000
            }
        );
        assert_eq!(Size::for_client(&display(0, 0, 0)), Size::DEFAULT);
        assert_eq!(
            Size::for_client(&display(10_000, 9_000, 500_000)),
            Size {
                width: 7680,
                height: 4320,
                refresh_mhz: 240_000
            }
        );
        assert_eq!(
            Size::for_client(&display(1280, 800, 10_000)).refresh_mhz,
            30_000
        );
    }
}
