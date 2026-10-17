//! The host's virtual microphone (G20): a PipeWire source named "Teleport
//! Microphone" that exists while a client sends its microphone.
//!
//! A null sink (the driver) receives the client's audio from a normal
//! pulsesink stream; a loopback turns its monitor into an `Audio/Source`
//! that applications such as Discord list as a microphone. A `pw-cli` child
//! owns both objects (neither lingers), so they disappear when it exits:
//! - at session end, when [`VirtualMic`] is dropped (the child is killed);
//! - when the host exits or crashes, through `PR_SET_PDEATHSIG` on the child;
//! - and should a helper still be orphaned, [`cleanup_stale`] at host start
//!   removes objects whose host process is gone.
//!
//! Both nodes have the lowest session priority, so PipeWire never makes
//! them a default device while a real one exists.
use anyhow::{Context, Result, bail, ensure};
use std::{
    io::Write,
    os::unix::process::CommandExt,
    process::{Command, Stdio},
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicU32, Ordering},
        mpsc,
    },
    time::{Duration, Instant},
};

/// Node names start with this; `teleport.host.pid` names the owning host.
const PREFIX: &str = "teleport_microphone";
/// What applications show for the source.
pub const DESCRIPTION: &str = "Teleport Microphone";
const CREATE_TIMEOUT: Duration = Duration::from_secs(5);

/// `pw-cli` is the only tool needed (PipeWire); without it the host does not
/// offer a microphone.
pub fn available() -> bool {
    Command::new("pw-cli")
        .arg("--version")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|status| status.success())
}

pub struct VirtualMic {
    /// The null sink the client's audio is played into.
    sink: String,
    /// The source applications record.
    source: String,
    /// Dropping it stops the owner thread, which kills `pw-cli`.
    stop: Option<mpsc::Sender<()>>,
    thread: Option<std::thread::JoinHandle<()>>,
    alive: Arc<AtomicBool>,
}

