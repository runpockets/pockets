/**
 * Zero-dependency ISO/IEC 18004 QR Code encoder (Byte mode, Error Correction
 * Level M, Versions 1–10 — up to 213 UTF-8 bytes) for Tailscale HTTPS URLs.
 *
 * Returns a deterministic boolean matrix (`true` = dark module) that callers
 * render as crisp vector SVG with a standard 4-module light quiet zone.
 */

export interface QrMatrix {
  version: number;
  size: number;
  /** Row-major flat array of length `size * size`; `true` = dark module. */
  modules: boolean[];
}

interface VersionSpec {
  version: number;
  totalDataBytes: number;
  ecBytesPerBlock: number;
  group1Blocks: number;
  group1DataBytes: number;
  group2Blocks: number;
  group2DataBytes: number;
  alignment: readonly number[];
}

/** ISO/IEC 18004 Table 9 (EC Level M) + Annex E alignment centers, Versions 1–10. */
const SPECS: readonly VersionSpec[] = [
  {
    version: 1,
    totalDataBytes: 16,
    ecBytesPerBlock: 10,
    group1Blocks: 1,
    group1DataBytes: 16,
    group2Blocks: 0,
    group2DataBytes: 0,
    alignment: [],
  },
  {
    version: 2,
    totalDataBytes: 28,
    ecBytesPerBlock: 16,
    group1Blocks: 1,
    group1DataBytes: 28,
    group2Blocks: 0,
    group2DataBytes: 0,
    alignment: [6, 18],
  },
  {
    version: 3,
    totalDataBytes: 44,
    ecBytesPerBlock: 26,
    group1Blocks: 1,
    group1DataBytes: 44,
    group2Blocks: 0,
    group2DataBytes: 0,
    alignment: [6, 22],
  },
  {
    version: 4,
    totalDataBytes: 64,
    ecBytesPerBlock: 18,
    group1Blocks: 2,
    group1DataBytes: 32,
    group2Blocks: 0,
    group2DataBytes: 0,
    alignment: [6, 26],
  },
  {
    version: 5,
    totalDataBytes: 86,
    ecBytesPerBlock: 24,
    group1Blocks: 2,
    group1DataBytes: 43,
    group2Blocks: 0,
    group2DataBytes: 0,
    alignment: [6, 30],
  },
  {
    version: 6,
    totalDataBytes: 108,
    ecBytesPerBlock: 16,
    group1Blocks: 4,
    group1DataBytes: 27,
    group2Blocks: 0,
    group2DataBytes: 0,
    alignment: [6, 34],
  },
  {
    version: 7,
    totalDataBytes: 124,
    ecBytesPerBlock: 18,
    group1Blocks: 4,
    group1DataBytes: 31,
    group2Blocks: 0,
    group2DataBytes: 0,
    alignment: [6, 22, 38],
  },
  {
    version: 8,
    totalDataBytes: 154,
    ecBytesPerBlock: 22,
    group1Blocks: 2,
    group1DataBytes: 38,
    group2Blocks: 2,
    group2DataBytes: 39,
    alignment: [6, 24, 42],
  },
  {
    version: 9,
    totalDataBytes: 182,
    ecBytesPerBlock: 22,
    group1Blocks: 3,
    group1DataBytes: 36,
    group2Blocks: 2,
    group2DataBytes: 37,
    alignment: [6, 26, 46],
  },
  {
    version: 10,
    totalDataBytes: 216,
    ecBytesPerBlock: 26,
    group1Blocks: 4,
    group1DataBytes: 43,
    group2Blocks: 1,
    group2DataBytes: 44,
    alignment: [6, 28, 50],
  },
];

// GF(256) lookup tables with primitive polynomial x^8 + x^4 + x^3 + x^2 + 1 (0x11d).
const GF_EXP = new Uint8Array(512);
const GF_LOG = new Uint8Array(256);
{
  let value = 1;
  for (let i = 0; i < 255; i++) {
    GF_EXP[i] = value;
    GF_LOG[value] = i;
    value <<= 1;
    if (value & 0x100) value ^= 0x11d;
  }
  for (let i = 255; i < 512; i++) {
    GF_EXP[i] = GF_EXP[i - 255];
  }
}

