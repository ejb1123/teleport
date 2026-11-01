/**
 * `@teleport/web`: open a Teleport session in the browser (view only).
 *
 *   const session = await openSession(canvas, { redeemUrl, onState });
 *   session.close();
 *
 * The page's issuer hands it a single-use redeem URL. This module makes a
 * fresh client key (in WebAssembly memory, never stored), posts only its
 * public half to that URL, and gets a session ticket bound to it with the
 * host fingerprint and the relay. It then connects to the relay over
 * WebTransport, opens an end-to-end encrypted session with the ticket in the
 * handshake (docs/tickets.md#tickets-in-the-handshake), and decodes the
 * host's H.264 stream with WebCodecs onto the canvas. The relay sees no
 * content and never the ticket.
 */
import * as Moq from "@moq/net";
import init, { ClientKey, TicketSession, namespace, random_hex } from "../wasm/teleport_e2e_wasm.js";
import { base64url, hexBytes } from "./b64.js";
import { codecString, isKeyframe } from "./h264.js";
import { type GroupWriter, InputWriter } from "./input.js";

export type State = "connecting" | "live" | "ended" | "error";

export interface Options {
  /** Single-use redeem URL from the issuer's site. */
  redeemUrl: string;
  /** State changes; `detail` says why on `ended` and `error`. */
  onState?: (state: State, detail?: string) => void;
  /** Override the relay's WebTransport URL from the grant (tests). */
  relayUrl?: string;
  /** Called with each decoded frame's size (tests, stats). */
  onFrame?: (width: number, height: number) => void;
}

export interface Session {
  /** End the session: closes the relay connection and the decoder. */
  close(): void;
  /** Decoded frames so far. */
  readonly frames: number;
  /** The host's display name from the grant. */
  readonly name: string;
}

interface Grant {
  ticket: string;
  name?: string;
  host_fingerprint: string;
  relay: { url: string; fingerprint?: string; key?: string; web_url?: string; cert_hashes?: string[] };
}

const HANDSHAKE_TIMEOUT_MS = 15_000;
const HEARTBEAT_MS = 500;

let wasmReady: Promise<unknown> | undefined;

function timeout<T>(promise: Promise<T>, ms: number, what: string): Promise<T> {
  return new Promise<T>((resolve, reject) => {
    const timer = setTimeout(() => reject(new Error(`${what} timed out`)), ms);
    promise.then(
      (value) => {
        clearTimeout(timer);
        resolve(value);
      },
      (error) => {
        clearTimeout(timer);
        reject(error);
      },
    );
  });
}

async function fetchGrant(redeemUrl: string, key: ClientKey): Promise<Grant> {
  const response = await fetch(redeemUrl, {
    method: "POST",
    headers: { "Content-Type": "application/json" },
    credentials: "omit",
    cache: "no-store",
    redirect: "error",
    body: JSON.stringify({ client_key: base64url(key.publicKey()) }),
  });
  const body = await response.json().catch(() => ({}));
  if (!response.ok) throw new Error(body?.error || `the session link was refused (HTTP ${response.status})`);
  if (typeof body?.ticket !== "string" || !body.ticket.startsWith("tkt1.")) throw new Error("no session ticket in the answer");
  if (!/^[0-9a-f]{64}$/.test(body?.host_fingerprint ?? "")) throw new Error("invalid host in the answer");
  if (typeof body?.relay?.url !== "string") throw new Error("no relay in the answer");
  return body as Grant;
}

/** The relay's WebTransport URL: `web_url`, else the moqt URL as https. */
export function webTransportUrl(relay: Grant["relay"], hostFingerprintNamespace: string): URL {
  const base = new URL(relay.web_url || relay.url.replace(/^moqt:/, "https:"));
  if (base.protocol !== "https:") throw new Error("the relay needs an https WebTransport URL");
  if (base.pathname.replace(/\/+$/, "") === "") base.pathname = `/${hostFingerprintNamespace}`;
  if (relay.key) base.searchParams.set("key", relay.key);
  return base;
}

