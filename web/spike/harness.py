"""Spike harness: a real relay (and optionally a host), a localhost page and
headless Chromium. Each check prints PASS/FAIL with the evidence."""
import contextlib, functools, http.server, os, re, socket, subprocess, sys, threading, time

TELEPORT = os.environ["TELEPORT_BIN"]
HERE = os.path.dirname(os.path.abspath(__file__))
os.environ.setdefault("PLAYWRIGHT_BROWSERS_PATH", os.environ.get("BROWSERS", ""))

def free_udp():
    s = socket.socket(socket.AF_INET, socket.SOCK_DGRAM); s.bind(("127.0.0.1", 0))
    port = s.getsockname()[1]; s.close(); return port

@contextlib.contextmanager
def relay(key="spike-key"):
    port = free_udp()
    p = subprocess.Popen([TELEPORT, "relay", "--listen", f"127.0.0.1:{port}", "--key", key,
                          "--stun-listen", "off"], stdout=subprocess.PIPE, stderr=open(os.environ.get("RELAY_LOG", os.devnull), "w"), text=True, env={**os.environ, "RUST_LOG": os.environ.get("RELAY_RUST_LOG", "info")})
    fp = None
    for line in p.stdout:
        m = re.match(r"RELAY FINGERPRINT ([0-9a-f]{64})", line)
        if m: fp = m.group(1); break
    log = open(os.environ.get("RELAY_LOG", os.devnull), "a")
    def drain():
        for line in p.stdout: log.write(line); log.flush()
    threading.Thread(target=drain, daemon=True).start()
    try: yield port, fp, key
    finally: p.kill(); p.wait()

@contextlib.contextmanager
def page_server():
    handler = functools.partial(http.server.SimpleHTTPRequestHandler, directory=HERE)
    class Quiet(handler.func):
        def log_message(self, *a): pass
    srv = http.server.ThreadingHTTPServer(("127.0.0.1", 0), functools.partial(Quiet, directory=HERE))
    threading.Thread(target=srv.serve_forever, daemon=True).start()
    try: yield f"http://localhost:{srv.server_address[1]}"
    finally: srv.shutdown()

@contextlib.contextmanager
def browser_page(base, page_name):
    from playwright.sync_api import sync_playwright
    with sync_playwright() as pw:
        b = getattr(pw, os.environ.get("BROWSER", "chromium")).launch(headless=True)
        page = b.new_page()
        logs = []
        page.on("console", lambda m: logs.append(m.text))
        page.goto(f"{base}/{page_name}")
        try: yield page, logs
        finally: b.close()

@contextlib.contextmanager
def host(relay_port, relay_fp, key, extra=()):
    """A real `teleport host --source test` connected to the relay; yields
    (namespace, fingerprint, events path) once it reports relay_connected."""
    import json, tempfile
    tmp = tempfile.mkdtemp(prefix="web-spike-host-")
    events = os.path.join(tmp, "events.jsonl")
    log = open(os.path.join(tmp, "host.log"), "w")
    p = subprocess.Popen([TELEPORT, "host", "--source", "test", "--encoder", "software",
                          "--listen", f"127.0.0.1:{free_udp()}", "--width", "640", "--no-p2p",
                          "--identity-dir", os.path.join(tmp, "identity"),
                          "--relay", f"moqt://127.0.0.1:{relay_port}", "--relay-fingerprint", relay_fp,
                          "--relay-key", key, "--events-file", events, *extra],
                         stdout=log, stderr=log)
    deadline = time.time() + 30
    ns = fp = None
    while time.time() < deadline and not ns:
        time.sleep(0.2)
        if os.path.exists(events):
            for line in open(events):
                e = json.loads(line)
                if e["event"] == "ready": fp = e["fingerprint"]
                if e["event"] == "relay_connected": ns = e["namespace"]
    try:
        if not ns: raise RuntimeError("host never connected to the relay: " + open(os.path.join(tmp, "host.log")).read()[-2000:])
        yield ns, fp, events, tmp
    finally:
        p.kill(); p.wait()