function gfMul(a: number, b: number): number {
  if (a === 0 || b === 0) return 0;
  return GF_EXP[GF_LOG[a] + GF_LOG[b]];
}

function rsGenerator(degree: number): Uint8Array {
  let poly = new Uint8Array([1]);
  for (let i = 0; i < degree; i++) {
    const next = new Uint8Array(poly.length + 1);
    const root = GF_EXP[i];
    for (let j = 0; j < poly.length; j++) {
      next[j] ^= poly[j];
      next[j + 1] ^= gfMul(poly[j], root);
    }
    poly = next;
  }
  return poly;
}

function rsRemainder(data: Uint8Array, ecLen: number): Uint8Array {
  const gen = rsGenerator(ecLen);
  const rem = new Uint8Array(ecLen);
  for (let i = 0; i < data.length; i++) {
    const factor = data[i] ^ rem[0];
    rem.copyWithin(0, 1);
    rem[ecLen - 1] = 0;
    if (factor !== 0) {
      for (let j = 0; j < ecLen; j++) {
        rem[j] ^= gfMul(gen[j + 1], factor);
      }
    }
  }
  return rem;
}

function selectSpec(byteLen: number): VersionSpec {
  for (const spec of SPECS) {
    const headerBits = 4 + (spec.version <= 9 ? 8 : 16);
    const capacityBits = spec.totalDataBytes * 8;
    if (headerBits + byteLen * 8 <= capacityBits) {
      return spec;
    }
  }
  throw new Error(
    `QR payload of ${byteLen} bytes exceeds maximum supported length.`,
  );
}

function encodeDataCodewords(bytes: Uint8Array, spec: VersionSpec): Uint8Array {
  const totalBits = spec.totalDataBytes * 8;
  const bits: number[] = [];
  const pushBits = (value: number, count: number) => {
    for (let i = count - 1; i >= 0; i--) {
      bits.push((value >>> i) & 1);
    }
  };

  // Mode indicator: 0100 (Byte mode).
  pushBits(0b0100, 4);
  // Character count indicator: 8 bits for V1–9, 16 bits for V10.
  pushBits(bytes.length, spec.version <= 9 ? 8 : 16);
  for (const byte of bytes) {
    pushBits(byte, 8);
  }
  // Terminator (up to 4 zero bits).
  const terminator = Math.min(4, totalBits - bits.length);
  pushBits(0, terminator);
  // Pad to byte boundary.
  while (bits.length % 8 !== 0) bits.push(0);

  const out = new Uint8Array(spec.totalDataBytes);
  let idx = 0;
  for (let i = 0; i < bits.length; i += 8) {
    let b = 0;
    for (let j = 0; j < 8; j++) b = (b << 1) | bits[i + j];
    out[idx++] = b;
  }
  // Pad codewords alternating 0xEC and 0x11.
  let padToggle = false;
  while (idx < spec.totalDataBytes) {
    out[idx++] = padToggle ? 0x11 : 0xec;
    padToggle = !padToggle;
  }
  return out;
}

function interleaveCodewords(data: Uint8Array, spec: VersionSpec): Uint8Array {
  const dataBlocks: Uint8Array[] = [];
  const ecBlocks: Uint8Array[] = [];
  let offset = 0;

  for (let i = 0; i < spec.group1Blocks; i++) {
    const block = data.slice(offset, offset + spec.group1DataBytes);
    offset += spec.group1DataBytes;
    dataBlocks.push(block);
    ecBlocks.push(rsRemainder(block, spec.ecBytesPerBlock));
  }
  for (let i = 0; i < spec.group2Blocks; i++) {
    const block = data.slice(offset, offset + spec.group2DataBytes);
    offset += spec.group2DataBytes;
    dataBlocks.push(block);
    ecBlocks.push(rsRemainder(block, spec.ecBytesPerBlock));
  }

  const maxDataLen = Math.max(spec.group1DataBytes, spec.group2DataBytes);
  const totalBlocks = spec.group1Blocks + spec.group2Blocks;
  const result = new Uint8Array(
    spec.totalDataBytes + totalBlocks * spec.ecBytesPerBlock,
  );
  let pos = 0;
  for (let col = 0; col < maxDataLen; col++) {
    for (const block of dataBlocks) {
      if (col < block.length) result[pos++] = block[col];
    }
  }
  for (let col = 0; col < spec.ecBytesPerBlock; col++) {
    for (const ec of ecBlocks) {
      result[pos++] = ec[col];
    }
  }
  return result;
}

