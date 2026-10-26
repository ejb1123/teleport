# Browser client risk spike

The four risks that decide whether the browser client in
`docs/design/self-hosted-remote-support.md` §7 is feasible, each checked in a
real headless browser with a control that must fail. Not the client itself.
`bash web/spike/run.sh` builds what they need and runs them.

| # | Risk | Check | Result (2026-10-26, Playwright Chromium 1243) |
|---|---|---|---|
| 1 | The relay accepts a browser over WebTransport | `risk1.py`: `new WebTransport(url, {serverCertificateHashes})` against `teleport relay` | Right hash: session open. Wrong hash: "Opening handshake failed". Also passes in Playwright Firefox. |
| 2 | moq-lite-05 from JavaScript | `risk2.py`: `@moq/net` 0.3.5 subscribes to a native host's signed `presence` track through the relay | One object, `version=moq-lite-05`, 1543 bytes of the host's statement. Wrong relay key: connection lost. Another namespace: remote error 30. |
| 3 | The E2E layer in WebAssembly | `risk3.py`: `src/e2e.rs` itself, compiled by `web/e2e-wasm` to wasm32, opens the native known-answer vector (`e2e::vector`) | Plaintext recovered. Flipped byte and wrong group: "object failed authentication". The client handshake runs on the browser's randomness (two different hellos). 175 KB of WASM. |
| 4 | WebCodecs decodes Teleport's H.264 | `risk4.py`: two seconds of access units exactly as a session publishes them (`dump_h264_access_units_for_the_browser_spike`) into `VideoDecoder`, Annex B with no description, and also converted to AVC with an `avcC` description | 60 of 60 frames both ways. Corrupted keyframe refused. |

Not proven here: decoding in Firefox and Safari. Playwright's Firefox build has
no H.264 decoder, and its Linux WebKit has neither WebTransport nor an H.264
decoder, so those runs say nothing about the shipping browsers. Check on real
Firefox 130+ and Safari 26.4+.
