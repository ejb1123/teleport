"""Risk 2: @moq/net 0.3.5 in Chromium speaks moq-lite-05 to the relay and
receives an object a native host publishes (its signed presence statement).
Controls: wrong relay key, and another namespace, receive nothing."""
from harness import *
with relay() as (port, rfp, key), host(port, rfp, key) as (ns, hfp, events, tmp), \
     page_server() as base, browser_page(base, "moq.html") as (page, logs):
    print("host namespace", ns)
    call = "([u,h,b,t])=>readOne(u,h,b,t)"
    good = page.evaluate(call, [f"https://127.0.0.1:{port}/{ns}?key={key}", rfp, "presence", "presence"])
    bad_key = page.evaluate(call, [f"https://127.0.0.1:{port}/{ns}?key=wrong", rfp, "presence", "presence"])
    other_ns = page.evaluate(call, [f"https://127.0.0.1:{port}/{'f'*32}?key={key}", rfp, "presence", "presence"])
    print("host namespace, key  ->", good[:200])
    print("wrong relay key      ->", bad_key[:200])
    print("another namespace    ->", other_ns[:200])
    for l in logs[-5:]: print("console:", l[:200])
    ok = good.startswith("object:") and "moq-lite-05" in good and bad_key.startswith("error") and other_ns.startswith("error")
    print("RISK2", "PASS" if ok else "FAIL")
    sys.exit(0 if ok else 1)
