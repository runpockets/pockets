/**
 * Theme mode (pockets-design-update-v2.md §9). Default follows
 * `prefers-color-scheme`; a manual System / Light / Dark override is
 * persisted and switching happens by changing the `data-theme` attribute
 * only — no reload, and no component hard-codes a hex value.
 *
 * Framework-free on purpose: `main.tsx` applies the stored mode before the
 * first paint, and App owns the interactive mode state through these same
 * functions.
 */

export type ThemeMode = "system" | "light" | "dark";
export type ResolvedTheme = "light" | "dark";

const STORAGE_KEY = "pockets.theme";

const isThemeMode = (value: unknown): value is ThemeMode =>
  value === "system" || value === "light" || value === "dark";

/** The persisted override, or "system" when nothing valid is stored. */
export function loadThemeMode(
  storage: Storage | undefined = safeStorage(),
): ThemeMode {
  try {
    const raw = storage?.getItem(STORAGE_KEY);
    return isThemeMode(raw) ? raw : "system";
  } catch {
    return "system";
  }
}

/** Persist the override; a denied or absent storage never throws. */
export function saveThemeMode(
  mode: ThemeMode,
  storage: Storage | undefined = safeStorage(),
): void {
  try {
    storage?.setItem(STORAGE_KEY, mode);
  } catch {
    /* persistence is best-effort: the session still gets the theme */
  }
}

/** What the OS asks for right now. Defaults to dark without matchMedia. */
export function systemTheme(
  matchMedia:
    ((query: string) => MediaQueryList) | undefined = typeof window !==
  "undefined"
    ? window.matchMedia?.bind(window)
    : undefined,
): ResolvedTheme {
  try {
    return matchMedia?.("(prefers-color-scheme: light)").matches
      ? "light"
      : "dark";
  } catch {
    return "dark";
  }
}

/** The theme a mode resolves to — "system" follows the OS. */
export function resolveTheme(
  mode: ThemeMode,
  system: ResolvedTheme = systemTheme(),
): ResolvedTheme {
  return mode === "system" ? system : mode;
}

/** Set `data-theme` (the only mechanism themes switch by) and report it. */
export function applyTheme(
  mode: ThemeMode,
  element: HTMLElement | undefined = document.documentElement,
  system: ResolvedTheme = systemTheme(),
): ResolvedTheme {
  const theme = resolveTheme(mode, system);
  if (element) element.dataset.theme = theme;
  return theme;
}

/**
 * Re-run `onChange` while the override is "system" and the OS flips.
 * Returns the unsubscribe function; a missing addEventListener (or no
 * matchMedia at all) simply never fires.
 */
export function watchSystemTheme(
  onChange: (system: ResolvedTheme) => void,
  matchMedia:
    ((query: string) => MediaQueryList) | undefined = typeof window !==
  "undefined"
    ? window.matchMedia?.bind(window)
    : undefined,
): () => void {
  let mq: MediaQueryList | null = null;
  let listener: ((event: MediaQueryListEvent) => void) | null = null;
  try {
    mq = matchMedia?.("(prefers-color-scheme: light)") ?? null;
    if (!mq?.addEventListener) return () => {};
    listener = () => onChange(systemTheme(matchMedia));
    mq.addEventListener("change", listener);
    return () => {
      if (mq !== null && listener !== null) {
        mq.removeEventListener("change", listener);
      }
    };
  } catch {
    return () => {};
  }
}

function safeStorage(): Storage | undefined {
  try {
    return globalThis.localStorage;
  } catch {
    return undefined;
  }
}
