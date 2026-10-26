// Risk 2: subscribe, from a browser, to a broadcast a native host publishes
// through the relay. Bundled with esbuild against @moq/net 0.3.5.
import * as Moq from "@moq/net";

function hexBytes(hex) {
  const a = new Uint8Array(hex.length / 2);
  for (let i = 0; i < a.length; i++) a[i] = parseInt(hex.substr(i * 2, 2), 16);
  return a;
}

// Read one object of `track` in `broadcast` under `url`; "object: <text>" or
// the error.
window.readOne = async (url, hashHex, broadcast, track, timeoutMs = 10000) => {
  let conn;
  try {
    conn = await Moq.Connection.connect(new URL(url), {
      webtransport: { serverCertificateHashes: [{ algorithm: "sha-256", value: hexBytes(hashHex) }] },
      websocket: { enabled: false },
    });
    const sub = conn.consume(Moq.Path.from(broadcast)).subscribe(track);
    const timer = new Promise((_, rej) => setTimeout(() => rej(new Error("timeout waiting for an object")), timeoutMs));
    const group = await Promise.race([sub.recvGroup(), timer]);
    if (!group) return "error: track ended";
    const frame = await Promise.race([group.readFrame(), timer]);
    if (!frame) return "error: group ended";
    return `object: version=${conn.version} bytes=${frame.payload.length} ` +
      new TextDecoder().decode(frame.payload.slice(0, 120));
  } catch (e) {
    return "error: " + (e && e.message || e);
  } finally {
    try { conn && conn.close(); } catch (_) {}
  }
};
