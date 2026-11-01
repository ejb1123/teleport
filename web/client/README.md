# @teleport/web: Teleport in the browser (view only)

```js
import { openSession } from "./teleport-web.js";
const session = await openSession(canvas, {
  redeemUrl,                       // single use, from your issuer's site
  onState(state, detail) {},       // connecting | live | ended | error
});
session.close();
```

**Build:** `nix build .#web-client` produces `teleport-web.js` and
`teleport_e2e_wasm_bg.wasm`. Serve both from one directory; the module
loads the WASM file from next to itself. The build runs the type check and
the unit tests.

**What it does.**
1. Makes a client key in WebAssembly memory. It is never stored or exported.
2. POSTs only the public half to `redeemUrl` (protocol in
   `docs/tickets.md` "Links from a web page"). The answer may add
   `relay.web_url` (the relay's WebTransport URL; by default the `moqt` URL
   as `https`) and `relay.cert_hashes`, which is for local development only
   (`serverCertificateHashes`).
3. Connects to the relay over WebTransport with `@moq/net`, which speaks
   moq-lite-05 as the native client does.
4. Opens the session with the ticket in the end-to-end handshake
   (`docs/tickets.md` "Tickets in the handshake"). `src/e2e.rs` itself,
   compiled to WASM by `web/e2e-wasm`, seals and opens every object, so the
   relay sees no content and never the ticket.
5. Decodes the host's H.264 with WebCodecs onto the canvas, requesting a
   keyframe on start and after decoder errors.
6. Sends a heartbeat every 500 ms on the sealed input track.

**Browsers.**

| | Proven here | Not proven |
|---|---|---|
| Chromium | Headless Chromium end to end: `test/run.sh` | |
| Firefox | Playwright's Firefox connects, runs the handshake and receives video | It cannot decode, because that build has no H.264 decoder; the session ends with "this browser cannot decode the host's video". Real Firefox 130+ has not been run here. |
| Safari | | Not run (needs 26.4+). |

**Limits:** view only, H.264 only, no audio. Input is milestone R2.

**Tests.**
- `npm test` runs the unit tests (vitest). `npm run typecheck` runs tsc.
- `bash web/client/test/run.sh` runs a real relay, a real host, a loopback
  issuer page and headless Chromium. It checks:
  - frames are decoded and the picture is not flat;
  - a reused link is refused;
  - a ticket from an untrusted issuer is refused.