function formatBitsForMask(mask: number): number {
  // EC Level M = 00.
  const data = (0b00 << 3) | (mask & 0b111);
  let rem = data << 10;
  for (let i = 14; i >= 10; i--) {
    if (rem & (1 << i)) rem ^= 0x537 << (i - 10);
  }
  return ((data << 10) | rem) ^ 0x5412;
}

function versionBits(version: number): number {
  let rem = version << 12;
  for (let i = 17; i >= 12; i--) {
    if (rem & (1 << i)) rem ^= 0x1f25 << (i - 12);
  }
  return (version << 12) | rem;
}

function maskMatches(mask: number, row: number, col: number): boolean {
  switch (mask) {
    case 0:
      return (row + col) % 2 === 0;
    case 1:
      return row % 2 === 0;
    case 2:
      return col % 3 === 0;
    case 3:
      return (row + col) % 3 === 0;
    case 4:
      return (Math.floor(row / 2) + Math.floor(col / 3)) % 2 === 0;
    case 5:
      return ((row * col) % 2) + ((row * col) % 3) === 0;
    case 6:
      return (((row * col) % 2) + ((row * col) % 3)) % 2 === 0;
    case 7:
      return (((row + col) % 2) + ((row * col) % 3)) % 2 === 0;
    default:
      return false;
  }
}

function writeFormatInfo(modules: boolean[], size: number, mask: number) {
  const bits = formatBitsForMask(mask);
  const bit = (index: number) => ((bits >>> index) & 1) === 1;
  const set = (r: number, c: number, dark: boolean) => {
    modules[r * size + c] = dark;
  };

  // Top-left copy (MSB = bit 14 at (8, 0), LSB = bit 0 at (0, 8)).
  set(8, 0, bit(14));
  set(8, 1, bit(13));
  set(8, 2, bit(12));
  set(8, 3, bit(11));
  set(8, 4, bit(10));
  set(8, 5, bit(9));
  set(8, 7, bit(8));
  set(8, 8, bit(7));
  set(7, 8, bit(6));
  set(5, 8, bit(5));
  set(4, 8, bit(4));
  set(3, 8, bit(3));
  set(2, 8, bit(2));
  set(1, 8, bit(1));
  set(0, 8, bit(0));

  // Split copy (bottom-left vertical bits 14..8, top-right horizontal bits 7..0).
  for (let i = 0; i < 7; i++) {
    set(size - 1 - i, 8, bit(14 - i));
  }
  set(size - 8, 8, true); // Fixed dark module.
  for (let i = 0; i < 8; i++) {
    set(8, size - 8 + i, bit(7 - i));
  }
}

