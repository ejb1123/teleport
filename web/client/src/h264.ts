// H.264 helpers for WebCodecs: Teleport sends each access unit as Annex B
// (start codes, parameter sets inline before every keyframe), which
// Chromium's VideoDecoder takes without a description.

/** NAL units of an Annex B access unit (without their start codes). */
export function nalUnits(data: Uint8Array): Uint8Array[] {
  const units: Uint8Array[] = [];
  let start = -1;
  let i = 0;
  while (i + 2 < data.length) {
    const three = data[i] === 0 && data[i + 1] === 0 && data[i + 2] === 1;
    const four =
      i + 3 < data.length && data[i] === 0 && data[i + 1] === 0 && data[i + 2] === 0 && data[i + 3] === 1;
    if (three || four) {
      if (start >= 0) units.push(data.subarray(start, i));
      i += four ? 4 : 3;
      start = i;
    } else {
      i += 1;
    }
  }
  if (start >= 0 && start < data.length) units.push(data.subarray(start));
  return units.filter((unit) => unit.length > 0);
}

/** NAL unit type (5 IDR, 7 SPS, 8 PPS). */
export function nalType(unit: Uint8Array): number {
  return unit[0] & 0x1f;
}

/** Whether the access unit holds an IDR slice. */
export function isKeyframe(data: Uint8Array): boolean {
  return nalUnits(data).some((unit) => nalType(unit) === 5);
}

/**
 * The WebCodecs codec string (`avc1.PPCCLL`) from the access unit's SPS,
 * or undefined when it carries none.
 */
export function codecString(data: Uint8Array): string | undefined {
  const sps = nalUnits(data).find((unit) => nalType(unit) === 7);
  if (!sps || sps.length < 4) return undefined;
  const hex = (byte: number) => byte.toString(16).padStart(2, "0");
  return `avc1.${hex(sps[1])}${hex(sps[2])}${hex(sps[3])}`;
}
