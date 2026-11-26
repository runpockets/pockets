/**
 * Formats a Pocket manifest `metadata.version` with a single leading `v`
 * (e.g. `0.2.0` -> `v0.2.0`, `v0.2.0` -> `v0.2.0`).
 */
export function formatVersionTag(version: string): string {
  const trimmed = version.trim();
  if (!trimmed) return "";
  const bare = trimmed.replace(/^[vV]+/, "");
  return bare ? `v${bare}` : trimmed;
}