function penaltyScore(modules: boolean[], size: number): number {
  let score = 0;
  const at = (r: number, c: number) => modules[r * size + c];

  // Rule 1: 5+ consecutive same-colored modules in row or column.
  for (let r = 0; r < size; r++) {
    let runLen = 1;
    for (let c = 1; c < size; c++) {
      if (at(r, c) === at(r, c - 1)) {
        runLen++;
      } else {
        if (runLen >= 5) score += 3 + (runLen - 5);
        runLen = 1;
      }
    }
    if (runLen >= 5) score += 3 + (runLen - 5);
  }
  for (let c = 0; c < size; c++) {
    let runLen = 1;
    for (let r = 1; r < size; r++) {
      if (at(r, c) === at(r - 1, c)) {
        runLen++;
      } else {
        if (runLen >= 5) score += 3 + (runLen - 5);
        runLen = 1;
      }
    }
    if (runLen >= 5) score += 3 + (runLen - 5);
  }

  // Rule 2: 2×2 blocks of the same color.
  for (let r = 0; r < size - 1; r++) {
    for (let c = 0; c < size - 1; c++) {
      const v = at(r, c);
      if (v === at(r, c + 1) && v === at(r + 1, c) && v === at(r + 1, c + 1)) {
        score += 3;
      }
    }
  }

  // Rule 3: Finder-like 1:1:3:1:1 patterns with 4 light modules on either side.
  const isFinderLike = (line: boolean[], start: number): boolean => {
    const a =
      line[start] &&
      !line[start + 1] &&
      line[start + 2] &&
      line[start + 3] &&
      line[start + 4] &&
      !line[start + 5] &&
      line[start + 6] &&
      !line[start + 7] &&
      !line[start + 8] &&
      !line[start + 9] &&
      !line[start + 10];
    const b =
      !line[start] &&
      !line[start + 1] &&
      !line[start + 2] &&
      !line[start + 3] &&
      line[start + 4] &&
      !line[start + 5] &&
      line[start + 6] &&
      line[start + 7] &&
      line[start + 8] &&
      !line[start + 9] &&
      line[start + 10];
    return a || b;
  };
  const line = new Array<boolean>(size);
  for (let r = 0; r < size; r++) {
    for (let c = 0; c < size; c++) line[c] = at(r, c);
    for (let c = 0; c <= size - 11; c++) {
      if (isFinderLike(line, c)) score += 40;
    }
  }
  for (let c = 0; c < size; c++) {
    for (let r = 0; r < size; r++) line[r] = at(r, c);
    for (let r = 0; r <= size - 11; r++) {
      if (isFinderLike(line, r)) score += 40;
    }
  }

  // Rule 4: Dark/light balance deviation from 50%.
  let darkCount = 0;
  for (const m of modules) if (m) darkCount++;
  const percent = (darkCount * 100) / (size * size);
  const steps = Math.floor(Math.abs(percent - 50) / 5);
  score += steps * 10;

  return score;
}

