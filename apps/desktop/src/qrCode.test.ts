import { describe, expect, it } from "vitest";
import { encodeQr, qrSvgPath } from "./qrCode";

describe("encodeQr", () => {
  it("encodes short payloads as Version 1 (21x21) with valid finder and timing patterns", () => {
    const qr = encodeQr("https://a.ts.net");
    expect(qr.version).toBe(2); // 16 bytes + 12 header bits > 16*8 -> V2 (25x25)
    expect(qr.size).toBe(25);
    expect(qr.modules).toHaveLength(25 * 25);

    const v1 = encodeQr("hello");
    expect(v1.version).toBe(1);
    expect(v1.size).toBe(21);
    const at = (r: number, c: number) => v1.modules[r * v1.size + c];

    // Top-left 7x7 finder outer ring and 3x3 core.
    for (let i = 0; i < 7; i++) {
      expect(at(0, i)).toBe(true);
      expect(at(6, i)).toBe(true);
      expect(at(i, 0)).toBe(true);
      expect(at(i, 6)).toBe(true);
    }
    for (let r = 2; r <= 4; r++) {
      for (let c = 2; c <= 4; c++) {
        expect(at(r, c)).toBe(true);
      }
    }
    // White separator around top-left finder.
    for (let i = 0; i < 8; i++) {
      expect(at(7, i)).toBe(false);
      expect(at(i, 7)).toBe(false);
    }
    // Fixed dark module at (size - 8, 8).
    expect(at(v1.size - 8, 8)).toBe(true);
    // Alternating timing pattern on row 6 and col 6.
    for (let i = 8; i < v1.size - 8; i++) {
      expect(at(6, i)).toBe(i % 2 === 0);
      expect(at(i, 6)).toBe(i % 2 === 0);
    }
  });

  it("encodes typical Tailscale HTTPS Pocket URLs cleanly and deterministically", () => {
    const url = "https://macbook-pro.tail00000.ts.net:18788";
    const first = encodeQr(url);
    const second = encodeQr(url);
    expect(first).toEqual(second);
    expect(first.version).toBe(3);
    expect(first.size).toBe(29);

    // Format info copies must match each other bit-for-bit.
    const at = (r: number, c: number) => first.modules[r * first.size + c];
    const copy1 = [
      at(8, 0),
      at(8, 1),
      at(8, 2),
      at(8, 3),
      at(8, 4),
      at(8, 5),
      at(8, 7),
      at(8, 8),
      at(7, 8),
      at(5, 8),
      at(4, 8),
      at(3, 8),
      at(2, 8),
      at(1, 8),
      at(0, 8),
    ];
    const copy2 = [
      at(first.size - 1, 8),
      at(first.size - 2, 8),
      at(first.size - 3, 8),
      at(first.size - 4, 8),
      at(first.size - 5, 8),
      at(first.size - 6, 8),
      at(first.size - 7, 8),
      at(8, first.size - 8),
      at(8, first.size - 7),
      at(8, first.size - 6),
      at(8, first.size - 5),
      at(8, first.size - 4),
      at(8, first.size - 3),
      at(8, first.size - 2),
      at(8, first.size - 1),
    ];
    expect(copy1).toEqual(copy2);

    const svg = qrSvgPath(first);
    expect(svg.viewBoxSize).toBe(37);
    expect(svg.path).toContain("M4 4h7v1h-7z");
  });

  it("scales up through Version 7+ (with version info blocks) and Version 10", () => {
    const v7 = encodeQr(
      "https://" + "a".repeat(90) + ".tail00000.ts.net:18788",
    );
    expect(v7.version).toBe(7);
    expect(v7.size).toBe(45);

    const v10 = encodeQr(
      "https://" + "a".repeat(180) + ".tail00000.ts.net:18788",
    );
    expect(v10.version).toBe(10);
    expect(v10.size).toBe(57);
  });

  it("rejects payloads longer than Version 10 capacity", () => {
    expect(() => encodeQr("a".repeat(300))).toThrow(/exceeds maximum/);
  });
});
