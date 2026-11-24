/**
 * Shell layout (pockets-design-update-v2.md §5, v3 §6/§8): the workspace is
 * always three columns that never stack — sidebar, chat, preview rail. The
 * two side columns collapse to nothing through the header toggles, and the
 * chat/preview boundary drags, so a cramped window is answered by collapsing
 * a side column or resizing a column — never by reflowing panels on top of
 * each other. Like the theme override, the choice is persisted; the default
 * is everything open with the built-in `1fr 1.2fr` chat/preview ratio.
 */

export interface LayoutState {
  sidebarCollapsed: boolean;
  previewCollapsed: boolean;
  /** Chat/preview divider position in px — the preview column's width.
      `null` (or absent, or junk) means "use the built-in ratio". */
  previewWidth: number | null;
}

const STORAGE_KEY = "pockets.layout";

/** Live-clamp bounds for the chat/preview divider (v3 §6.2), shared with
    App's live computation: the preview never goes under 340px and the chat
    never under 360px. The persisted number itself is only sanity-checked for
    being a finite positive amount — a stale width from a bigger monitor is
    clamped when used, not rejected on load. */
export const PREVIEW_MIN_WIDTH = 340;
export const CHAT_MIN_WIDTH = 360;

export const DEFAULT_LAYOUT: LayoutState = {
  sidebarCollapsed: false,
  previewCollapsed: false,
  previewWidth: null,
};

const readFlag = (value: unknown): boolean => value === true;

/** Anything that isn't a finite positive number — missing, string, zero,
    negative, NaN — loads as `null`, which means the built-in ratio.
    (v3 §8.5) */
const readWidth = (value: unknown): number | null => {
  if (typeof value !== "number" || !Number.isFinite(value) || value <= 0) {
    return null;
  }
  return Math.round(value);
};

/** The persisted choice, or the all-open default on absent/denied/corrupt storage. */
export function loadLayout(
  storage: Storage | undefined = safeStorage(),
): LayoutState {
  try {
    const raw = storage?.getItem(STORAGE_KEY);
    if (raw === null || raw === undefined) return DEFAULT_LAYOUT;
    const parsed: unknown = JSON.parse(raw);
    if (typeof parsed !== "object" || parsed === null) return DEFAULT_LAYOUT;
    const { sidebarCollapsed, previewCollapsed, previewWidth } =
      parsed as Record<string, unknown>;
    return {
      sidebarCollapsed: readFlag(sidebarCollapsed),
      previewCollapsed: readFlag(previewCollapsed),
      previewWidth: readWidth(previewWidth),
    };
  } catch {
    return DEFAULT_LAYOUT;
  }
}

/** Persist the choice; a denied or absent storage never throws. */
export function saveLayout(
  state: LayoutState,
  storage: Storage | undefined = safeStorage(),
): void {
  try {
    storage?.setItem(STORAGE_KEY, JSON.stringify(state));
  } catch {
    /* persistence is best-effort: the session still gets the layout */
  }
}

function safeStorage(): Storage | undefined {
  try {
    return globalThis.localStorage;
  } catch {
    return undefined;
  }
}
