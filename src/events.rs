//! Host events for a supervisor (`teleport host --events-file`): one JSON
//! object per line, appended and flushed as each happens, so a service that
//! starts the host (a kiosk's controller, a support agent) can follow and
//! audit sessions without the admin socket, which only the host's own user
//! may use (docs/supervision.md).
//!
//! Every line has `at` (unix milliseconds) and `event`; the other fields
//! depend on the event. Writing is best effort: a full disk must not stop a
//! session, so failures are logged once and the host carries on.
use serde_json::{Map, Value, json};
use std::{
    io::Write,
    path::Path,
    sync::{
        Mutex, OnceLock,
        atomic::{AtomicBool, Ordering},
    },
};

static SINK: OnceLock<Mutex<std::fs::File>> = OnceLock::new();
static FAILED: AtomicBool = AtomicBool::new(false);

/// Start writing events to `path` (created 0640 if missing, appended).
pub fn open(path: &Path) -> anyhow::Result<()> {
    use std::os::unix::fs::OpenOptionsExt;
    let file = std::fs::OpenOptions::new()
        .append(true)
        .create(true)
        .mode(0o640)
        .open(path)?;
    SINK.set(Mutex::new(file))
        .map_err(|_| anyhow::anyhow!("events file already open"))
}

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or_default()
}

/// The line written for `event` with `fields` (an object).
fn line(event: &str, fields: Value, at: u64) -> String {
    let mut object = Map::new();
    object.insert("at".into(), json!(at));
    object.insert("event".into(), json!(event));
    if let Value::Object(fields) = fields {
        for (key, value) in fields {
            object.entry(key).or_insert(value);
        }
    }
    let mut text = Value::Object(object).to_string();
    text.push('\n');
    text
}

/// Record `event`; a no-op without `--events-file`.
pub fn emit(event: &str, fields: Value) {
    let Some(sink) = SINK.get() else {
        return;
    };
    let text = line(event, fields, now_ms());
    let written = sink
        .lock()
        .map_err(|_| std::io::Error::other("events file lock poisoned"))
        .and_then(|mut file| {
            file.write_all(text.as_bytes())?;
            file.flush()
        });
    if let Err(error) = written
        && !FAILED.swap(true, Ordering::Relaxed)
    {
        tracing::warn!(%error, "could not write the events file; continuing without it");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lines_are_single_json_objects_and_keep_at_and_event() {
        let text = line(
            "session_left",
            json!({"session": 3, "reason": "a\nb", "event": "spoofed", "at": 1}),
            42,
        );
        assert!(text.ends_with('\n') && text.matches('\n').count() == 1);
        let value: Value = serde_json::from_str(text.trim_end()).unwrap();
        assert_eq!(value["at"], 42);
        assert_eq!(value["event"], "session_left");
        assert_eq!(value["session"], 3);
        assert_eq!(value["reason"], "a\nb");
    }
}
