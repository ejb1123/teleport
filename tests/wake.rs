#![cfg(target_os = "linux")]
//! Wake-on-LAN and owner power requests (G23) in a private, unprivileged
//! network namespace: a dummy "LAN" interface, `teleport wake-listen` as the
//! sleeping machine's network card, and (for the wake relay) a real relay
//! and host. Magic packets never leave the namespace, so nothing on the real
//! LAN is woken. Needs `unshare -rnm` and `ip` (iproute2, in the dev shell).

use std::process::{Command, Output};

const MAC: &str = "00:1b:21:3c:4d:5e";

fn teleport_binary() -> std::ffi::OsString {
    std::env::var_os("TELEPORT_TEST_BINARY")
        .unwrap_or_else(|| env!("CARGO_BIN_EXE_teleport").into())
}

/// Run `body` in a new user, network and mount namespace with `lan0`
/// (10.9.0.1/24, the default route) and a listener on UDP 9 that expects
/// `packets` magic packets (its output in `$DIR/listen.log`).
fn in_namespace(body: &str, packets: u32) -> (Output, String, tempfile::TempDir) {
    let temp = tempfile::tempdir().unwrap();
    let script = format!(
        r#"set -eu
# This namespace's interfaces in /sys (the host reads its MAC there).
mount -t sysfs none /sys
ip link set lo up
ip link add lan0 type dummy
ip addr add 10.9.0.1/24 dev lan0
ip link set lan0 up
ip route add default dev lan0
"$TELEPORT" wake-listen --port 9 --count {packets} --timeout 30 >"$DIR/listen.log" 2>&1 &
listener=$!
trap 'kill $listener 2>/dev/null || true' EXIT
tries=0
until grep -q LISTENING "$DIR/listen.log" 2>/dev/null; do
    tries=$((tries + 1)); [ $tries -lt 200 ] || {{ echo "listener not ready" >&2; exit 90; }}
    sleep 0.05
done
{body}
wait $listener
"#
    );
    let output = Command::new("unshare")
        .args(["-rnm", "sh", "-c", &script])
        .env("TELEPORT", teleport_binary())
        .env("DIR", temp.path())
        .env_remove("TELEPORT_RELAY_KEY")
        .output()
        .expect("unshare is required for wake tests");
    let listened = std::fs::read_to_string(temp.path().join("listen.log")).unwrap_or_default();
    (output, listened, temp)
}

fn describe(output: &Output, listened: &str, temp: &tempfile::TempDir) -> String {
    format!(
        "status {}\nstdout:\n{}\nstderr:\n{}\nlistener:\n{listened}\nhost:\n{}",
        output.status,
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
        std::fs::read_to_string(temp.path().join("host.log")).unwrap_or_default()
    )
}

