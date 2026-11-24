import { afterEach, describe, expect, it, vi } from "vitest";
import { cleanup, fireEvent, render, screen } from "@testing-library/react";
import { ColumnDivider } from "./ColumnDivider";

/**
 * The chat/preview divider (pockets-design-update-v3.md §8): a focusable
 * vertical separator whose keyboard and pointer gestures resize the preview
 * column, reporting live through `onResize` and committing (persist) only
 * when the interaction ends. The parent owns the clamped band — the
 * component only ever reports values inside `min`/`max` it is given.
 *
 * Event simulation notes, honestly stated: jsdom 31 does implement the
 * `PointerEvent` constructor (its init carries `clientX`, `pointerId` and
 * `button`, so `fireEvent.pointerDown/Move/Up/Cancel` reach React as real
 * pointer events), but it implements none of the pointer-capture API —
 * `setPointerCapture`/`hasPointerCapture`/`releasePointerCapture` do not
 * exist on `Element`, so each test installs `vi.fn()` stubs on the divider
 * element itself and asserts the capture round trip through them.
 */

function renderDivider(
  overrides: Partial<Parameters<typeof ColumnDivider>[0]> = {},
) {
  const props = {
    width: 600,
    min: 340,
    max: 900,
    onResize: vi.fn(),
    onCommit: vi.fn(),
    ...overrides,
  };
  const utils = render(<ColumnDivider {...props} />);
  return { props, ...utils };
}

function divider(): HTMLElement {
  return screen.getByRole("separator");
}

/** Install the pointer-capture stubs jsdom lacks; returns the spies. */
function stubCapture(el: HTMLElement) {
  const setPointerCapture = vi.fn();
  const hasPointerCapture = vi.fn(() => true);
  const releasePointerCapture = vi.fn();
  el.setPointerCapture = setPointerCapture;
  el.hasPointerCapture = hasPointerCapture;
  el.releasePointerCapture = releasePointerCapture;
  return { setPointerCapture, hasPointerCapture, releasePointerCapture };
}

/** Dispatch a cancelable keydown the way a browser would and report whether
 *  the component prevented it (the source preventDefaults tracked keys). */
function pressKey(el: HTMLElement, key: string): boolean {
  const event = new KeyboardEvent("keydown", {
    key,
    bubbles: true,
    cancelable: true,
  });
  el.dispatchEvent(event);
  return event.defaultPrevented;
}

afterEach(() => {
  cleanup();
});

describe("ColumnDivider semantics", () => {
  it("is a focusable vertical separator carrying the live width", () => {
    renderDivider();
    const el = divider();
    expect(el.getAttribute("aria-orientation")).toBe("vertical");
    expect(el.getAttribute("aria-label")).toBe("Resize preview");
    expect(el.getAttribute("tabindex")).toBe("0");
    expect(el.getAttribute("aria-valuenow")).toBe("600");
    expect(el.getAttribute("aria-valuemin")).toBe("340");
    expect(el.getAttribute("aria-valuemax")).toBe("900");
  });

  it("resets to the built-in ratio on double click", () => {
    const { props } = renderDivider();
    fireEvent.doubleClick(divider());
    expect(props.onResize).toHaveBeenCalledWith(null);
    expect(props.onCommit).toHaveBeenCalledWith(null);
  });
});

describe("ColumnDivider keyboard", () => {
  it("steps 16px per arrow, committing every press", () => {
    const { props } = renderDivider();
    // §8.3: left widens the preview (dragging left widens it too), right
    // narrows it. Each key press is its own interaction: report and commit.
    expect(pressKey(divider(), "ArrowLeft")).toBe(true);
    expect(props.onResize).toHaveBeenLastCalledWith(616);
    expect(props.onCommit).toHaveBeenLastCalledWith(616);

    expect(pressKey(divider(), "ArrowRight")).toBe(true);
    expect(props.onResize).toHaveBeenLastCalledWith(584);
    expect(props.onCommit).toHaveBeenLastCalledWith(584);
  });

  it("clamps the steps at the band and prevents the tracked keys", () => {
    // Chat keeps its 360px floor and the preview its 340px one: the band
    // comes from the parent, the divider never reports outside it.
    const high = renderDivider({
      width: 895,
      onResize: vi.fn(),
      onCommit: vi.fn(),
    });
    pressKey(divider(), "ArrowLeft"); // 895 + 16 = 911 → clamped
    expect(high.props.onResize).toHaveBeenLastCalledWith(900);
    expect(high.props.onCommit).toHaveBeenLastCalledWith(900);
    cleanup();

    const low = renderDivider({ width: 350 });
    pressKey(divider(), "ArrowRight"); // 350 − 16 = 334 → clamped
    expect(low.props.onResize).toHaveBeenLastCalledWith(340);
    expect(low.props.onCommit).toHaveBeenLastCalledWith(340);
  });

  it("jumps to the band ends on Home and End", () => {
    const { props } = renderDivider();
    expect(pressKey(divider(), "Home")).toBe(true);
    expect(props.onResize).toHaveBeenLastCalledWith(340);
    expect(props.onCommit).toHaveBeenLastCalledWith(340);

    expect(pressKey(divider(), "End")).toBe(true);
    expect(props.onResize).toHaveBeenLastCalledWith(900);
    expect(props.onCommit).toHaveBeenLastCalledWith(900);
  });

  it("leaves untracked keys to the page", () => {
    const { props } = renderDivider();
    expect(pressKey(divider(), "ArrowUp")).toBe(false);
    expect(pressKey(divider(), "PageDown")).toBe(false);
    expect(pressKey(divider(), "a")).toBe(false);
    expect(props.onResize).not.toHaveBeenCalled();
    expect(props.onCommit).not.toHaveBeenCalled();
  });
});