/** One message (frames of one group) on a `message` track, as the native handshake publishes. */
function publishMessage(conn: Moq.Connection.Established, path: string, frames: Uint8Array[]): Moq.Broadcast.Producer {
  const broadcast = new Moq.Broadcast.Producer();
  const track = broadcast.createTrack("message", { ordered: true } as Partial<Moq.Track.Info>);
  const group = track.appendGroup();
  for (const payload of frames) group.writeFrame({ payload, timestamp: Moq.Time.Timestamp.now() });
  group.close();
  conn.publish(Moq.Path.from(path), broadcast);
  return broadcast;
}

/** Wait until `path` is announced, then consume it. */
async function announced(conn: Moq.Connection.Established, path: string): Promise<Moq.Broadcast.Consumer> {
  const watch = conn.announced(Moq.Path.from(path));
  try {
    for (;;) {
      const event = await watch.next();
      if (!event) throw new Error("the relay closed the connection");
      if (event.active) return conn.consume(Moq.Path.from(path));
    }
  } finally {
    watch.close();
  }
}

async function readMessage(broadcast: Moq.Broadcast.Consumer): Promise<Uint8Array> {
  const sub = broadcast.subscribe("message", { startGroup: 0, ordered: true });
  const group = await sub.recvGroup();
  if (!group) throw new Error("empty handshake message");
  const frame = await group.readFrame();
  if (!frame) throw new Error("empty handshake message");
  return frame.payload;
}

