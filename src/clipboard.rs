//! Explicit, bounded text transfers only; never continuously scrape the clipboard.
use crate::protocol::{Event, MAX_TEXT};
use anyhow::{Context, Result, ensure};
use std::{process::Stdio, time::Duration};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    process::Command,
};

fn command(write: bool) -> Command {
    let wayland = std::env::var_os("WAYLAND_DISPLAY").is_some();
    let mut command = if wayland {
        let mut c = Command::new(if write { "wl-copy" } else { "wl-paste" });
        if write {
            c.args(["--type", "text/plain;charset=utf-8"]);
        } else {
            c.args(["--no-newline", "--type", "text"]);
        }
        c
    } else {
        let mut c = Command::new("xclip");
        c.args([
            "-selection",
            "clipboard",
            if write { "-in" } else { "-out" },
        ]);
        c
    };
    command.kill_on_drop(true).stderr(Stdio::null());
    command
}

pub async fn read() -> Result<String> {
    tokio::time::timeout(Duration::from_secs(2), async {
        let mut child = command(false)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .spawn()
            .context("clipboard helper unavailable (install wl-clipboard or xclip)")?;
        let mut bytes = Vec::new();
        child
            .stdout
            .take()
            .unwrap()
            .take(MAX_TEXT as u64 + 1)
            .read_to_end(&mut bytes)
            .await?;
        ensure!(bytes.len() <= MAX_TEXT, "clipboard exceeds 64 KiB");
        ensure!(child.wait().await?.success(), "clipboard read failed");
        let text = String::from_utf8(bytes)?;
        Event::Clipboard { text: text.clone() }.validate()?;
        Ok::<_, anyhow::Error>(text)
    })
    .await
    .context("clipboard read timed out")?
}

pub async fn write(text: &str) -> Result<()> {
    Event::Clipboard { text: text.into() }.validate()?;
    tokio::time::timeout(Duration::from_secs(2), async {
        let mut child = command(true)
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .spawn()
            .context("clipboard helper unavailable (install wl-clipboard or xclip)")?;
        let mut input = child.stdin.take().unwrap();
        input.write_all(text.as_bytes()).await?;
        drop(input);
        ensure!(child.wait().await?.success(), "clipboard write failed");
        Ok::<_, anyhow::Error>(())
    })
    .await
    .context("clipboard write timed out")?
}
