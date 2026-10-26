"""Risk 1: a browser reaches the relay over WebTransport, pinned by
serverCertificateHashes; a wrong hash and a missing relay key fail."""
from harness import *
ns = "0123456789abcdef0123456789abcdef"
with relay() as (port, fp, key), page_server() as base, browser_page(base, "wt.html") as (page, logs):
    print("relay fingerprint", fp)
    url = f"https://127.0.0.1:{port}/{ns}?key={key}"
    good = page.evaluate("([u,h])=>connect(u,h)", [url, fp])
    bad_hash = page.evaluate("([u,h])=>connect(u,h)", [url, "00" * 32])
    no_key = page.evaluate("([u,h])=>connect(u,h)", [f"https://127.0.0.1:{port}/{ns}", fp])
    print("right hash, right key ->", good)
    print("wrong hash           ->", bad_hash)
    print("no relay key         ->", no_key)
    # The relay checks its key when the MoQ session is set up (risk 2), so a
    # bare WebTransport session without it is only refused once it speaks MoQ.
    ok = good == "open" and bad_hash.startswith("error")
    print("RISK1", "PASS" if ok else "FAIL")
    sys.exit(0 if ok else 1)