describe("ColumnDivider pointer drag", () => {
  it("grows the preview while dragging left and commits only on release", () => {
    const { props, rerender } = renderDivider();
    const el = divider();
    const capture = stubCapture(el);

    fireEvent.pointerDown(el, { pointerId: 1, clientX: 500, button: 0 });
    expect(capture.setPointerCapture).toHaveBeenCalledWith(1);
    expect(el.getAttribute("data-dragging")).toBe("true");
    expect(props.onCommit).not.toHaveBeenCalled();

    // Leftward movement widens the preview (§8.2 direction), reported live
    // — nothing commits while the button is down.
    fireEvent.pointerMove(el, { pointerId: 1, clientX: 480 });
    expect(props.onResize).toHaveBeenLastCalledWith(620);
    expect(props.onCommit).not.toHaveBeenCalled();
    fireEvent.pointerMove(el, { pointerId: 1, clientX: 460 });
    expect(props.onResize).toHaveBeenLastCalledWith(640);
    expect(props.onCommit).not.toHaveBeenCalled();

    // The parent applies the live width; releasing commits the current one
    // and lets go of the pointer.
    rerender(<ColumnDivider {...props} width={640} />);
    fireEvent.pointerUp(el, { pointerId: 1 });
    expect(props.onCommit).toHaveBeenCalledTimes(1);
    expect(props.onCommit).toHaveBeenCalledWith(640);
    expect(capture.releasePointerCapture).toHaveBeenCalledWith(1);
    expect(el.getAttribute("data-dragging")).toBeNull();

    // After release the drag state is gone: further moves are ignored.
    vi.mocked(props.onResize).mockClear();
    fireEvent.pointerMove(el, { pointerId: 1, clientX: 300 });
    expect(props.onResize).not.toHaveBeenCalled();
  });

  it("commits nothing for a press that never moved", () => {
    const { props } = renderDivider();
    const el = divider();
    const capture = stubCapture(el);
    fireEvent.pointerDown(el, { pointerId: 1, clientX: 500, button: 0 });
    fireEvent.pointerUp(el, { pointerId: 1 });
    // A click keeps the built-in ratio instead of pinning it in px.
    expect(props.onResize).not.toHaveBeenCalled();
    expect(props.onCommit).not.toHaveBeenCalled();
    expect(capture.releasePointerCapture).toHaveBeenCalledWith(1);
  });

  it("commits the last live width without waiting for a re-render", () => {
    const { props } = renderDivider();
    const el = divider();
    stubCapture(el);
    fireEvent.pointerDown(el, { pointerId: 1, clientX: 500, button: 0 });
    fireEvent.pointerMove(el, { pointerId: 1, clientX: 450 });
    // No rerender with width 650: the release must still commit 650.
    fireEvent.pointerUp(el, { pointerId: 1 });
    expect(props.onCommit).toHaveBeenCalledWith(650);
  });

  it("clamps a drag that runs past the band", () => {
    const { props } = renderDivider();
    const el = divider();
    stubCapture(el);
    fireEvent.pointerDown(el, { pointerId: 1, clientX: 500, button: 0 });
    fireEvent.pointerMove(el, { pointerId: 1, clientX: 40 }); // 600 + 460
    expect(props.onResize).toHaveBeenLastCalledWith(900);
    fireEvent.pointerMove(el, { pointerId: 1, clientX: 900 }); // 600 − 400
    expect(props.onResize).toHaveBeenLastCalledWith(340);
  });

  it("treats a cancelled pointer like a release", () => {
    const { props } = renderDivider();
    const el = divider();
    const capture = stubCapture(el);
    fireEvent.pointerDown(el, { pointerId: 1, clientX: 500, button: 0 });
    fireEvent.pointerMove(el, { pointerId: 1, clientX: 480 });
    vi.mocked(props.onCommit).mockClear(); // live moves must never have committed
    fireEvent.pointerCancel(el, { pointerId: 1 });
    expect(props.onCommit).toHaveBeenCalledTimes(1);
    // The newest live width, even though the parent never re-rendered
    // with it (the props here stay at 600).
    expect(props.onCommit).toHaveBeenCalledWith(620);
    expect(capture.releasePointerCapture).toHaveBeenCalledWith(1);
    expect(el.getAttribute("data-dragging")).toBeNull();
  });

  it("ignores a pointerdown that is not the left button", () => {
    const { props } = renderDivider();
    const el = divider();
    const capture = stubCapture(el);
    fireEvent.pointerDown(el, { pointerId: 1, clientX: 500, button: 2 });
    expect(capture.setPointerCapture).not.toHaveBeenCalled();
    expect(el.getAttribute("data-dragging")).toBeNull();
    fireEvent.pointerMove(el, { pointerId: 1, clientX: 400 });
    expect(props.onResize).not.toHaveBeenCalled();
    fireEvent.pointerUp(el, { pointerId: 1 });
    expect(props.onCommit).not.toHaveBeenCalled();
  });
});