impl VirtualMic {
    /// Create the sink and source and wait until both exist.
    pub fn create() -> Result<Self> {
        static SESSION: AtomicU32 = AtomicU32::new(0);
        let host = std::process::id();
        let tag = format!("{host}.{}", SESSION.fetch_add(1, Ordering::Relaxed) + 1);
        let sink = format!("{PREFIX}_sink.{tag}");
        let source = format!("{PREFIX}.{tag}");
        let commands = commands(&sink, &source, host);
        let (ready, started) = mpsc::channel();
        let (stop, stopped) = mpsc::channel::<()>();
        let alive = Arc::new(AtomicBool::new(true));
        let running = alive.clone();
        // The owner thread lives exactly as long as the virtual microphone:
        // PR_SET_PDEATHSIG fires when the *thread* that forked the child exits.
        let thread = std::thread::Builder::new()
            .name("virtual-mic".into())
            .spawn(move || {
                let parent = std::process::id() as libc::pid_t;
                let mut command = Command::new("pw-cli");
                command
                    .stdin(Stdio::piped())
                    .stdout(Stdio::null())
                    .stderr(Stdio::null());
                // SAFETY: only async-signal-safe calls between fork and exec.
                unsafe {
                    command.pre_exec(move || {
                        if libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGKILL) != 0
                            || libc::getppid() != parent
                        {
                            return Err(std::io::Error::last_os_error());
                        }
                        Ok(())
                    });
                }
                let mut child = match command.spawn() {
                    Ok(child) => child,
                    Err(error) => {
                        running.store(false, Ordering::Relaxed);
                        let _ =
                            ready.send(Err(anyhow::Error::from(error).context("starting pw-cli")));
                        return;
                    }
                };
                // pw-cli keeps running (and owning the objects) after its
                // input ends, so stdin may close here.
                let written = child
                    .stdin
                    .take()
                    .context("pw-cli input")
                    .and_then(|mut input| Ok(input.write_all(commands.as_bytes())?));
                let failed = written.is_err();
                let _ = ready.send(written);
                // Until dropped (or the sender is gone), or pw-cli died.
                while !failed
                    && let Err(mpsc::RecvTimeoutError::Timeout) =
                        stopped.recv_timeout(Duration::from_millis(250))
                    && matches!(child.try_wait(), Ok(None))
                {}
                running.store(false, Ordering::Relaxed);
                let _ = child.kill();
                let _ = child.wait();
            })?;
        let mut mic = Self {
            sink,
            source,
            stop: Some(stop),
            thread: Some(thread),
            alive,
        };
        started
            .recv()
            .context("virtual microphone helper stopped")??;
        let deadline = Instant::now() + CREATE_TIMEOUT;
        loop {
            let nodes = nodes()?;
            let exists = |name: &str| nodes.iter().any(|node| node.name == name);
            if exists(&mic.sink) && exists(&mic.source) {
                break;
            }
            ensure!(
                mic.alive(),
                "pw-cli exited before creating the virtual microphone"
            );
            if Instant::now() >= deadline {
                mic.stop();
                bail!("PipeWire did not create the virtual microphone");
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        tracing::info!(source = %mic.source, "virtual microphone \"{DESCRIPTION}\" created");
        Ok(mic)
    }

    /// The sink to play the client's microphone into.
    pub fn sink(&self) -> &str {
        &self.sink
    }

    /// The node name applications record from.
    pub fn source(&self) -> &str {
        &self.source
    }

    /// False once its helper exited: the playback stream must stop before
    /// PipeWire could move it elsewhere.
    pub fn alive(&self) -> bool {
        self.alive.load(Ordering::Relaxed)
    }

    fn stop(&mut self) {
        self.stop.take();
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

impl Drop for VirtualMic {
    fn drop(&mut self) {
        self.stop();
        tracing::info!(source = %self.source, "virtual microphone removed");
    }
}

/// `pw-cli` commands creating the null sink and the loopback source.
fn commands(sink: &str, source: &str, host: u32) -> String {
    let owner = format!("teleport.host.pid={host}");
    format!(
        "create-node adapter {{ factory.name=support.null-audio-sink node.name={sink} \
         node.description=\"Teleport microphone feed\" media.class=Audio/Sink \
         audio.position=[FL FR] node.virtual=true priority.session=1 priority.driver=1 {owner} }}\n\
         load-module libpipewire-module-loopback {{ audio.position=[FL FR] \
         node.description=\"{DESCRIPTION}\" \
         capture.props={{ node.name={PREFIX}_capture.{tag} stream.capture.sink=true \
         target.object={sink} node.dont-fallback=true node.dont-reconnect=true node.passive=true {owner} }} \
         playback.props={{ media.class=Audio/Source node.name={source} \
         node.description=\"{DESCRIPTION}\" priority.session=1 priority.driver=1 {owner} }} }}\n",
        tag = source.trim_start_matches(PREFIX).trim_start_matches('.'),
    )
}

#[derive(Debug, Default, PartialEq)]
struct Node {
    id: u32,
    name: String,
    host: Option<u32>,
    process: Option<u32>,
}

fn nodes() -> Result<Vec<Node>> {
    let output = Command::new("pw-cli")
        .args(["ls", "Node"])
        .stderr(Stdio::null())
        .output()
        .context("listing PipeWire nodes")?;
    ensure!(output.status.success(), "pw-cli could not list nodes");
    Ok(parse_nodes(&String::from_utf8_lossy(&output.stdout)))
}

/// `pw-cli ls Node`: `id N, type ...` lines, each followed by `key = "value"`
/// properties.
fn parse_nodes(listing: &str) -> Vec<Node> {
    let mut nodes: Vec<Node> = Vec::new();
    for line in listing.lines().map(str::trim) {
        if let Some(rest) = line.strip_prefix("id ") {
            if let Some(id) = rest.split(',').next().and_then(|id| id.parse().ok()) {
                nodes.push(Node {
                    id,
                    ..Node::default()
                });
            }
            continue;
        }
        let (Some(node), Some((key, value))) = (nodes.last_mut(), line.split_once(" = ")) else {
            continue;
        };
        let value = value.trim_matches('"');
        match key {
            "node.name" => node.name = value.to_owned(),
            "teleport.host.pid" => node.host = value.parse().ok(),
            "application.process.id" => node.process = value.parse().ok(),
            _ => (),
        }
    }
    nodes
}

/// Whether `pid` is a running process whose name contains `name`.
fn running(pid: u32, name: &str) -> bool {
    std::fs::read_to_string(format!("/proc/{pid}/comm")).is_ok_and(|comm| comm.contains(name))
}

/// Remove virtual microphones left by a host that is no longer running: end
/// an orphaned `pw-cli` helper (taking its objects with it), or destroy
/// leftover nodes. Objects of running hosts are never touched.
pub fn cleanup_stale() {
    let Ok(nodes) = nodes() else {
        return;
    };
    let mut helpers = Vec::new();
    for node in nodes.iter().filter(|node| node.name.starts_with(PREFIX)) {
        if node.host.is_some_and(|host| running(host, "teleport")) {
            continue;
        }
        match node.process {
            Some(process) if running(process, "pw-cli") => {
                if !helpers.contains(&process) {
                    helpers.push(process);
                }
            }
            _ => {
                let _ = Command::new("pw-cli")
                    .args(["destroy", &node.id.to_string()])
                    .stdout(Stdio::null())
                    .stderr(Stdio::null())
                    .status();
            }
        }
        tracing::warn!(node = %node.name, "removed a virtual microphone left by an earlier host");
    }
    for process in helpers {
        // SAFETY: plain signal to a pw-cli process that owns stale objects.
        unsafe {
            libc::kill(process as libc::pid_t, libc::SIGTERM);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lists_teleport_nodes_with_their_owners() {
        let listing = r#"	id 31, type PipeWire:Interface:Node/3
 		object.serial = "31"
 		node.name = "alsa_output.pci"
	id 205, type PipeWire:Interface:Node/3
 		application.process.id = "4242"
 		node.name = "teleport_microphone.77.1"
 		teleport.host.pid = "77"
"#;
        assert_eq!(
            parse_nodes(listing),
            vec![
                Node {
                    id: 31,
                    name: "alsa_output.pci".into(),
                    host: None,
                    process: None
                },
                Node {
                    id: 205,
                    name: "teleport_microphone.77.1".into(),
                    host: Some(77),
                    process: Some(4242)
                },
            ]
        );
    }

    #[test]
    fn helper_commands_name_both_nodes_and_their_owner() {
        let text = commands("teleport_microphone_sink.9.2", "teleport_microphone.9.2", 9);
        assert_eq!(text.lines().count(), 2);
        assert!(text.contains("node.name=teleport_microphone_sink.9.2"));
        assert!(text.contains("target.object=teleport_microphone_sink.9.2"));
        assert!(text.contains("node.name=teleport_microphone_capture.9.2"));
        assert!(text.contains("media.class=Audio/Source node.name=teleport_microphone.9.2"));
        assert_eq!(text.matches("teleport.host.pid=9").count(), 3);
        // Never a default device while a real one exists.
        assert!(!text.contains("priority.session=1000"));
    }
}
