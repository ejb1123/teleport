import { describe, expect, it } from "vitest";
import { base64url, hexBytes } from "../src/b64";
import { codecString, isKeyframe, nalUnits } from "../src/h264";
import { type GroupWriter, InputWriter } from "../src/input";

describe("h264", () => {
  // SPS (profile 0x42, constraints 0xc0, level 0x1e), PPS, IDR slice.
  const key = new Uint8Array([0, 0, 0, 1, 0x67, 0x42, 0xc0, 0x1e, 0xaa, 0, 0, 1, 0x68, 0xce, 0, 0, 1, 0x65, 0x88, 0x84]);
  const delta = new Uint8Array([0, 0, 0, 1, 0x41, 0x9a, 0x02]);

  it("splits Annex B with three- and four-byte start codes", () => {
    expect(nalUnits(key).map((unit) => unit[0] & 0x1f)).toEqual([7, 8, 5]);
    expect(nalUnits(new Uint8Array([1, 2, 3]))).toEqual([]);
  });
  it("tells keyframes from deltas", () => {
    expect(isKeyframe(key)).toBe(true);
    expect(isKeyframe(delta)).toBe(false);
  });
  it("derives the WebCodecs codec string from the SPS, only when there is one", () => {
    expect(codecString(key)).toBe("avc1.42c01e");
    expect(codecString(delta)).toBeUndefined();
  });
});

describe("base64url and hex", () => {
  it("encodes without padding, URL-safe", () => {
    expect(base64url(new Uint8Array([0xfb, 0xff, 0xfe]))).toBe("-__-");
    expect(base64url(new Uint8Array([0x66]))).toBe("Zg");
  });
  it("decodes hex and refuses odd or non-hex input", () => {
    expect(Array.from(hexBytes("00ff10"))).toEqual([0, 255, 16]);
    expect(() => hexBytes("abc")).toThrow();
    expect(() => hexBytes("zz")).toThrow();
  });
});

describe("InputWriter", () => {
  function fake() {
    const groups: { sequence: number; frames: string[]; closed: boolean }[] = [];
    const sealed: number[] = [];
    const writer = new InputWriter(
      (): GroupWriter => {
        const group = { sequence: groups.length, frames: [] as string[], closed: false };
        groups.push(group);
        return {
          sequence: group.sequence,
          write: (payload) => group.frames.push(new TextDecoder().decode(payload)),
          close: () => {
            group.closed = true;
          },
        };
      },
      (group, plain) => {
        sealed.push(group);
        return plain;
      },
    );
    return { writer, groups, sealed };
  }

  it("sends select_codec alone in group 0, then numbers events on", () => {
    const { writer, groups } = fake();
    writer.sendAlone({ type: "select_codec", codec: "h264" });
    writer.send({ type: "ping" });
    expect(groups.length).toBe(2);
    expect(groups[0].closed).toBe(true);
    expect(JSON.parse(groups[0].frames[0])).toEqual({ sequence: 0, event: { type: "select_codec", codec: "h264" } });
    expect(JSON.parse(groups[1].frames[0]).sequence).toBe(1);
  });

  it("starts a new group every 256 events and seals each frame for its group", () => {
    const { writer, groups, sealed } = fake();
    for (let i = 0; i < 600; i++) writer.send({ type: "ping" });
    expect(groups.map((g) => g.frames.length)).toEqual([256, 256, 88]);
    expect(groups[0].closed && groups[1].closed && !groups[2].closed).toBe(true);
    expect(sealed.slice(254, 258)).toEqual([0, 0, 1, 1]);
    expect(writer.sequence).toBe(600);
  });
});
