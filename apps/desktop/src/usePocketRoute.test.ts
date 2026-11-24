import { afterEach, describe, expect, it, vi } from "vitest";
import { act, cleanup, renderHook } from "@testing-library/react";
import { readPocketRoute, usePocketRoute } from "./usePocketRoute";

// jsdom provides window.location; the hook only ever touches `hash`, so no
// Tauri runtime or router package is involved.

afterEach(() => {
  cleanup();
  window.location.hash = "";
});

describe("readPocketRoute", () => {
  it("parses the new-pocket workspace route", () => {
    expect(readPocketRoute("#/")).toEqual({
      page: "workspace",
      projectId: null,
    });
  });

  it("parses the live and drafts management routes", () => {
    expect(readPocketRoute("#/live")).toEqual({ page: "live" });
    expect(readPocketRoute("#/drafts")).toEqual({ page: "drafts" });
  });

  it("lands the legacy all-pockets hash on the live page", () => {
    expect(readPocketRoute("#/pockets")).toEqual({ page: "live" });
  });

  it("parses the selected-pocket workspace route", () => {
    expect(readPocketRoute("#/p/p-1")).toEqual({
      page: "workspace",
      projectId: "p-1",
    });
    expect(readPocketRoute("#/p/my%20pocket")).toEqual({
      page: "workspace",
      projectId: "my pocket",
    });
  });

  it("round-trips opaque project ids through encodeURIComponent", () => {
    for (const id of ["plain", "with space", "a/b", "q?x=1#2", "%41", "../x"]) {
      expect(readPocketRoute("#/p/" + encodeURIComponent(id))).toEqual({
        page: "workspace",
        projectId: id,
      });
    }
  });

  it("falls back to the safe workspace for malformed or extra-segment hashes", () => {
    const fallback = { page: "workspace", projectId: null } as const;
    for (const hash of [
      "",
      "#",
      "#/p",
      "#/p/",
      "#/p/%",
      "#/p/%zz",
      "#/p/%E0%A4%A",
      "#/p/a/b",
      "#/pockets/extra",
      "#/pockets/",
      "#/else",
      "#/pockets%2Fx",
      "no-slash/p-1",
    ]) {
      expect(readPocketRoute(hash), hash).toEqual(fallback);
    }
  });
});

describe("usePocketRoute", () => {
  it("reads the initial route from the current hash", () => {
    window.location.hash = "#/p/p-1";
    const { result } = renderHook(() => usePocketRoute());
    expect(result.current.route).toEqual({
      page: "workspace",
      projectId: "p-1",
    });
  });

  it("updates the route and the hash synchronously on navigation", () => {
    const { result } = renderHook(() => usePocketRoute());

    act(() => result.current.selectProject("my pocket"));
    expect(result.current.route).toEqual({
      page: "workspace",
      projectId: "my pocket",
    });
    expect(window.location.hash).toBe("#/p/my%20pocket");

    act(() => result.current.showLivePockets());
    expect(result.current.route).toEqual({ page: "live" });
    expect(window.location.hash).toBe("#/live");

    act(() => result.current.showDrafts());
    expect(result.current.route).toEqual({ page: "drafts" });
    expect(window.location.hash).toBe("#/drafts");

    act(() => result.current.newPocket());
    expect(result.current.route).toEqual({
      page: "workspace",
      projectId: null,
    });
    expect(window.location.hash).toBe("#/");
  });

  it("ignores an empty project id", () => {
    const { result } = renderHook(() => usePocketRoute());
    act(() => result.current.selectProject(""));
    expect(result.current.route).toEqual({
      page: "workspace",
      projectId: null,
    });
    expect(window.location.hash).not.toBe("#/p/");
  });

  it("keeps navigation callbacks stable across renders and route changes", () => {
    const { result, rerender } = renderHook(() => usePocketRoute());
    const first = result.current;

    rerender();
    act(() => result.current.selectProject("p-1"));

    expect(result.current.selectProject).toBe(first.selectProject);
    expect(result.current.newPocket).toBe(first.newPocket);
    expect(result.current.showLivePockets).toBe(first.showLivePockets);
    expect(result.current.showDrafts).toBe(first.showDrafts);
  });

  it("follows hashchange events, as fired by back/forward navigation", () => {
    const { result } = renderHook(() => usePocketRoute());
    expect(result.current.route).toEqual({
      page: "workspace",
      projectId: null,
    });

    act(() => {
      window.location.hash = "#/drafts";
      window.dispatchEvent(new Event("hashchange"));
    });
    expect(result.current.route).toEqual({ page: "drafts" });

    // A malformed external hash still resolves to the safe workspace.
    act(() => {
      window.location.hash = "#/p/%";
      window.dispatchEvent(new Event("hashchange"));
    });
    expect(result.current.route).toEqual({
      page: "workspace",
      projectId: null,
    });
  });

  it("removes the hashchange listener on unmount", () => {
    const removeSpy = vi.spyOn(window, "removeEventListener");
    const { result, unmount } = renderHook(() => usePocketRoute());
    act(() => result.current.showLivePockets());
    unmount();

    const calls = removeSpy.mock.calls.filter(
      ([type]) => type === "hashchange",
    );
    expect(calls).toHaveLength(1);
    removeSpy.mockRestore();

    // A late event after unmount must not throw or change the last route.
    act(() => {
      window.location.hash = "#/";
      window.dispatchEvent(new Event("hashchange"));
    });
    expect(result.current.route).toEqual({ page: "live" });
  });
});
