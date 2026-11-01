"""Risk 4: Chromium's WebCodecs VideoDecoder decodes Teleport's H.264 Annex B
access units as the host publishes them; a corrupted keyframe does not."""
from harness import *
import json
with page_server() as base, browser_page(base, "webcodecs.html") as (page, logs):
    r = page.evaluate("runDecode()")
    print(json.dumps(r))
    ok = r["supported"] and r["good"]["frames"] > 0 and not r["good"]["errors"] \
        and (r["corrupt"]["frames"] == 0 and r["corrupt"]["errors"])
    print("RISK4", "PASS" if ok else "FAIL")
    sys.exit(0 if ok else 1)
