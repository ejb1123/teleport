#![cfg(target_os = "linux")]
//! Peer-to-peer hole punching between two NATed networks, with the relay
//! used only for signalling. Everything runs in unprivileged user and network
//! namespaces (`unshare -rn`, `nsenter`, iproute2 and iptables with the
//! kernel's NAT and conntrack modules), like `tests/network.rs`:
//!
//! ```text
//!   host 10.1.0.2 ── NAT A ── 198.51.100.2 ┐                ┌ 192.0.2.10 relay
//!                                          ├─ "internet" ───┤  (+ STUN :3478)
//! client 10.2.0.2 ── NAT B ── 203.0.113.2 ┘   5 ms   15 ms  └
//! ```
//!
//! Both NATs are Linux MASQUERADE with a home-router firewall (inbound only
//! for established flows), i.e. endpoint-independent mapping with port
//! preservation and address-and-port-dependent filtering. Without that
//! firewall, an unsolicited packet leaves a conntrack entry on the router that
//! makes it remap the port of a later outgoing flow to the same peer; the
//! host's first punches carry a low TTL so they never reach the client's NAT
//! (covered too). With `--random-fully` on NAT B the client's NAT becomes
//! symmetric (a new public port per destination), which hole punching cannot
//! traverse, so the client must fall back to the relay. Link delays make the
//! relay detour measurable: about 20 ms RTT direct versus 80 ms through the
//! relay.

use std::{collections::BTreeMap, process::Command};

const RELAY_KEY: &str = "p2p-test-relay-key";

struct Scenario {
    name: &'static str,
    /// Extra iptables MASQUERADE options on the client's NAT.
    client_nat: &'static str,
    /// Drop all UDP between the two NATs in the core (only the relay works).
    block_peer_udp: bool,
    /// NATs without the inbound firewall: unsolicited packets reach the
    /// router itself and leave conntrack entries behind.
    open_routers: bool,
    host_args: &'static str,
    client_args: &'static str,
}

struct Outcome {
    stdout: String,
    host_log: String,
    fields: BTreeMap<String, String>,
}

impl Outcome {
    fn field(&self, name: &str) -> u64 {
        self.fields
            .get(name)
            .and_then(|value| value.parse().ok())
            .unwrap_or_else(|| panic!("summary missing {name}: {}", self.stdout))
    }

    /// `elapsed_ms` of the client's "connection path chosen" line.
    fn connect_ms(&self) -> u64 {
        self.stdout
            .lines()
            .find(|line| line.contains("connection path chosen"))
            .and_then(|line| {
                line.split_whitespace()
                    .filter_map(|pair| pair.split_once('='))
                    .find(|(key, _)| *key == "elapsed_ms")
                    .and_then(|(_, value)| value.parse().ok())
            })
            .unwrap_or_else(|| panic!("no connection time: {}", self.stdout))
    }
}