/** Encodes a UTF-8 string (e.g. a Tailscale HTTPS URL) as a QR matrix. */
export function encodeQr(text: string): QrMatrix {
  const bytes = new TextEncoder().encode(text);
  const spec = selectSpec(bytes.length);
  const size = spec.version * 4 + 17;
  const dataCodewords = encodeDataCodewords(bytes, spec);
  const codewords = interleaveCodewords(dataCodewords, spec);

  const base = new Array<boolean>(size * size).fill(false);
  const isFunction = new Array<boolean>(size * size).fill(false);

  const setFn = (r: number, c: number, dark: boolean) => {
    if (r < 0 || c < 0 || r >= size || c >= size) return;
    base[r * size + c] = dark;
    isFunction[r * size + c] = true;
  };

  // Finder patterns + 1-module white separators.
  const placeFinder = (top: number, left: number) => {
    for (let dr = -1; dr <= 7; dr++) {
      for (let dc = -1; dc <= 7; dc++) {
        const r = top + dr;
        const c = left + dc;
        if (r < 0 || c < 0 || r >= size || c >= size) continue;
        const inOuter = dr >= 0 && dr <= 6 && dc >= 0 && dc <= 6;
        const onRing =
          inOuter && (dr === 0 || dr === 6 || dc === 0 || dc === 6);
        const inCore = dr >= 2 && dr <= 4 && dc >= 2 && dc <= 4;
        setFn(r, c, onRing || inCore);
      }
    }
  };
  placeFinder(0, 0);
  placeFinder(0, size - 7);
  placeFinder(size - 7, 0);

  // Timing patterns.
  for (let i = 8; i < size - 8; i++) {
    setFn(6, i, i % 2 === 0);
    setFn(i, 6, i % 2 === 0);
  }

  // Alignment patterns.
  for (const r of spec.alignment) {
    for (const c of spec.alignment) {
      if (
        (r <= 8 && c <= 8) ||
        (r <= 8 && c >= size - 9) ||
        (r >= size - 9 && c <= 8)
      ) {
        continue;
      }
      for (let dr = -2; dr <= 2; dr++) {
        for (let dc = -2; dc <= 2; dc++) {
          const dist = Math.max(Math.abs(dr), Math.abs(dc));
          setFn(r + dr, c + dc, dist === 0 || dist === 2);
        }
      }
    }
  }

  // Reserve format info + dark module.
  for (let i = 0; i <= 8; i++) {
    if (i !== 6) {
      setFn(8, i, false);
      setFn(i, 8, false);
    }
  }
  for (let i = 0; i < 8; i++) {
    setFn(8, size - 8 + i, false);
    setFn(size - 8 + i, 8, false);
  }
  setFn(size - 8, 8, true);

  // Version info for Versions 7–10.
  if (spec.version >= 7) {
    const bits = versionBits(spec.version);
    for (let i = 0; i < 18; i++) {
      const dark = ((bits >>> i) & 1) === 1;
      const a = Math.floor(i / 3);
      const b = size - 11 + (i % 3);
      setFn(a, b, dark);
      setFn(b, a, dark);
    }
  }

  // Place interleaved data + EC bits in standard upward/downward 2-column zigzag.
  let bitIndex = 0;
  const totalBits = codewords.length * 8;
  let upward = true;
  for (let rightCol = size - 1; rightCol >= 1; rightCol -= 2) {
    if (rightCol === 6) rightCol = 5; // Skip vertical timing column.
    for (let step = 0; step < size; step++) {
      const r = upward ? size - 1 - step : step;
      for (let offset = 0; offset < 2; offset++) {
        const c = rightCol - offset;
        if (isFunction[r * size + c]) continue;
        let dark = false;
        if (bitIndex < totalBits) {
          const byte = codewords[bitIndex >>> 3];
          dark = ((byte >>> (7 - (bitIndex & 7))) & 1) === 1;
          bitIndex++;
        }
        base[r * size + c] = dark;
      }
    }
    upward = !upward;
  }

  // Try all 8 masks and pick the lowest ISO penalty score.
  let bestModules = base;
  let bestPenalty = Number.POSITIVE_INFINITY;
  for (let mask = 0; mask < 8; mask++) {
    const candidate = base.slice();
    for (let r = 0; r < size; r++) {
      for (let c = 0; c < size; c++) {
        const idx = r * size + c;
        if (!isFunction[idx] && maskMatches(mask, r, c)) {
          candidate[idx] = !candidate[idx];
        }
      }
    }
    writeFormatInfo(candidate, size, mask);
    const penalty = penaltyScore(candidate, size);
    if (penalty < bestPenalty) {
      bestPenalty = penalty;
      bestModules = candidate;
    }
  }

  return { version: spec.version, size, modules: bestModules };
}

/**
 * Builds a single SVG `<path>` `d` attribute covering all dark modules in a
 * coordinate space with a 4-module quiet zone (`viewBox="0 0 size+8 size+8"`).
 */
export function qrSvgPath(
  matrix: QrMatrix,
  quietZone = 4,
): {
  viewBoxSize: number;
  path: string;
} {
  const { size, modules } = matrix;
  const parts: string[] = [];
  for (let r = 0; r < size; r++) {
    let c = 0;
    while (c < size) {
      if (!modules[r * size + c]) {
        c++;
        continue;
      }
      const start = c;
      while (c < size && modules[r * size + c]) c++;
      const run = c - start;
      parts.push(`M${start + quietZone} ${r + quietZone}h${run}v1h-${run}z`);
    }
  }
  return { viewBoxSize: size + quietZone * 2, path: parts.join("") };
}
