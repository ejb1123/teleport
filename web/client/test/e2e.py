"""@teleport/web end to end, in headless Chromium: a real `teleport relay`,
a real `teleport host --source test` that trusts a ticket issuer, and a
loopback "issuer site" that serves the page and mints a ticket for the key
the browser posts, like a TRaX account or admin site would.

Checks, each against a control:
  1. a session from a fresh redeem URL decodes frames onto the canvas, and
     the canvas is not one flat colour;
  2. the same URL again is refused (the site spends it on first use);
  3. a ticket from an issuer the host does not trust ends in an error state
     and never decodes a frame.
Run: bash web/client/test/run.sh
"""
import contextlib, functools, http.server, json, os, subprocess, sys, tempfile, threading
sys.path.insert(0, os.path.join(os.path.dirname(os.path.abspath(__file__)), "../../spike"))
from harness import TELEPORT, relay, host, browser_page

ROOT = os.path.abspath(os.path.join(os.path.dirname(__file__), ".."))

def keygen(kind, path):
    return subprocess.run([TELEPORT, "ticket", kind, path], check=True, capture_output=True, text=True).stdout.strip()

def sign(issuer_seed, fingerprint, client_key):
    return subprocess.run([TELEPORT, "ticket", "sign", "--issuer-key-file", issuer_seed,
                           "--host-fingerprint", fingerprint, "--client-key", client_key,
                           "--name", "Support (Browser test)", "--session-id", "web-e2e"],
                          check=True, capture_output=True, text=True).stdout.strip()

@contextlib.contextmanager
def issuer_site(grant_for):
    """Serves web/client (page + bundle) and POST /redeem/<code>: first use
    gets grant_for(code, client_key), later uses 410."""
    spent = set()
    class Handler(http.server.SimpleHTTPRequestHandler):
        def __init__(self, *a, **k): super().__init__(*a, directory=ROOT, **k)
        def log_message(self, *a): pass
        def end_headers(self):
            self.send_header("Cache-Control", "no-store"); super().end_headers()
        def do_POST(self):
            code = self.path.rsplit("/", 1)[-1]
            body = json.loads(self.rfile.read(int(self.headers["Content-Length"])))
            if code in spent:
                status, answer = 410, {"error": "this session link is no longer valid"}
            else:
                spent.add(code)
                status, answer = 200, grant_for(code, body["client_key"])
            data = json.dumps(answer).encode()
            self.send_response(status); self.send_header("Content-Type", "application/json")
            self.send_header("Content-Length", str(len(data))); self.end_headers(); self.wfile.write(data)
    server = http.server.ThreadingHTTPServer(("127.0.0.1", 0), Handler)
    threading.Thread(target=server.serve_forever, daemon=True).start()
    try: yield f"http://localhost:{server.server_address[1]}"
    finally: server.shutdown()

keys = tempfile.mkdtemp(prefix="web-e2e-keys-")
issuer = keygen("issuer-keygen", os.path.join(keys, "issuer"))
keygen("issuer-keygen", os.path.join(keys, "stranger"))
results = {}
with relay() as (port, rfp, rkey), \
     host(port, rfp, rkey, ["--ticket-issuer-key", issuer, "--ticket-approval", "issuer"]) as (ns, hfp, events, tmp):
    def grant_for(code, client_key):
        seed = os.path.join(keys, "stranger" if code.startswith("foreign") else "issuer")
        return {"ticket": sign(seed, hfp, client_key), "name": "Test host", "host_fingerprint": hfp,
                "relay": {"url": f"moqt://127.0.0.1:{port}", "key": rkey,
                          "web_url": f"https://127.0.0.1:{port}/", "cert_hashes": [rfp]}}
    with issuer_site(grant_for) as base, browser_page(base, "test/view.html") as (page, logs):
        run = "([u, n, ms]) => run(u, n, ms)"
        results["good"] = page.evaluate(run, [f"{base}/redeem/one", 10, 30000])
        results["reuse"] = page.evaluate(run, [f"{base}/redeem/one", 1, 10000])
        results["foreign"] = page.evaluate(run, [f"{base}/redeem/foreign-two", 1, 25000])
        for line in logs[-8:]: print("console:", line[:200])
    log = open(os.path.join(tmp, "host.log")).read()
for name, value in results.items(): print(name, json.dumps(value)[:400])
good, reuse, foreign = results["good"], results["reuse"], results["foreign"]
checks = {
    "frames decoded": good["frames"] >= 10,
    "picture is not flat": good["distinct"] > 1,
    "went live": any(s == "live" for s, _ in good["states"]),
    "reuse refused": reuse["frames"] == 0 and any(s in ("error", "thrown") and "no longer valid" in d for s, d in reuse["states"]),
    "foreign issuer refused": foreign["frames"] == 0 and any(s in ("error", "thrown") for s, _ in foreign["states"]),
    "host saw the foreign ticket refused": "relayed ticket refused" in log,
}
for name, ok in checks.items(): print("PASS" if ok else "FAIL", name)
print("WEB E2E", "PASS" if all(checks.values()) else "FAIL")
sys.exit(0 if all(checks.values()) else 1)