fn run(scenario: &Scenario) -> Outcome {
    let temp = tempfile::tempdir().unwrap();
    let script = format!(
        r#"set -eu
ip link set lo up
sysctl -qw net.ipv4.ip_forward=1
pids=""
cleanup() {{ for pid in $pids; do kill $pid 2>/dev/null || true; done; wait 2>/dev/null || true; }}
trap cleanup EXIT
trap 'exit 130' INT TERM
# A network namespace is a sleeping process; nsenter joins it.
namespace() {{ unshare -n sleep 600 >/dev/null 2>&1 & pids="$pids $!"; eval "$1=$!"; }}
namespace RELAY; namespace NATA; namespace HOST; namespace NATB; namespace CLIENT
sleep 0.2
at() {{ target=$1; shift; nsenter -t "$target" -n "$@"; }}
# link NAME_HERE NS_HERE ADDR_HERE NAME_THERE NS_THERE ADDR_THERE DELAY
link() {{
    ip link add "$1" type veth peer name "$4"
    [ "$2" = core ] || ip link set "$1" netns "$2"
    ip link set "$4" netns "$5"
    if [ "$2" = core ]; then ip addr add "$3" dev "$1"; ip link set "$1" up; [ "$7" = 0 ] || tc qdisc add dev "$1" root netem delay "$7"
    else at "$2" sh -c "ip link set lo up; ip addr add $3 dev $1; ip link set $1 up"; [ "$7" = 0 ] || at "$2" tc qdisc add dev "$1" root netem delay "$7"; fi
    at "$5" sh -c "ip link set lo up; ip addr add $6 dev $4; ip link set $4 up"
    [ "$7" = 0 ] || at "$5" tc qdisc add dev "$4" root netem delay "$7"
}}
link to_relay core 192.0.2.1/24 eth0 $RELAY 192.0.2.10/24 15ms
link to_a core 198.51.100.1/24 wan $NATA 198.51.100.2/24 5ms
link to_b core 203.0.113.1/24 wan $NATB 203.0.113.2/24 5ms
link lan $NATA 10.1.0.1/24 eth0 $HOST 10.1.0.2/24 0
link lan $NATB 10.2.0.1/24 eth0 $CLIENT 10.2.0.2/24 0
at $RELAY ip route add default via 192.0.2.1
at $HOST ip route add default via 10.1.0.1
at $CLIENT ip route add default via 10.2.0.1
# Home routers: masquerade, and accept from the WAN only established flows.
router() {{
    at "$1" ip route add default via "$2"
    at "$1" sysctl -qw net.ipv4.ip_forward=1
    at "$1" iptables -t nat -A POSTROUTING -o wan -j MASQUERADE $3
    [ "$4" = 1 ] || return 0
    for chain in INPUT FORWARD; do
        at "$1" iptables -A $chain -i wan -m conntrack --ctstate ESTABLISHED,RELATED -j ACCEPT
        at "$1" iptables -A $chain -i wan -j DROP
    done
}}
router $NATA 198.51.100.1 "" {firewall}
router $NATB 203.0.113.1 "{client_nat}" {firewall}
if [ {block} = 1 ]; then
    iptables -A FORWARD -p udp -s 198.51.100.0/24 -d 203.0.113.0/24 -j DROP
    iptables -A FORWARD -p udp -s 203.0.113.0/24 -d 198.51.100.0/24 -j DROP
fi
# Backgrounded directly (not through `at`): `$!` must be the teleport
# process itself, which nsenter -n becomes, or cleanup misses it.
nsenter -t $RELAY -n "$TELEPORT" relay --listen 0.0.0.0:4444 --key {key} >"$DIR/relay.log" 2>&1 &
pids="$pids $!"
tries=0
until grep -q "RELAY FINGERPRINT" "$DIR/relay.log"; do
    tries=$((tries + 1)); [ $tries -lt 300 ] || {{ echo "relay not ready" >&2; exit 90; }}
    sleep 0.05
done
FP=$(sed -n 's/^RELAY FINGERPRINT //p' "$DIR/relay.log")
nsenter -t $HOST -n env TELEPORT_TEST_PATTERN=ball "$TELEPORT" host --source test --encoder software \
    --listen 0.0.0.0:4443 --width 640 --fps 30 --bitrate 2000 --identity-dir "$DIR/identity" \
    --relay moqt://192.0.2.10:4444 --relay-fingerprint "$FP" --relay-key {key} {host_args} \
    >"$DIR/host.log" 2>&1 &
pids="$pids $!"
tries=0
until grep -q "connected to relay" "$DIR/host.log"; do
    tries=$((tries + 1)); [ $tries -lt 400 ] || {{ echo "host not on relay" >&2; cat "$DIR/host.log" >&2; exit 91; }}
    sleep 0.05
done
# The host's own address is private to NAT A: unreachable from the client.
at $CLIENT "$TELEPORT" client 10.1.0.2:4443 --pairing-file "$DIR/identity/pairing.json" \
    --route auto --relay moqt://192.0.2.10:4444 --relay-fingerprint "$FP" --relay-key {key} \
    --headless-frames 150 --software-decoder --smoke-input-latency {client_args}
sleep 0.3
"#,
        client_nat = scenario.client_nat,
        firewall = u8::from(!scenario.open_routers),
        block = u8::from(scenario.block_peer_udp),
        key = RELAY_KEY,
        host_args = scenario.host_args,
        client_args = scenario.client_args,
    );
    let output = Command::new("unshare")
        .args(["-rn", "sh", "-c", &script])
        .env("TELEPORT", teleport_binary())
        .env("DIR", temp.path())
        .env_remove("TELEPORT_RELAY_URL")
        .env_remove("TELEPORT_RELAY_KEY")
        .output()
        .expect("unshare is required for the peer-to-peer namespace test");
    let stdout = String::from_utf8_lossy(&output.stdout).into_owned();
    let host_log = std::fs::read_to_string(temp.path().join("host.log")).unwrap_or_default();
    assert!(
        output.status.success() && stdout.contains("SMOKE PASS"),
        "[{}] session failed ({})\nclient stdout:\n{stdout}\nclient stderr:\n{}\nhost:\n{host_log}\nrelay:\n{}",
        scenario.name,
        output.status,
        String::from_utf8_lossy(&output.stderr),
        std::fs::read_to_string(temp.path().join("relay.log")).unwrap_or_default(),
    );
    let fields = stdout
        .lines()
        .find(|line| line.contains("SMOKE PASS"))
        .unwrap_or_default()
        .split_whitespace()
        .filter_map(|pair| pair.split_once('='))
        .map(|(key, value)| (key.to_owned(), value.trim_matches('"').to_owned()))
        .collect();
    let outcome = Outcome {
        stdout,
        host_log,
        fields,
    };
    for line in outcome.stdout.lines().chain(outcome.host_log.lines()) {
        if ["peer-to-peer", "path chosen", "signalling"]
            .iter()
            .any(|needle| line.contains(needle))
        {
            println!("  {line}");
        }
    }
    println!(
        "P2P {:<14} connect_ms={} probes={} probe_p50_ms={:.1} probe_p95_ms={:.1}",
        scenario.name,
        outcome.connect_ms(),
        outcome.field("probes"),
        outcome.field("probe_p50_us") as f64 / 1000.0,
        outcome.field("probe_p95_us") as f64 / 1000.0,
    );
    outcome
}

