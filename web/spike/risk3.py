"""Risk 3: src/e2e.rs compiled to wasm32 opens, in Chromium, an object the
native build sealed (known-answer vector); a flipped byte and a wrong group
fail; the client handshake runs on the browser's randomness."""
from harness import *
with page_server() as base, browser_page(base, "e2e.html") as (page, logs):
    page.wait_for_function("window.ready === true")
    r = page.evaluate("runE2E()")
    for k, v in r.items(): print(f"{k:10} -> {v[:110]}")
    ok = (r["good"] == "plain: teleport e2e known-answer vector"
          and r["flipped"].startswith("error") and r["wrongGroup"].startswith("error")
          and len(r["hello1"]) > 64 and r["hello1"] != r["hello2"])
    print("RISK3", "PASS" if ok else "FAIL")
    sys.exit(0 if ok else 1)
