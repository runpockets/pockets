import { describe, expect, it } from "vitest";
import { DEFAULT_LAYOUT, loadLayout, saveLayout } from "./layout";

/**
 * Unit coverage for the persisted column layout (pockets-design-update-v2.md
 * §5, v3 §8.5): the all-open default, the round trip through storage, the
 * `previewWidth` contract — anything that is not a finite positive number
 * loads as `null`, the built-in ratio — and the fact that absent, denied or
 * corrupt storage always lands back on a usable default. Every double is
 * injected, so nothing here depends on localStorage being available in the
 * test host.
 */

const KEY = "pockets.layout";

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

describe("stored layout", () => {
  it("defaults to everything open when nothing valid is stored", () => {
    expect(loadLayout(fakeStore().store)).toEqual(DEFAULT_LAYOUT);
    expect(loadLayout(fakeStore({ [KEY]: "not json" }).store)).toEqual(
      DEFAULT_LAYOUT,
    );
    expect(loadLayout(fakeStore({ [KEY]: "[1,2]" }).store)).toEqual(
      DEFAULT_LAYOUT,
    );
    expect(loadLayout(fakeStore({ [KEY]: "null" }).store)).toEqual(
      DEFAULT_LAYOUT,
    );
    expect(loadLayout(undefined)).toEqual(DEFAULT_LAYOUT);
  });

  it("reads back every combination it can write", () => {
    for (const sidebarCollapsed of [false, true]) {
      for (const previewCollapsed of [false, true]) {
        for (const previewWidth of [null, 720]) {
          const { data, store } = fakeStore();
          const state = { sidebarCollapsed, previewCollapsed, previewWidth };
          saveLayout(state, store);
          expect(JSON.parse(data.get(KEY) as string)).toEqual(state);
          expect(loadLayout(store)).toEqual(state);
        }
      }
    }
  });

  it("falls back per flag on partial or non-boolean payloads", () => {
    expect(
      loadLayout(
        fakeStore({ [KEY]: JSON.stringify({ previewCollapsed: true }) }).store,
      ),
    ).toEqual({
      sidebarCollapsed: false,
      previewCollapsed: true,
      previewWidth: null,
    });
    expect(
      loadLayout(
        fakeStore({ [KEY]: JSON.stringify({ sidebarCollapsed: "yes" }) }).store,
      ),
    ).toEqual(DEFAULT_LAYOUT);
  });

  it("never lets unavailable persistence break the layout", () => {
    const { store } = fakeStore({}, true);
    expect(() =>
      saveLayout(
        { sidebarCollapsed: true, previewCollapsed: true, previewWidth: 720 },
        store,
      ),
    ).not.toThrow();
    expect(loadLayout(store)).toEqual(DEFAULT_LAYOUT);
  });
});

describe("stored preview width", () => {
  it("loads the built-in ratio when the field is absent", () => {
    // A layout saved before v3 (or by a host that only writes the flags)
    // carries no width: the divider stays on the `1fr 1.2fr` ratio.
    expect(
      loadLayout(
        fakeStore({ [KEY]: JSON.stringify({ sidebarCollapsed: true }) }).store,
      ),
    ).toEqual({
      sidebarCollapsed: true,
      previewCollapsed: false,
      previewWidth: null,
    });
  });

  it("round-trips a valid px width through save and load", () => {
    const { data, store } = fakeStore();
    saveLayout(
      { sidebarCollapsed: false, previewCollapsed: false, previewWidth: 720 },
      store,
    );
    expect(JSON.parse(data.get(KEY) as string).previewWidth).toBe(720);
    expect(loadLayout(store).previewWidth).toBe(720);
    // An explicit reset to the ratio persists as null and loads back null.
    saveLayout(
      { sidebarCollapsed: false, previewCollapsed: false, previewWidth: null },
      store,
    );
    expect(JSON.parse(data.get(KEY) as string).previewWidth).toBeNull();
    expect(loadLayout(store).previewWidth).toBeNull();
  });

  it("rounds a persisted fractional width to whole pixels", () => {
    // A stale value written by another build need not be integral; the load
    // rounds it rather than rejecting it.
    expect(
      loadLayout(fakeStore({ [KEY]: '{"previewWidth":640.4}' }).store)
        .previewWidth,
    ).toBe(640);
    expect(
      loadLayout(fakeStore({ [KEY]: '{"previewWidth":640.6}' }).store)
        .previewWidth,
    ).toBe(641);
    // save() itself does not round: the persisted text keeps the fraction.
    const { data, store } = fakeStore();
    saveLayout(
      { sidebarCollapsed: false, previewCollapsed: false, previewWidth: 640.6 },
      store,
    );
    expect(JSON.parse(data.get(KEY) as string).previewWidth).toBe(640.6);
    expect(loadLayout(store).previewWidth).toBe(641);
  });

  it("falls back to the built-in ratio on anything not a finite positive number", () => {
    // §8.5: zero, negative, fractional nonsense aside — any non-positive,
    // non-finite or non-numeric field loads as null, never as a width.
    for (const junk of [
      "0",
      "-5",
      '"720"',
      "true",
      "false",
      "null",
      "{}",
      "1e999", // JSON.parse turns this into Infinity, still not finite
    ]) {
      expect(
        loadLayout(fakeStore({ [KEY]: `{"previewWidth":${junk}}` }).store)
          .previewWidth,
        junk,
      ).toBeNull();
    }
    // NaN never survives JSON: JSON.stringify writes null for it, so a save
    // carrying NaN is stored as null and loads back as the ratio.
    const { data, store } = fakeStore();
    saveLayout(
      {
        sidebarCollapsed: false,
        previewCollapsed: false,
        previewWidth: Number.NaN,
      },
      store,
    );
    expect(JSON.parse(data.get(KEY) as string).previewWidth).toBeNull();
    expect(loadLayout(store).previewWidth).toBeNull();
  });

  it("lands on the full default when the payload itself is corrupt", () => {
    // A width inside junk JSON is still corrupt: the whole entry defaults.
    expect(
      loadLayout(fakeStore({ [KEY]: "{previewWidth: 720" }).store),
    ).toEqual(DEFAULT_LAYOUT);
  });
});