fn teleport_binary() -> std::ffi::OsString {
    std::env::var_os("TELEPORT_TEST_BINARY")
        .unwrap_or_else(|| env!("CARGO_BIN_EXE_teleport").into())
}

#[test]
#[ignore = "requires unprivileged user/network namespaces, iproute2, iptables (NAT, conntrack) and GStreamer plugins"]
fn hole_punching_connects_two_nated_peers_directly() {
    for (name, open_routers) in [("p2p", false), ("p2p_open_nat", true)] {
        let outcome = run(&Scenario {
            name,
            client_nat: "",
            block_peer_udp: false,
            open_routers,
            host_args: "",
            client_args: "",
        });
        assert!(
            outcome
                .stdout
                .contains("connection path chosen: p2p, server-reflexive candidate"),
            "[{name}] {}",
            outcome.stdout
        );
        // The relay carried only signalling: no relayed session was set up.
        assert!(
            !outcome.stdout.contains("relay session established"),
            "[{name}] {}",
            outcome.stdout
        );
        // The host saw the client's NAT B address: the packets crossed both
        // NATs.
        assert!(
            outcome
                .host_log
                .contains("peer-to-peer connection; checking its credential remote=203.0.113.2:"),
            "[{name}] {}",
            outcome.host_log
        );
        // About 20 ms RTT direct (80 ms via the relay); probes add little.
        assert!(
            outcome.field("probe_p50_us") < 60_000,
            "[{name}] {}",
            outcome.stdout
        );
    }
}

#[test]
#[ignore = "requires unprivileged user/network namespaces, iproute2, iptables (NAT, conntrack) and GStreamer plugins"]
fn symmetric_nat_falls_back_to_the_relay() {
    let outcome = run(&Scenario {
        name: "symmetric",
        client_nat: "--random-fully",
        block_peer_udp: false,
        open_routers: false,
        host_args: "",
        client_args: "",
    });
    // Signalling worked, the punch could not.
    assert!(
        outcome
            .stdout
            .contains("peer-to-peer candidates exchanged through the relay"),
        "{}",
        outcome.stdout
    );
    assert!(
        outcome.stdout.contains("peer-to-peer path unavailable"),
        "{}",
        outcome.stdout
    );
    assert!(
        outcome.stdout.contains("connection path chosen: relay"),
        "{}",
        outcome.stdout
    );
    assert!(
        outcome
            .stdout
            .contains("end-to-end encrypted relay session established")
    );
    assert!(
        outcome.field("probe_p50_us") >= 75_000,
        "{}",
        outcome.stdout
    );
}

#[test]
#[ignore = "requires unprivileged user/network namespaces, iproute2, iptables (NAT, conntrack) and GStreamer plugins"]
fn blocked_peer_udp_and_old_hosts_use_the_relay() {
    let blocked = run(&Scenario {
        name: "blocked",
        client_nat: "",
        block_peer_udp: true,
        open_routers: false,
        host_args: "",
        client_args: "",
    });
    assert!(
        blocked.stdout.contains("connection path chosen: relay"),
        "{}",
        blocked.stdout
    );
    // A host without peer-to-peer (as an older host) never answers the
    // signalling hello; the client goes to the relay as before.
    let old_host = run(&Scenario {
        name: "host_no_p2p",
        client_nat: "",
        block_peer_udp: false,
        open_routers: false,
        host_args: "--no-p2p",
        client_args: "",
    });
    assert!(
        old_host.stdout.contains("connection path chosen: relay"),
        "{}",
        old_host.stdout
    );
    assert!(
        !old_host.host_log.contains("peer-to-peer"),
        "{}",
        old_host.host_log
    );
    // A client with --no-p2p behaves exactly as before (direct, then relay).
    let old_client = run(&Scenario {
        name: "client_no_p2p",
        client_nat: "",
        block_peer_udp: false,
        open_routers: false,
        host_args: "",
        client_args: "--no-p2p",
    });
    assert!(
        old_client.stdout.contains("connection path chosen: relay"),
        "{}",
        old_client.stdout
    );
    assert!(
        !old_client.stdout.contains("peer-to-peer"),
        "{}",
        old_client.stdout
    );
}