export async function openSession(canvas: HTMLCanvasElement, options: Options): Promise<Session> {
  const report = (state: State, detail?: string) => {
    try {
      options.onState?.(state, detail);
    } catch {
      /* the page's handler must not break the session */
    }
  };
  report("connecting");
  let conn: Moq.Connection.Established | undefined;
  let decoder: VideoDecoder | undefined;
  let heartbeat: ReturnType<typeof setInterval> | undefined;
  let closed = false;
  let frames = 0;
  const keep: { close(): void }[] = [];
  const close = (state: State = "ended", detail?: string) => {
    if (closed) return;
    closed = true;
    if (heartbeat) clearInterval(heartbeat);
    for (const item of keep) {
      try {
        item.close();
      } catch {
        /* already closed */
      }
    }
    try {
      decoder?.close();
    } catch {
      /* already closed */
    }
    try {
      conn?.close();
    } catch {
      /* already closed */
    }
    report(state, detail);
  };
  try {
    wasmReady ??= init();
    await wasmReady;
    const key = new ClientKey();
    const grant = await fetchGrant(options.redeemUrl, key);
    const ns = namespace(grant.host_fingerprint);
    const url = options.relayUrl ? new URL(options.relayUrl) : webTransportUrl(grant.relay, ns);
    if (options.relayUrl && url.pathname.replace(/\/+$/, "") === "") url.pathname = `/${ns}`;
    if (options.relayUrl && grant.relay.key) url.searchParams.set("key", grant.relay.key);
    const hashes = (grant.relay.cert_hashes ?? []).map((hash) => ({ algorithm: "sha-256", value: hexBytes(hash) }));
    conn = await timeout(
      Moq.Connection.connect(url, {
        webtransport: hashes.length ? { serverCertificateHashes: hashes } : {},
        websocket: { enabled: false },
      } as Moq.Connection.ConnectProps),
      HANDSHAKE_TIMEOUT_MS,
      "connecting to the relay",
    );
    const sid = random_hex(16);
    const session = new TicketSession(grant.ticket, "codec=h264", key);
    keep.push(publishMessage(conn, `${sid}/hello`, [session.hello()]));
    const reply = await timeout(
      announced(conn, `${sid}/reply`).then(readMessage),
      HANDSHAKE_TIMEOUT_MS,
      "waiting for the host (the session link may be used up or not for this host)",
    );
    const confirmation = Array.from(session.finish(reply, grant.host_fingerprint)) as Uint8Array[];
    keep.push(publishMessage(conn, `${sid}/confirm`, confirmation));

    // Our controls: the host subscribes to `input` (and may ask for others,
    // which a view-only client does not have).
    const controls = new Moq.Broadcast.Producer();
    const inputTrack = controls.createTrack("input", { ordered: true } as Partial<Moq.Track.Info>);
    conn.publish(Moq.Path.from(`${sid}/controls`), controls);
    keep.push(controls);
    void (async () => {
      for (;;) {
        const request = await controls.requested();
        if (!request) return;
        request.reject(new Error("not published by a view-only client"));
      }
    })();
    const input = new InputWriter(
      (): GroupWriter => {
        const group = inputTrack.appendGroup();
        return {
          sequence: group.sequence,
          write: (payload) => group.writeFrame({ payload, timestamp: Moq.Time.Timestamp.now() }),
          close: () => group.close(),
        };
      },
      (group, plain) => session.seal("input", BigInt(group), plain),
    );
    keep.push(input);

    // The host's desktop: metadata first, then video.
    const desktop = await timeout(announced(conn, `${sid}/desktop`), HANDSHAKE_TIMEOUT_MS, "waiting for the host's desktop");
    keep.push(desktop);
    const metadata = desktop.subscribe("desktop", { startGroup: 0, ordered: true });
    const first = await timeout(metadata.recvGroup(), HANDSHAKE_TIMEOUT_MS, "reading the desktop");
    const firstFrame = first && (await first.readFrame());
    if (!first || !firstFrame) throw new Error("the host sent no desktop description");
    const info = JSON.parse(new TextDecoder().decode(session.open("desktop", BigInt(first.sequence), firstFrame.payload)));
    if (info.version !== 2) throw new Error("incompatible Teleport host version");
    if (info.codec_selection && !info.shared_stream) input.sendAlone({ type: "select_codec", codec: "h264" });
    else if (info.codec !== "h264") throw new Error("the host streams a codec this client does not decode");
    input.send({ type: "ping" });
    heartbeat = setInterval(() => {
      try {
        input.send({ type: "ping" });
      } catch (error) {
        close("error", String(error));
      }
    }, HEARTBEAT_MS);
    const requestKeyframe = () => {
      if (info.keyframe_requests) input.send({ type: "request_keyframe" });
    };

    const context = canvas.getContext("2d");
    if (!context) throw new Error("no 2D canvas context");
    let configured = false;
    let waitingForKey = true;
    decoder = new VideoDecoder({
      output: (frame) => {
        if (canvas.width !== frame.displayWidth || canvas.height !== frame.displayHeight) {
          canvas.width = frame.displayWidth;
          canvas.height = frame.displayHeight;
        }
        context.drawImage(frame, 0, 0);
        frames += 1;
        options.onFrame?.(frame.displayWidth, frame.displayHeight);
        frame.close();
        if (frames === 1) report("live");
      },
      error: () => {
        // Decoder state is lost: start again at the next keyframe.
        configured = false;
        waitingForKey = true;
        requestKeyframe();
      },
    });
    const video = desktop.subscribe("h264", { priority: 1, latencyMax: 100 });
    requestKeyframe();
    void (async () => {
      try {
        for (;;) {
          const group = await video.recvGroup();
          if (!group) {
            close("ended", "the host ended the session");
            return;
          }
          for (;;) {
            const frame = await group.readFrame();
            if (!frame) break;
            let plain: Uint8Array;
            try {
              plain = session.open("h264", BigInt(group.sequence), frame.payload);
            } catch {
              continue; // not ours: never decoded
            }
            const key = isKeyframe(plain);
            if (waitingForKey && !key) continue;
            if (!configured || decoder.state !== "configured") {
              const codec = codecString(plain);
              if (!key || !codec) continue;
              const config = { codec, optimizeForLatency: true };
              const support = await VideoDecoder.isConfigSupported(config).catch(() => ({ supported: false }));
              if (!support.supported) {
                close("error", `this browser cannot decode the host's video (${codec})`);
                return;
              }
              decoder.configure(config);
              configured = true;
            }
            waitingForKey = false;
            decoder.decode(
              new EncodedVideoChunk({ type: key ? "key" : "delta", timestamp: frame.timestamp.value * 1000, data: plain }),
            );
          }
        }
      } catch (error) {
        if (!closed) close("error", String((error as Error)?.message ?? error));
      }
    })();
    conn.closed.then(
      () => {
        if (!closed) close("ended", "the relay closed the connection");
      },
      (error: unknown) => {
        if (!closed) close("error", String((error as Error)?.message ?? error));
      },
    );
    return {
      close: () => close("ended", "closed"),
      get frames() {
        return frames;
      },
      name: grant.name ?? "",
    };
  } catch (error) {
    close("error", String((error as Error)?.message ?? error));
    throw error;
  }
}
