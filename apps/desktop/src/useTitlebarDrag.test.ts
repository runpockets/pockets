import { createElement } from "react";
import { describe, expect, it } from "vitest";
import { render, fireEvent } from "@testing-library/react";
import { useTitlebarDrag } from "./useTitlebarDrag";

function DragProbe() {
  const onMouseDown = useTitlebarDrag();
  return createElement(
    "div",
    { "data-testid": "bar", onMouseDown },
    createElement("button", { type: "button" }, "Action"),
  );
}

describe("useTitlebarDrag", () => {
  it("is a safe no-op in the browser-hosted Pockets dashboard", () => {
    const { getByTestId, getByRole } = render(createElement(DragProbe));
    expect(() => {
      fireEvent.mouseDown(getByTestId("bar"), { button: 0 });
      fireEvent.mouseDown(getByTestId("bar"), { button: 0, detail: 2 });
      fireEvent.mouseDown(getByRole("button"), { button: 0 });
    }).not.toThrow();
  });
});
