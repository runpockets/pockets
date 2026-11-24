import { describe, expect, it, vi } from "vitest";
import {
  applyTheme,
  loadThemeMode,
  resolveTheme,
  saveThemeMode,
  systemTheme,
  watchSystemTheme,
} from "./theme";

/**
 * Unit coverage for the theme mode (pockets-design-update-v2.md §9): the
 * stored override, what each mode resolves to, and the fact that switching
 * happens through the `data-theme` attribute alone. Every double is injected,
 * so nothing here depends on a real OS preference or on localStorage being
 * available in the test host.
 */

const KEY = "pockets.theme";

/** Storage double with only the two members the module uses. `failing`
 *  simulates a host that refuses persistence (private mode, quota). */
function fakeStore(initial: Record<string, string> = {}, failing = false) {
  const data = new Map(Object.entries(initial));
  const read = (key: string) => {
    if (failing) throw new Error("storage disabled");
    return data.has(key) ? (data.get(key) as string) : null;
  };
  const write = (key: string, value: string) => {
    if (failing) throw new Error("storage disabled");
    data.set(key, value);
  };
  // One documented cast: the module only ever calls getItem/setItem, so the
  // partial double stands in for the full DOM interface.
  const store = { getItem: read, setItem: write } as Storage;
  return { data, store };
}

/** matchMedia double for the one query the module asks about, plus a way to
 *  fire the change event the way a real MediaQueryList would. */
function fakeMatchMedia(matchesLight: boolean) {
  const listeners = new Set<(event: MediaQueryListEvent) => void>();
  const mq = {
    matches: matchesLight,
    addEventListener: (_type: string, fn: (e: MediaQueryListEvent) => void) => {
      listeners.add(fn);
    },
    removeEventListener: (
      _type: string,
      fn: (e: MediaQueryListEvent) => void,
    ) => {
      listeners.delete(fn);
    },
  } as unknown as MediaQueryList;
  return {
    listeners,
    matchMedia: (_query: string) => mq,
    emit(matches: boolean) {
      (mq as { matches: boolean }).matches = matches;
      for (const fn of listeners) fn({ matches } as MediaQueryListEvent);
    },
  };
}

describe("stored mode", () => {
  it("defaults to system when nothing valid is stored", () => {
    expect(loadThemeMode(fakeStore().store)).toBe("system");
    expect(loadThemeMode(fakeStore({ [KEY]: "sepia" }).store)).toBe("system");
    expect(loadThemeMode(undefined)).toBe("system");
  });

  it("reads back each mode it can write", () => {
    for (const mode of ["system", "light", "dark"] as const) {
      const { data, store } = fakeStore();
      saveThemeMode(mode, store);
      expect(data.get(KEY)).toBe(mode);
      expect(loadThemeMode(store)).toBe(mode);
    }
  });

  it("never lets unavailable persistence break the theme", () => {
    const { store } = fakeStore({}, true);
    expect(() => saveThemeMode("light", store)).not.toThrow();
    expect(loadThemeMode(store)).toBe("system");
  });
});

describe("resolving a mode", () => {
  it("follows the OS only in system mode", () => {
    expect(resolveTheme("system", "light")).toBe("light");
    expect(resolveTheme("system", "dark")).toBe("dark");
    expect(resolveTheme("light", "dark")).toBe("light");
    expect(resolveTheme("dark", "light")).toBe("dark");
  });

  it("asks the OS for its preference and falls back to dark", () => {
    expect(systemTheme(fakeMatchMedia(true).matchMedia)).toBe("light");
    expect(systemTheme(fakeMatchMedia(false).matchMedia)).toBe("dark");
    // No matchMedia at all (older webview, or a non-browser host): dark.
    expect(systemTheme(undefined)).toBe("dark");
    expect(
      systemTheme(() => {
        throw new Error("matchMedia blocked");
      }),
    ).toBe("dark");
  });
});

describe("applying a theme", () => {
  it("switches by the data-theme attribute and nothing else", () => {
    const element = document.createElement("div");
    expect(applyTheme("light", element, "dark")).toBe("light");
    expect(element.dataset.theme).toBe("light");
    expect(applyTheme("system", element, "dark")).toBe("dark");
    expect(element.dataset.theme).toBe("dark");
    // A manual choice survives an OS flip: the OS is only consulted in
    // system mode.
    expect(applyTheme("dark", element, "light")).toBe("dark");
    expect(element.dataset.theme).toBe("dark");
  });

  it("reports the theme when there is no element to write to", () => {
    expect(applyTheme("light", undefined, "dark")).toBe("light");
  });
});

describe("following the OS in system mode", () => {
  it("reports the new preference while subscribed and stops after unsubscribe", () => {
    const media = fakeMatchMedia(false);
    const onChange = vi.fn();
    const unsubscribe = watchSystemTheme(onChange, media.matchMedia);

    media.emit(true);
    expect(onChange).toHaveBeenCalledTimes(1);
    expect(onChange).toHaveBeenLastCalledWith("light");

    unsubscribe();
    media.emit(false);
    expect(onChange).toHaveBeenCalledTimes(1);
    expect(media.listeners.size).toBe(0);
  });

  it("is a no-op when the host cannot watch media queries", () => {
    const onChange = vi.fn();
    expect(watchSystemTheme(onChange, undefined)).toBeTypeOf("function");
    expect(
      watchSystemTheme(onChange, () => {
        throw new Error("matchMedia blocked");
      }),
    ).toBeTypeOf("function");
    expect(onChange).not.toHaveBeenCalled();
  });
});