#[test]
#[ignore = "requires unprivileged user/network/mount namespaces and iproute2"]
fn magic_packets_reach_the_lan_from_the_client() {
    // The limited broadcast and lan0's subnet broadcast (10.9.0.255),
    // given again explicitly: still sent once.
    let (output, listened, temp) = in_namespace(
        &format!(r#""$TELEPORT" wake --mac {MAC} --broadcast 10.9.0.255"#),
        2,
    );
    let detail = describe(&output, &listened, &temp);
    assert!(output.status.success(), "{detail}");
    assert!(
        String::from_utf8_lossy(&output.stdout).contains("to 2 address(es)"),
        "{detail}"
    );
    assert_eq!(
        listened
            .lines()
            .filter(|line| line.starts_with(&format!("WAKE {MAC} from")))
            .count(),
        2,
        "{detail}"
    );
    assert!(!listened.contains("IGNORED"), "{detail}");
}

/// A relay, and a host on lan0 acting as wake relay that suspends on
/// request (a fake suspend command), all inside the namespace.
const HOST: &str = r#"
"$TELEPORT" relay --listen 127.0.0.1:4444 --key relay-key >"$DIR/relay.log" 2>&1 &
relay=$!
tries=0
until grep -q "RELAY FINGERPRINT" "$DIR/relay.log" 2>/dev/null; do
    tries=$((tries + 1)); [ $tries -lt 200 ] || { echo "relay not ready" >&2; exit 91; }
    sleep 0.05
done
pin=$(sed -n 's/^RELAY FINGERPRINT //p' "$DIR/relay.log")
"$TELEPORT" host --source test --encoder software --listen 127.0.0.1:4443 --width 640 \
    --identity-dir "$DIR/id" --p2p-listen 127.0.0.1:0 \
    --relay moqt://127.0.0.1:4444 --relay-fingerprint "$pin" --relay-key relay-key \
    --wake-relay --allow-suspend --suspend-command "touch $DIR/suspended" \
    >"$DIR/host.log" 2>&1 &
host=$!
trap 'kill $listener $host $relay 2>/dev/null || true' EXIT
tries=0
until grep -q "connected to relay" "$DIR/host.log" 2>/dev/null; do
    tries=$((tries + 1)); [ $tries -lt 400 ] || { echo "host not ready" >&2; exit 92; }
    sleep 0.05
done
relay_args="--relay moqt://127.0.0.1:4444 --relay-fingerprint $pin --relay-key relay-key"
"#;

#[test]
#[ignore = "requires unprivileged user/network/mount namespaces, iproute2 and GStreamer plugins"]
fn wake_relay_and_suspend_through_the_end_to_end_relay() {
    let body = format!(
        r#"{HOST}
# Through the relay only: the direct address is unreachable.
"$TELEPORT" wake --mac {MAC} --no-local --via 127.0.0.1:9 \
    --via-pairing-file "$DIR/id/pairing.json" $relay_args --route relay
echo WAKE-RELAYED
# Owner suspend, through the relay and directly.
"$TELEPORT" suspend 127.0.0.1:9 --pairing-file "$DIR/id/pairing.json" $relay_args --route relay
sleep 3
[ -e "$DIR/suspended" ] && echo SUSPENDED-RELAYED
rm -f "$DIR/suspended"
"$TELEPORT" suspend 127.0.0.1:4443 --pairing-file "$DIR/id/pairing.json"
sleep 3
[ -e "$DIR/suspended" ] && echo SUSPENDED-DIRECT
# A credential the host does not know gets nothing.
sed 's/"token": *"[0-9a-f]*"/"token":"{token}"/' "$DIR/id/pairing.json" >"$DIR/stranger.json"
chmod 600 "$DIR/stranger.json"
if "$TELEPORT" suspend 127.0.0.1:4443 --pairing-file "$DIR/stranger.json"; then
    echo STRANGER-ACCEPTED
fi
"#,
        token = "0".repeat(64)
    );
    // The host sends the limited broadcast and lan0's subnet broadcast.
    let (output, listened, temp) = in_namespace(&body, 2);
    let detail = describe(&output, &listened, &temp);
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(output.status.success(), "{detail}");
    assert!(
        stdout.contains(&format!(
            "Wake relay 127.0.0.1:9: sent a magic packet for {MAC}"
        )),
        "{detail}"
    );
    assert!(stdout.contains("WAKE-RELAYED"), "{detail}");
    assert_eq!(
        listened
            .lines()
            .filter(|line| line.starts_with(&format!("WAKE {MAC} from 10.9.0.1:")))
            .count(),
        2,
        "{detail}"
    );
    assert!(stdout.contains("suspending in 2 s"), "{detail}");
    assert!(stdout.contains("SUSPENDED-RELAYED"), "{detail}");
    assert!(stdout.contains("SUSPENDED-DIRECT"), "{detail}");
    assert!(!stdout.contains("STRANGER-ACCEPTED"), "{detail}");
    let host = std::fs::read_to_string(temp.path().join("host.log")).unwrap_or_default();
    // The host reported its LAN card, and never captured for a command.
    assert!(
        host.contains("mac=") && host.contains("network=\"10.9.0.1/24\""),
        "{detail}"
    );
    assert!(host.contains("relayed host command"), "{detail}");
    assert!(!host.contains("client disconnected"), "{detail}");
}

#[test]
#[ignore = "requires unprivileged user/network/mount namespaces, iproute2 and GStreamer plugins"]
fn hosts_refuse_power_requests_they_were_not_given() {
    let body = format!(
        r#"
"$TELEPORT" host --source test --encoder software --listen 127.0.0.1:4443 --width 640 \
    --identity-dir "$DIR/id" >"$DIR/host.log" 2>&1 &
host=$!
trap 'kill $listener $host 2>/dev/null || true' EXIT
tries=0
until [ -s "$DIR/id/pairing.json" ]; do
    tries=$((tries + 1)); [ $tries -lt 400 ] || {{ echo "host not ready" >&2; exit 92; }}
    sleep 0.05
done
sleep 0.5
"$TELEPORT" suspend 127.0.0.1:4443 --pairing-file "$DIR/id/pairing.json" && echo SUSPEND-ACCEPTED
"$TELEPORT" wake --mac {MAC} --no-local --via 127.0.0.1:4443 \
    --via-pairing-file "$DIR/id/pairing.json" && echo WAKE-ACCEPTED
# Let the listener finish: this machine's own packet.
"$TELEPORT" wake --mac {MAC} >/dev/null
"#
    );
    let (output, listened, temp) = in_namespace(&body, 2);
    let detail = describe(&output, &listened, &temp);
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(output.status.success(), "{detail}");
    assert!(stdout.contains("does not suspend on request"), "{detail}");
    assert!(stdout.contains("is not a wake relay"), "{detail}");
    assert!(
        !stdout.contains("SUSPEND-ACCEPTED") && !stdout.contains("WAKE-ACCEPTED"),
        "{detail}"
    );
    // The host sent nothing; only the client's own packets arrived.
    let host = std::fs::read_to_string(temp.path().join("host.log")).unwrap_or_default();
    assert!(!host.contains("as wake relay"), "{detail}");
    assert!(host.contains("host command"), "{detail}");
}
