import { afterEach, beforeEach, describe, expect, it } from "vitest";
import { cleanup, render, screen } from "@testing-library/react";
import { TitleBar } from "./TitleBar";
import titlebarCss from "./TitleBar.css?raw";

/**
 * The shared column header (pockets-design-update-v3.md §1.4, §2): a named
 * toolbar with `leading` / `title` / `trailing` slots in DOM order, and the
 * traffic-light reserve that only the leftmost visible column asks for —
 * and that lands only inside Tauri, because outside no native buttons
 * overlay the row (§1.5). The geometry itself is pinned as a CSS contract so
 * every column's bottom border keeps landing on the same 52px line.
 */

/* The component reads `"__TAURI_INTERNALS__" in window` at render time,
 * exactly like App and PocketSidebar, so tests toggle the flag before
 * rendering. */
function asTauri() {
  (window as unknown as Record<string, unknown>).__TAURI_INTERNALS__ = {};
}

function asBrowser() {
  delete (window as unknown as Record<string, unknown>).__TAURI_INTERNALS__;
}

beforeEach(() => {
  asBrowser();
});

afterEach(() => {
  cleanup();
});

describe("TitleBar structure", () => {
  it("is a toolbar named for its column", () => {
    render(<TitleBar ariaLabel="Preview" title="habit-tracker" />);
    expect(screen.getByRole("toolbar", { name: "Preview" })).toBeTruthy();
  });

  it("renders leading, title and trailing slots in that DOM order", () => {
    const { container } = render(
      <TitleBar
        ariaLabel="Chat"
        leading={<button type="button">collapse</button>}
        title={<span>habit-tracker</span>}
        trailing={<button type="button">toggle</button>}
      />,
    );
    const bar = screen.getByRole("toolbar", { name: "Chat" });
    expect(Array.from(bar.children).map((child) => child.className)).toEqual([
      "titlebar-leading",
      "titlebar-title",
      "titlebar-trailing",
    ]);
    expect(bar.querySelector(".titlebar-leading")?.textContent).toBe(
      "collapse",
    );
    expect(bar.querySelector(".titlebar-title")?.textContent).toBe(
      "habit-tracker",
    );
    expect(bar.querySelector(".titlebar-trailing")?.textContent).toBe("toggle");
    expect(container.querySelectorAll(".titlebar")).toHaveLength(1);
  });

  it("omits empty slots entirely rather than rendering shells", () => {
    const { container } = render(
      <TitleBar ariaLabel="Sidebar" title="Pockets" />,
    );
    expect(container.querySelectorAll(".titlebar > *")).toHaveLength(1);
    expect(container.querySelector(".titlebar-title")).toBeTruthy();
    expect(container.querySelector(".titlebar-leading")).toBeNull();
    expect(container.querySelector(".titlebar-trailing")).toBeNull();
  });
});

describe("TitleBar traffic-light reserve", () => {
  it("adds no reserve outside Tauri even when the column asks for one", () => {
    // §1.5/§10: the reserve exists because native buttons float over the
    // webview; a plain browser window has none, so the attribute is absent
    // whatever the prop says.
    asBrowser();
    render(
      <TitleBar ariaLabel="Sidebar" title="Pockets" reserveTrafficLights />,
    );
    expect(
      screen
        .getByRole("toolbar", { name: "Sidebar" })
        .getAttribute("data-reserve-lights"),
    ).toBeNull();
  });

  it("marks the leftmost column's header inside Tauri, and unmarked otherwise", () => {
    asTauri();
    render(
      <TitleBar ariaLabel="Sidebar" title="Pockets" reserveTrafficLights />,
    );
    expect(
      screen
        .getByRole("toolbar", { name: "Sidebar" })
        .getAttribute("data-reserve-lights"),
    ).toBe("true");

    cleanup();
    render(<TitleBar ariaLabel="Chat" title="habit-tracker" />);
    expect(
      screen
        .getByRole("toolbar", { name: "Chat" })
        .getAttribute("data-reserve-lights"),
    ).toBeNull();

    cleanup();
    render(
      <TitleBar
        ariaLabel="Preview"
        title="habit-tracker"
        reserveTrafficLights={false}
      />,
    );
    expect(
      screen
        .getByRole("toolbar", { name: "Preview" })
        .getAttribute("data-reserve-lights"),
    ).toBeNull();
  });
});

// Mirrors the CSS-contract helper in PreviewPanel.test.tsx.
function topLevelRules(
  css: string,
): Array<{ selectors: string[]; body: string }> {
  const stripped = css
    .replace(/\/\*[\s\S]*?\*\//g, "")
    .replace(/@media[^{]+\{(?:[^{}]*\{[^{}]*\})*[^{}]*\}/g, "");
  const rules: Array<{ selectors: string[]; body: string }> = [];
  for (const match of stripped.matchAll(/([^{}]+)\{([^{}]*)\}/g)) {
    rules.push({
      selectors: match[1]
        .split(",")
        .map((selector) => selector.trim())
        .filter((selector) => selector !== ""),
      body: match[2],
    });
  }
  return rules;
}

function soleRuleBody(css: string, selector: string): string {
  const bodies = topLevelRules(css)
    .filter((rule) => rule.selectors.includes(selector))
    .map((rule) => rule.body);
  expect(bodies).toHaveLength(1);
  return bodies[0];
}

describe("title bar layout contract", () => {
  it("pins the 52px row and the continuous bottom border", () => {
    // §1.3/§1.4: the height is the token every column header shares, and
    // the 1px border is what reads as one line across the window.
    const bar = soleRuleBody(titlebarCss, ".titlebar");
    expect(bar).toContain("height: var(--titlebar-h);");
    expect(bar).toContain("border-bottom: 1px solid var(--border);");
    expect(bar).toContain("display: flex;");
    expect(bar).toContain("padding: 0 var(--space-12);");
    expect(bar).toContain("user-select: none;");
  });

  it("opens the traffic-light strip with a 92px left padding", () => {
    // The lights end at x ≈ 78; 92px leaves a ~14px gap. Only
    // the data attribute draws it; TitleBar decides when it is set.
    expect(
      soleRuleBody(titlebarCss, '.titlebar[data-reserve-lights="true"]'),
    ).toContain("padding-left: 92px;");
  });

  it("truncates the title slot instead of pushing the trailing controls out", () => {
    const title = soleRuleBody(titlebarCss, ".titlebar-title");
    expect(title).toContain("min-width: 0;");
    expect(title).toContain("overflow: hidden;");
    expect(title).toContain("text-overflow: ellipsis;");
    expect(title).toContain("white-space: nowrap;");
  });

  it("keeps the trailing slot pinned right and the slots flex-none", () => {
    // The controls must never shrink into the truncating title.
    expect(soleRuleBody(titlebarCss, ".titlebar-trailing")).toContain(
      "margin-left: auto;",
    );
    expect(soleRuleBody(titlebarCss, ".titlebar-trailing")).toContain(
      "flex: none;",
    );
    expect(soleRuleBody(titlebarCss, ".titlebar-leading")).toContain(
      "flex: none;",
    );
  });

  it("stays flat, token-only and free of responsive rules", () => {
    expect(titlebarCss).not.toMatch(/@media/);
    expect(titlebarCss).not.toMatch(/#[0-9a-fA-F]{3,8}\b/);
    expect(titlebarCss).not.toMatch(/box-shadow/);
    expect(titlebarCss).not.toMatch(/border-radius: [1-9]/);
  });
});
