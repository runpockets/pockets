/**
 * Shared IEC binary byte formatter (1024-based): 1 KiB = 1024 B.
 * Used across the sidebar meters, resource monitor, and snapshot metadata so
 * the same byte count always reads identically everywhere in the app.
 */
export function formatBytes(bytes: number): string {
  if (!Number.isFinite(bytes) || bytes < 0) return "unknown size";
  const units = ["B", "KiB", "MiB", "GiB", "TiB"];
  let value = bytes;
  let unit = 0;
  while (value >= 1024 && unit < units.length - 1) {
    value /= 1024;
    unit += 1;
  }
  return `${unit === 0 ? Math.round(value) : value.toFixed(1)} ${units[unit]}`;
}

/** "42.5%" / "9%" style; one decimal only when it carries information. */
export function formatPercent(value: number): string {
  const rounded = Math.round(value * 10) / 10;
  const text = Number.isInteger(rounded) ? String(rounded) : rounded.toFixed(1);
  return `${text}%`;
}
