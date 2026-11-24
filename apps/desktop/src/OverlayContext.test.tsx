import { afterEach, describe, expect, it, vi } from "vitest";
import { cleanup, fireEvent, render, screen } from "@testing-library/react";
import { OverlayProvider, useOverlay, type OverlayApi } from "./OverlayContext";

/**
 * The overlay contract (pockets-design-update-v3.md §9): popups that can
 * reach the native preview register here so App hides the pane while they
 * are open. The context's job is wiring, not counting — so the tests are
 * exactly the two guarantees the module documents: outside a provider (page
 * components mounted alone in tests, a browser without a native pane)
 * `setOverlay` is a no-op that never throws, and inside one the consumer
 * receives the provider's api by identity, so calls land on App's real
 * implementation.
 */

/** Probe: records the api its consumer position resolves to, and calls
 *  `setOverlay(true)` when clicked. */
function Consumer({ seen }: { seen: { current: OverlayApi | null } }) {
  const api = useOverlay();
  seen.current = api;
  return (
    <button type="button" onClick={() => api.setOverlay(true)}>
      open overlay
    </button>
  );
}

afterEach(() => {
  cleanup();
});

describe("default overlay context", () => {
  it("is a no-op that never throws outside a provider", () => {
    const seen: { current: OverlayApi | null } = { current: null };
    expect(() => render(<Consumer seen={seen} />)).not.toThrow();
    expect(typeof seen.current!.setOverlay).toBe("function");
    expect(() => fireEvent.click(screen.getByRole("button"))).not.toThrow();
  });

  it("hands every unparented consumer the same stable no-op", () => {
    // The context default is one frozen object (OverlayContext.tsx), so the
    // api a consumer holds never churns between renders or mounts — the
    // "stable { setOverlay }" the hook's comment promises.
    const first: { current: OverlayApi | null } = { current: null };
    const second: { current: OverlayApi | null } = { current: null };
    render(<Consumer seen={first} />);
    cleanup();
    render(<Consumer seen={second} />);
    expect(second.current!.setOverlay).toBe(first.current!.setOverlay);
  });
});

describe("OverlayProvider", () => {
  it("passes the given api through by identity, calls and all", () => {
    const api: OverlayApi = { setOverlay: vi.fn() };
    const seen: { current: OverlayApi | null } = { current: null };
    render(
      <OverlayProvider value={api}>
        <Consumer seen={seen} />
      </OverlayProvider>,
    );
    // The same object, not a copy: App's counter is the single owner of
    // hide/restore, and every popup must reach that one instance.
    expect(seen.current).toBe(api);
    fireEvent.click(screen.getByRole("button"));
    expect(api.setOverlay).toHaveBeenCalledTimes(1);
    expect(api.setOverlay).toHaveBeenCalledWith(true);
  });
});
