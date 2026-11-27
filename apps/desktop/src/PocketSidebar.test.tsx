import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import {
  act,
  cleanup,
  fireEvent,
  render,
  screen,
} from "@testing-library/react";
import PocketSidebar, { type PocketSidebarProps } from "./PocketSidebar";
import { invoke } from "./api";
import type { PocketCatalog } from "./CreationPanel";
import sidebarCss from "./PocketSidebar.css?raw";

// The footer's meters are the sidebar's only native call; the real module
// would fail outside Tauri, so mock the whole module (same as the other
// component tests).
vi.mock("./api", () => ({ invoke: vi.fn() }));

const invokeMock = vi.mocked(invoke);

type Project = PocketCatalog["projects"][number];
type StatusEntry = PocketCatalog["statuses"][string];

/** Native-shaped project record; only the fields the sidebar reads matter,
 *  but the fixture stays complete so it keeps matching the real payload.
 *  `editedFrom` is passed only for lineage-imported drafts: ordinary
 *  fixtures keep the field absent, exactly like the native payload. */
function makeProject(
  id: string,
  phase: Project["phase"] = "installed",
  editedFrom?: string,
): Project {
  return {
    id,
    manifest: {
      metadata: {
        id,
        name: id,
        version: "1.0.0",
        description: "A small pocket.",
      },
      runtime: { image: "node:22-slim", architectures: ["arm64"] },
      web: { port: 3000, healthcheck: "/healthz" },
      storage: { name: `${id}-data`, path: "/data" },
    },
    phase,
    builtImageId: phase === "installed" ? "sha256:img" : null,
    files: ["Containerfile"],
    totalBytes: 2048,
    containerfile: "FROM node:22-slim",
    sourceDigest: `sha256:${"a".repeat(64)}`,
    ...(editedFrom === undefined ? {} : { editedFrom }),
  };
}

function readyStatus(
  state: "running" | "stopped",
  ok: boolean,
  message = "",
): StatusEntry {
  return {
    status: "ready",
    // A real runtime URL, deliberately rendered nowhere by the sidebar.
    value: { state, ready: ok, message, url: "http://127.0.0.1:32768/" },
  };
}

function makeCatalog(overrides: Partial<PocketCatalog> = {}): PocketCatalog {
  return {
    projects: [],
    statuses: {},
    loading: false,
    error: null,
    buildingProjectId: null,
    ...overrides,
  };
}

function renderSidebar(
  catalog: PocketCatalog,
  overrides: Partial<PocketSidebarProps> = {},
) {
  const props: PocketSidebarProps = {
    catalog,
    selectedProjectId: null,
    onSelectProject: vi.fn(),
    onNewPocket: vi.fn(),
    page: "workspace",
    onShowLive: vi.fn(),
    onShowDrafts: vi.fn(),
    onToggleCollapsed: vi.fn(),
    hostLabel: "pockets.local",
    runningCount: null,
    ...overrides,
  };
  const utils = render(<PocketSidebar {...props} />);
  return { props, ...utils };
}

function rows(container: HTMLElement): HTMLButtonElement[] {
  return Array.from(
    container.querySelectorAll<HTMLButtonElement>(".pocket-sidebar-item"),
  );
}

/** Rows that sit directly in the list (no group wrapper around them). */
function topLevelRows(container: HTMLElement): HTMLButtonElement[] {
  return Array.from(
    container.querySelectorAll<HTMLButtonElement>(
      ".pocket-sidebar-list > li > .pocket-sidebar-item, .pocket-sidebar-list > li > .pocket-sidebar-row > .pocket-sidebar-item",
    ),
  );
}

/** The row's name line text (fixtures name every project after its id). */
function nameOf(row: HTMLElement): string {
  return row.querySelector(".pocket-sidebar-name")?.textContent ?? "";
}

function navText(): string {
  return screen.getByRole("navigation", { name: "Pockets" }).textContent ?? "";
}

/* The component reads `"__TAURI_INTERNALS__" in window` at render time,
 * exactly like App, so tests toggle the flag before rendering. */
function asTauri() {
  (window as unknown as Record<string, unknown>).__TAURI_INTERNALS__ = {};
}

function asBrowser() {
  delete (window as unknown as Record<string, unknown>).__TAURI_INTERNALS__;
}

/** Snapshot shaped like the native `resource_snapshot` totals contract. */
function resourceSnapshotFixture(totals: {
  cpuPercent: number | null;
  memoryBytes: number | null;
}) {
  return {
    sampledAtMs: 1_795_064_828_687,
    pockets: [],
    totals,
    warnings: [],
  };
}

/** Drains the microtask queue (and any pending 0ms timer) inside act. */
async function settle() {
  await act(async () => {
    await vi.advanceTimersByTimeAsync(0);
  });
}

function meterFills(container: HTMLElement) {
  return container.querySelectorAll(".pocket-sidebar-meter-fill");
}

beforeEach(() => {
  invokeMock.mockReset();
  invokeMock.mockImplementation(async () => {
    throw new Error("tests reach the native backend only when asked");
  });
  asBrowser();
});

afterEach(() => {
  cleanup();
  vi.useRealTimers();
});

describe("PocketSidebar list", () => {
  it("lists every pocket with a pip-and-word status line and the Pockets heading", () => {
    const { container } = renderSidebar(
      makeCatalog({
        projects: [makeProject("habit-tracker"), makeProject("recipe-box")],
        statuses: {
          "habit-tracker": readyStatus("running", true),
          "recipe-box": readyStatus("stopped", false),
        },
      }),
    );

    expect(screen.getByRole("navigation", { name: "Pockets" })).toBeTruthy();
    expect(
      screen.getByRole("button", { name: "habit-tracker Running" }),
    ).toBeTruthy();
    expect(
      screen.getByRole("button", { name: "recipe-box Stopped" }),
    ).toBeTruthy();

    // The shared 7×7 square pip travels with the word (v2 §4).
    const [rowA, rowB] = rows(container);
    expect(rowA.querySelector(".status-pip")?.getAttribute("data-tone")).toBe(
      "ok",
    );
    expect(rowB.querySelector(".status-pip")?.getAttribute("data-tone")).toBe(
      "stopped",
    );

    // The brand mark + "Pockets" wordmark lives in the top title row; no
    // duplicate list heading or v1 header count appears below the divider.
    expect(container.querySelector(".pocket-sidebar-brand")?.textContent).toBe(
      "Pockets",
    );
    expect(container.querySelector(".pocket-sidebar-title")).toBeNull();
    expect(container.textContent).not.toContain("(2)");
  });

  it("keeps the sidebar brand wordmark static on every page", () => {
    const { container } = renderSidebar(makeCatalog(), { page: "live" });
    expect(container.querySelector(".pocket-sidebar-brand")?.textContent).toBe(
      "Pockets",
    );
    expect(container.querySelector(".pocket-sidebar-title")).toBeNull();
  });

  it("carries the honest extra detail a status record holds, and never a port", () => {
    const { container } = renderSidebar(
      makeCatalog({
        projects: [makeProject("notes")],
        statuses: {
          notes: readyStatus("running", false, "waiting for the health check"),
        },
      }),
    );

    const row = screen.getByRole("button", {
      name: /notes\s+Starting/,
    });
    expect(row.textContent).toContain("waiting for the health check");

    // Line 2 shows no ":3104"-style address (manifest.rs: a manifest never
    // supplies a host port) and never the runtime URL either.
    expect(navText()).not.toMatch(/:\d+/);
    expect(navText()).not.toContain("3000");
    expect(navText()).not.toContain("32768");
    expect(navText()).not.toContain("127.0.0.1");
    expect(navText()).not.toContain("http");

    // Shell glyphs from v1 are gone entirely (§2).
    for (const glyph of ["▸", "▾", "◐", "●", "○", "✗", "──", "["]) {
      expect(container.textContent).not.toContain(glyph);
    }
  });

  it("selects by exact id and marks only the selected row, without a marker", () => {
    const projects = [makeProject("p-1", "draft"), makeProject("p-2")];
    const { props, container } = renderSidebar(
      makeCatalog({
        projects,
        statuses: { "p-2": readyStatus("stopped", false) },
      }),
      { selectedProjectId: "p-2" },
    );

    const [rowA, rowB] = rows(container);
    expect(rowA.getAttribute("aria-current")).toBeNull();
    expect(rowB.getAttribute("aria-current")).toBe("true");
    // v2 selection is a background only: no marker element, no stripe.
    expect(container.querySelectorAll(".pocket-sidebar-marker")).toHaveLength(
      0,
    );

    fireEvent.click(rowA);
    expect(props.onSelectProject).toHaveBeenCalledWith("p-1");
  });
});

describe("PocketSidebar lineage grouping", () => {
  it("folds a draft with recorded lineage under its installed parent", () => {
    const { props, container } = renderSidebar(
      makeCatalog({
        projects: [
          makeProject("habit-tracker"),
          makeProject("habit-tracker-copy", "draft", "habit-tracker"),
        ],
        statuses: { "habit-tracker": readyStatus("running", true) },
      }),
    );

    // Folded by default: the parent row counts its drafts, and the draft
    // row is not rendered until the chevron opens the group.
    const parentRow = screen.getByRole("button", {
      name: "habit-tracker Running · 1 draft",
    });
    expect(
      screen.queryByRole("button", { name: /habit-tracker-copy/ }),
    ).toBeNull();
    const toggle = screen.getByRole("button", {
      name: "Show drafts of habit-tracker",
    });
    expect(toggle.getAttribute("aria-expanded")).toBe("false");

    fireEvent.click(toggle);
    expect(toggle.getAttribute("aria-expanded")).toBe("true");
    expect(toggle.getAttribute("aria-label")).toBe(
      "Hide drafts of habit-tracker",
    );
    // A renamed draft keeps its own name.
    const draftRow = screen.getByRole("button", {
      name: "habit-tracker-copy Draft",
    });

    // DOM order: the draft row follows the parent row, inside the group's
    // child list.
    const all: HTMLElement[] = rows(container);
    expect(all.indexOf(draftRow)).toBe(all.indexOf(parentRow) + 1);
    const group = container.querySelector(".pocket-sidebar-group");
    expect(group).toBeTruthy();
    expect(parentRow.parentElement?.parentElement).toBe(group);
    const children = group!.querySelector(":scope > .pocket-sidebar-children");
    expect(children).toBeTruthy();
    expect(children!.id).toBe(toggle.getAttribute("aria-controls"));
    expect(draftRow.parentElement?.parentElement).toBe(children);

    // A child row still selects by exact id.
    fireEvent.click(draftRow);
    expect(props.onSelectProject).toHaveBeenCalledWith("habit-tracker-copy");

    // And the chevron folds the group again.
    fireEvent.click(toggle);
    expect(container.querySelector(".pocket-sidebar-children")).toBeNull();
  });

  it("opens the group by itself while one of its drafts is selected", () => {
    renderSidebar(
      makeCatalog({
        projects: [
          makeProject("habit-tracker"),
          makeProject("habit-tracker-copy", "draft", "habit-tracker"),
        ],
      }),
      { selectedProjectId: "habit-tracker-copy" },
    );
    expect(
      screen
        .getByRole("button", { name: "Hide drafts of habit-tracker" })
        .getAttribute("aria-expanded"),
    ).toBe("true");
    expect(
      screen
        .getByRole("button", { name: "habit-tracker-copy Draft" })
        .getAttribute("aria-current"),
    ).toBe("true");
  });

  it("labels drafts that still carry the parent's name as edited copies", () => {
    const twin = (id: string) => {
      const draft = makeProject(id, "draft", "gift-wishlist");
      draft.manifest.metadata.name = "Gift Wishlist";
      return draft;
    };
    const parent = makeProject("gift-wishlist");
    parent.manifest.metadata.name = "Gift Wishlist";
    renderSidebar(
      makeCatalog({ projects: [parent, twin("copy-a"), twin("copy-b")] }),
    );
    fireEvent.click(
      screen.getByRole("button", { name: "Show drafts of Gift Wishlist" }),
    );
    expect(
      screen.getByRole("button", { name: "Edited copy 1 Draft" }),
    ).toBeTruthy();
    expect(
      screen.getByRole("button", { name: "Edited copy 2 Draft" }),
    ).toBeTruthy();
  });

  it("keeps an orphan draft a top-level row, exactly like any other draft", () => {
    // Lineage toward a deleted project, toward another draft, or recorded
    // on an installed record itself groups nothing — no wrapper appears
    // and every row sits directly in the list.
    const { container } = renderSidebar(
      makeCatalog({
        projects: [
          makeProject("habit-tracker", "installed", "a-deleted-parent"),
          makeProject("notes", "draft", "a-deleted-parent"),
          makeProject("notes-copy", "draft", "notes"),
        ],
        statuses: { "habit-tracker": readyStatus("stopped", false) },
      }),
    );

    expect(container.querySelector(".pocket-sidebar-group")).toBeNull();
    expect(container.querySelector(".pocket-sidebar-children")).toBeNull();
    const all = rows(container);
    expect(topLevelRows(container)).toEqual(all);
    expect(all.map(nameOf)).toEqual(["habit-tracker", "notes", "notes-copy"]);
  });

  it("renders a draft without the lineage field as today", () => {
    const project = makeProject("notes", "draft");
    // The fixture keeps the wire field absent, as native does for drafts
    // that were not imported as a copy.
    expect("editedFrom" in project).toBe(false);

    const { container } = renderSidebar(makeCatalog({ projects: [project] }));
    const row = screen.getByRole("button", { name: "notes Draft" });
    expect(container.querySelector(".pocket-sidebar-group")).toBeNull();
    expect(topLevelRows(container)).toEqual([row]);
  });

  it("keeps parents in catalog order and pulls grouped drafts from the top level", () => {
    const { container } = renderSidebar(
      makeCatalog({
        projects: [
          makeProject("b-copy-two", "draft", "b-tracker"),
          makeProject("a-tracker"),
          makeProject("loose-draft", "draft"),
          makeProject("b-tracker"),
          makeProject("b-copy-one", "draft", "b-tracker"),
        ],
      }),
    );
    fireEvent.click(
      screen.getByRole("button", { name: "Show drafts of b-tracker" }),
    );

    // Parents keep catalog order (a before b); the recorded drafts vanish
    // from the top level and render under b in catalog order.
    expect(rows(container).map(nameOf)).toEqual([
      "a-tracker",
      "loose-draft",
      "b-tracker",
      "b-copy-two",
      "b-copy-one",
    ]);
    expect(topLevelRows(container).map(nameOf)).toEqual([
      "a-tracker",
      "loose-draft",
      "b-tracker",
    ]);
  });
});

describe("PocketSidebar truthful status", () => {
  const cases: Array<{
    name: string;
    catalog: PocketCatalog;
    word: string;
    tone: string;
  }> = [
    {
      name: "building wins over the draft phase",
      catalog: makeCatalog({
        projects: [makeProject("notes", "draft")],
        buildingProjectId: "notes",
      }),
      word: "Building",
      tone: "warn",
    },
    {
      name: "draft before it is installed",
      catalog: makeCatalog({ projects: [makeProject("notes", "draft")] }),
      word: "Draft",
      tone: "stopped",
    },
    {
      name: "running and ready",
      catalog: makeCatalog({
        projects: [makeProject("notes")],
        statuses: { notes: readyStatus("running", true) },
      }),
      word: "Running",
      tone: "ok",
    },
    {
      name: "running but not ready",
      catalog: makeCatalog({
        projects: [makeProject("notes")],
        statuses: { notes: readyStatus("running", false) },
      }),
      word: "Starting",
      tone: "warn",
    },
    {
      name: "stopped",
      catalog: makeCatalog({
        projects: [makeProject("notes")],
        statuses: { notes: readyStatus("stopped", false) },
      }),
      word: "Stopped",
      tone: "stopped",
    },
    {
      name: "failed status read",
      catalog: makeCatalog({
        projects: [makeProject("notes")],
        statuses: { notes: { status: "error", message: "daemon offline" } },
      }),
      word: "Status unavailable",
      tone: "warn",
    },
    {
      name: "status still loading",
      catalog: makeCatalog({
        projects: [makeProject("notes")],
        statuses: { notes: { status: "loading" } },
      }),
      word: "Checking",
      tone: "warn",
    },
    {
      name: "status never requested",
      catalog: makeCatalog({ projects: [makeProject("notes")] }),
      word: "Checking",
      tone: "warn",
    },
  ];

  for (const testCase of cases) {
    it(testCase.name, () => {
      const { container } = renderSidebar(testCase.catalog);
      const row = screen.getByRole("button", {
        name: new RegExp(`notes\\s+${testCase.word}`),
      });
      expect(row.querySelector(".status-pip")?.getAttribute("data-tone")).toBe(
        testCase.tone,
      );
      // A guessed lifecycle word must never leak in alongside the honest one.
      if (testCase.word === "Checking" || testCase.word === "Draft") {
        expect(row.textContent).not.toContain("Running");
        expect(row.textContent).not.toContain("Stopped");
        expect(row.textContent).not.toContain("Building");
      }
      // Words are capitalized (v2 §4); nothing is color alone.
      expect(row.textContent).toContain(testCase.word);
      expect(container.querySelector(".pocket-sidebar-glyph")).toBeNull();
    });
  }

  it("shows the failed read's message verbatim beside the honest word", () => {
    renderSidebar(
      makeCatalog({
        projects: [makeProject("notes")],
        statuses: { notes: { status: "error", message: "daemon offline" } },
      }),
    );
    const row = screen.getByRole("button", {
      name: /notes\s+Status unavailable/,
    });
    expect(row.textContent).toContain("daemon offline");
  });
});

describe("PocketSidebar callbacks and states", () => {
  it("fires onNewPocket from the top section row without selecting anything", () => {
    const { props, container } = renderSidebar(
      makeCatalog({ projects: [makeProject("notes")] }),
      { page: "live" },
    );
    const button = screen.getByRole("button", { name: "New pocket" });
    expect(button.getAttribute("title")).toBe("New pocket ⌘N");
    expect(button.getAttribute("aria-current")).toBeNull();
    expect(button.classList.contains("pocket-sidebar-section")).toBe(true);
    expect(button.querySelector(".pocket-sidebar-section-count")).toBeNull();
    expect(
      container.querySelector(
        '.pocket-sidebar-titlebar button[aria-label="New pocket"]',
      ),
    ).toBeNull();

    fireEvent.click(button);
    expect(props.onNewPocket).toHaveBeenCalledTimes(1);
    expect(props.onSelectProject).not.toHaveBeenCalled();
    expect(screen.queryByText(/\+ new/)).toBeNull();
    expect(screen.getAllByRole("button", { name: /new pocket/i })).toHaveLength(
      1,
    );
  });

  it("marks the top section New pocket row current only when on the new-pocket workspace view", () => {
    const { rerender, props } = renderSidebar(
      makeCatalog({ projects: [makeProject("notes")] }),
      { page: "workspace", selectedProjectId: null },
    );
    expect(
      screen
        .getByRole("button", { name: "New pocket" })
        .getAttribute("aria-current"),
    ).toBe("page");

    rerender(
      <PocketSidebar {...props} page="workspace" selectedProjectId="notes" />,
    );
    expect(
      screen
        .getByRole("button", { name: "New pocket" })
        .getAttribute("aria-current"),
    ).toBeNull();

    rerender(<PocketSidebar {...props} page="live" selectedProjectId={null} />);
    expect(
      screen
        .getByRole("button", { name: "New pocket" })
        .getAttribute("aria-current"),
    ).toBeNull();
  });

  it("renders the title row with a pressed toggle that collapses the column", () => {
    const { props, container } = renderSidebar(
      makeCatalog({ projects: [makeProject("notes")] }),
    );
    const titlebar = screen.getByRole("toolbar", { name: "Sidebar" });
    expect(titlebar.classList.contains("pocket-sidebar-titlebar")).toBe(true);
    expect(container.querySelector(".pocket-sidebar")?.firstElementChild).toBe(
      titlebar,
    );
    // Outside Tauri nothing overlays the row, so the reserve stays off even
    // though the flag is passed (§10). No search control exists to render.
    expect(titlebar.getAttribute("data-reserve-lights")).toBeNull();
    expect(screen.queryByRole("button", { name: "Search" })).toBeNull();

    // The column is open — that is what the toggle's pressed state says.
    const toggle = screen.getByRole("button", { name: "Toggle sidebar" });
    expect(toggle.getAttribute("aria-pressed")).toBe("true");
    fireEvent.click(toggle);
    expect(props.onToggleCollapsed).toHaveBeenCalledTimes(1);
  });

  it("shows the empty state and keeps the data-loading flag honest", () => {
    renderSidebar(makeCatalog());
    expect(screen.getByText(/None yet — create or link a pocket/)).toBeTruthy();
    expect(
      screen
        .getByRole("navigation", { name: "Pockets" })
        .getAttribute("data-loading"),
    ).toBe("false");
  });

  it("shows a loading line while the first list request is in flight", () => {
    renderSidebar(makeCatalog({ loading: true }));
    expect(screen.getByText(/Loading pockets/)).toBeTruthy();
    expect(document.querySelectorAll(".pocket-sidebar-item")).toHaveLength(0);
    expect(
      screen
        .getByRole("navigation", { name: "Pockets" })
        .getAttribute("data-loading"),
    ).toBe("true");
  });

  it("shows the load error verbatim and no rows", () => {
    renderSidebar(
      makeCatalog({ error: "list_projects failed: permission denied" }),
    );
    expect(screen.getByText(/permission denied/)).toBeTruthy();
    expect(document.querySelectorAll(".pocket-sidebar-item")).toHaveLength(0);
  });
});

describe("PocketSidebar section rows", () => {
  it("counts each management page from the catalog phase it lists", () => {
    const { props } = renderSidebar(
      makeCatalog({
        projects: [
          makeProject("a-tracker"),
          makeProject("b-box"),
          makeProject("a-copy", "draft"),
        ],
      }),
      { page: "live" },
    );

    // Honest tallies of the same catalog the list renders (v3 §3.1).
    const live = screen.getByRole("button", { name: "Live pockets 2" });
    const drafts = screen.getByRole("button", { name: "Drafts 1" });
    expect(live.getAttribute("aria-current")).toBe("page");
    expect(drafts.getAttribute("aria-current")).toBeNull();

    fireEvent.click(live);
    fireEvent.click(drafts);
    expect(props.onShowLive).toHaveBeenCalledTimes(1);
    expect(props.onShowDrafts).toHaveBeenCalledTimes(1);
    // Section rows navigate; they never select a pocket.
    expect(props.onSelectProject).not.toHaveBeenCalled();
  });

  it("marks Drafts current on its page and nothing on the workspace", () => {
    renderSidebar(makeCatalog(), { page: "drafts" });
    expect(
      screen
        .getByRole("button", { name: "Drafts 0" })
        .getAttribute("aria-current"),
    ).toBe("page");
    expect(
      screen
        .getByRole("button", { name: "Live pockets 0" })
        .getAttribute("aria-current"),
    ).toBeNull();

    cleanup();
    renderSidebar(makeCatalog(), { page: "workspace" });
    for (const name of ["Live pockets 0", "Drafts 0"]) {
      expect(
        screen.getByRole("button", { name }).getAttribute("aria-current"),
      ).toBeNull();
    }
  });
});

describe("PocketSidebar footer meters", () => {
  it("renders no fabricated meters before the daemon reports a snapshot", async () => {
    vi.useFakeTimers({ toFake: ["setTimeout", "clearTimeout"] });
    invokeMock.mockImplementation(() => new Promise(() => {}));
    const { container } = renderSidebar(
      makeCatalog({ projects: [makeProject("notes")] }),
    );

    expect(invokeMock).toHaveBeenCalledWith("resource_snapshot");
    expect(screen.queryByText("CPU")).toBeNull();
    expect(screen.queryByText("Memory")).toBeNull();
    expect(container.querySelectorAll(".pocket-sidebar-meter")).toHaveLength(0);
    expect(
      container.querySelectorAll(".pocket-sidebar-meter-bar"),
    ).toHaveLength(0);
    expect(navText()).not.toContain("0%");
    expect(navText()).not.toContain("0 B");
    // The footer itself now always renders — the host row is static App
    // text (§3.5); only the meters stay conditional. Navigation (New
    // pocket) still works.
    expect(container.querySelectorAll(".pocket-sidebar-footer")).toHaveLength(
      1,
    );
    expect(container.querySelectorAll(".pocket-sidebar-meter")).toHaveLength(0);
    expect(screen.getByRole("button", { name: "New pocket" })).toBeTruthy();

    await act(async () => {
      await vi.advanceTimersByTimeAsync(30_000);
    });
    expect(screen.queryByText("CPU")).toBeNull();
    expect(screen.queryByText("Memory")).toBeNull();
  });

  it("shows the sampled CPU percent, its bar width and absolute memory", async () => {
    vi.useFakeTimers({ toFake: ["setTimeout", "clearTimeout"] });
    asTauri();
    invokeMock.mockImplementation(async () =>
      resourceSnapshotFixture({ cpuPercent: 4.5, memoryBytes: 1_234_567_296 }),
    );
    const { container } = renderSidebar(
      makeCatalog({ projects: [makeProject("notes")] }),
    );

    // Nothing shows before the first reply lands — no spinner, no zero.
    expect(screen.queryByText("CPU")).toBeNull();
    expect(invokeMock).toHaveBeenCalledWith("resource_snapshot");

    await settle();

    expect(screen.getByText("CPU").textContent).toBe("CPU");
    expect(screen.getByText("4.5%")).toBeTruthy();
    expect(screen.getByText("Memory")).toBeTruthy();
    expect(screen.getByText("1.1 GiB")).toBeTruthy();

    // The bar fill follows the sample; only CPU gets a bar at all.
    const fill = meterFills(container)[0] as HTMLElement;
    expect(fill.style.width).toBe("4.5%");
    expect(
      container.querySelectorAll(".pocket-sidebar-meter-bar"),
    ).toHaveLength(1);
    // The bar is decoration; the value travels as text.
    expect(
      container
        .querySelector(".pocket-sidebar-meter-bar")!
        .getAttribute("aria-hidden"),
    ).toBe("true");
  });

  it("prints a multi-core CPU total in full but clamps the bar to the track", async () => {
    vi.useFakeTimers({ toFake: ["setTimeout", "clearTimeout"] });
    asTauri();
    invokeMock.mockImplementation(async () =>
      resourceSnapshotFixture({ cpuPercent: 120.4, memoryBytes: 5e6 }),
    );
    const { container } = renderSidebar(makeCatalog());
    await settle();

    expect(screen.getByText("120.4%")).toBeTruthy();
    const fill = meterFills(container)[0] as HTMLElement;
    expect(fill.style.width).toBe("100%");
    // Memory stays absolute — no percentage, no bar — and uses IEC units
    // matching ResourceMonitor.
    expect(screen.getByText("4.8 MiB")).toBeTruthy();
    expect(screen.queryByText(/Memory.*%/)).toBeNull();
  });

  it("omits a meter whose field the backend reported as null", async () => {
    vi.useFakeTimers({ toFake: ["setTimeout", "clearTimeout"] });
    asTauri();
    invokeMock.mockImplementation(async () =>
      resourceSnapshotFixture({ cpuPercent: 4.5, memoryBytes: null }),
    );
    const { container } = renderSidebar(makeCatalog());
    await settle();

    expect(screen.getByText("4.5%")).toBeTruthy();
    expect(screen.queryByText("Memory")).toBeNull();
    expect(screen.queryByText("0 B")).toBeNull();
    expect(screen.queryByText("0 GB")).toBeNull();
    expect(
      container.querySelectorAll(".pocket-sidebar-meter-bar"),
    ).toHaveLength(1);

    cleanup();
    invokeMock.mockImplementation(async () =>
      resourceSnapshotFixture({ cpuPercent: null, memoryBytes: 1_500_000 }),
    );
    const second = renderSidebar(makeCatalog());
    await settle();

    expect(screen.queryByText("CPU")).toBeNull();
    expect(screen.queryByText("0%")).toBeNull();
    expect(screen.getByText("1.4 MiB")).toBeTruthy();
    expect(
      second.container.querySelectorAll(".pocket-sidebar-meter-bar"),
    ).toHaveLength(0);
  });

  it("formats kilobyte-scale memory with KiB instead of raw bytes and supports theme cycling", async () => {
    vi.useFakeTimers({ toFake: ["setTimeout", "clearTimeout"] });
    asTauri();
    invokeMock.mockImplementation(async () =>
      resourceSnapshotFixture({ cpuPercent: null, memoryBytes: 512 * 1024 }),
    );
    const onChangeThemeMode = vi.fn();
    renderSidebar(makeCatalog(), {
      themeMode: "system",
      onChangeThemeMode,
    });
    await settle();

    expect(screen.getByText("512.0 KiB")).toBeTruthy();
    fireEvent.click(
      screen.getByRole("button", {
        name: "Theme: System. Switch to Light",
      }),
    );
    expect(onChangeThemeMode).toHaveBeenCalledWith("light");
  });

  it("shows no meter and no fabricated value when the command rejects", async () => {
    vi.useFakeTimers({ toFake: ["setTimeout", "clearTimeout"] });
    asTauri();
    invokeMock.mockImplementation(async () => {
      throw new Error("resource_snapshot is unavailable");
    });
    const { container } = renderSidebar(makeCatalog());

    await settle();

    expect(invokeMock).toHaveBeenCalledTimes(1);
    expect(container.querySelectorAll(".pocket-sidebar-meter")).toHaveLength(0);
    expect(screen.queryByText("CPU")).toBeNull();
    expect(screen.queryByText("Memory")).toBeNull();
    expect(navText()).not.toContain("0%");
    expect(navText()).not.toContain("0 B");

    // It keeps polling at the normal cadence, still showing nothing.
    await act(async () => {
      await vi.advanceTimersByTimeAsync(5000);
    });
    expect(invokeMock).toHaveBeenCalledTimes(2);
    expect(container.querySelectorAll(".pocket-sidebar-meter")).toHaveLength(0);
  });

  it("polls on the ResourceMonitor cadence while mounted and stops on unmount", async () => {
    vi.useFakeTimers({ toFake: ["setTimeout", "clearTimeout"] });
    asTauri();
    invokeMock.mockImplementation(async () =>
      resourceSnapshotFixture({ cpuPercent: 1, memoryBytes: 2048 }),
    );
    const { unmount } = renderSidebar(makeCatalog());

    expect(invokeMock).toHaveBeenCalledTimes(1);
    await act(async () => {
      await vi.advanceTimersByTimeAsync(5000);
    });
    expect(invokeMock).toHaveBeenCalledTimes(2);
    await act(async () => {
      await vi.advanceTimersByTimeAsync(5000);
    });
    expect(invokeMock).toHaveBeenCalledTimes(3);

    unmount();
    await act(async () => {
      await vi.advanceTimersByTimeAsync(30_000);
    });
    expect(invokeMock).toHaveBeenCalledTimes(3);
  });

  it("never overlaps requests while one is still in flight", async () => {
    vi.useFakeTimers({ toFake: ["setTimeout", "clearTimeout"] });
    asTauri();
    invokeMock.mockImplementation(() => new Promise(() => {}));
    renderSidebar(makeCatalog());

    expect(invokeMock).toHaveBeenCalledTimes(1);
    await act(async () => {
      await vi.advanceTimersByTimeAsync(30_000);
    });
    expect(invokeMock).toHaveBeenCalledTimes(1);
  });

  it("attaches a CPU tooltip explaining the per-core scale when core counts are unknown", async () => {
    vi.useFakeTimers({ toFake: ["setTimeout", "clearTimeout"] });
    asTauri();
    invokeMock.mockImplementation(async () =>
      resourceSnapshotFixture({ cpuPercent: 30, memoryBytes: null }),
    );
    const { container } = renderSidebar(makeCatalog());
    await settle();

    const meter = container.querySelector(".pocket-sidebar-meter-cpu")!;
    const tooltip = screen.getByRole("tooltip", { hidden: true });
    expect(meter.getAttribute("tabindex")).toBe("0");
    expect(meter.getAttribute("aria-describedby")).toBe(tooltip.id);
    expect(tooltip.textContent).toContain(
      "Percentage of 1 core (100% = 1 full core).",
    );
    expect(tooltip.textContent).toContain(
      "Multi-core workloads can exceed 100%.",
    );
  });

  it("shows Podman and host core counts in the CPU tooltip when Podman uses a subset of host cores", async () => {
    vi.useFakeTimers({ toFake: ["setTimeout", "clearTimeout"] });
    asTauri();
    invokeMock.mockImplementation(async () =>
      resourceSnapshotFixture({ cpuPercent: 30, memoryBytes: null }),
    );
    renderSidebar(makeCatalog(), { cpuCores: 9, hostCpuCores: 18 });
    await settle();

    const tooltip = screen.getByRole("tooltip", { hidden: true });
    expect(tooltip.textContent).toContain(
      "Percentage of 1 core (100% = 1 full core).",
    );
    expect(tooltip.textContent).toContain(
      "Podman is allocated 9 of 18 host cores (3.3% of Podman, 1.7% of host).",
    );
  });

  it("shows Podman core count when Podman has all host cores or host count is absent", async () => {
    vi.useFakeTimers({ toFake: ["setTimeout", "clearTimeout"] });
    asTauri();
    invokeMock.mockImplementation(async () =>
      resourceSnapshotFixture({ cpuPercent: 30, memoryBytes: null }),
    );
    renderSidebar(makeCatalog(), { cpuCores: 6, hostCpuCores: 6 });
    await settle();

    const tooltip = screen.getByRole("tooltip", { hidden: true });
    expect(tooltip.textContent).toContain(
      "6 cores available to Podman (5% of total).",
    );

    cleanup();
    renderSidebar(makeCatalog(), { cpuCores: 1, hostCpuCores: 1 });
    await settle();
    expect(screen.getByRole("tooltip", { hidden: true }).textContent).toContain(
      "1 core available to Podman.",
    );
  });
});

describe("PocketSidebar footer host row", () => {
  it("renders the host label alone when no running count is known", () => {
    const { container } = renderSidebar(makeCatalog(), {
      hostLabel: "pockets.local",
      runningCount: null,
    });
    const host = container.querySelector(".pocket-sidebar-host");
    expect(host?.textContent).toContain("pockets.local");
    expect(host?.querySelector(".status-pip")).toBeNull();
    expect(screen.queryByText(/running/)).toBeNull();
  });

  it("shows the App's running count as an ok pip beside the host", () => {
    const { container } = renderSidebar(makeCatalog(), { runningCount: 2 });
    expect(
      container
        .querySelector(".pocket-sidebar-host .status-pip")!
        .getAttribute("data-tone"),
    ).toBe("ok");
    expect(screen.getByText("2 running")).toBeTruthy();
  });

  it("marks zero running as stopped, never as absent", () => {
    const { container } = renderSidebar(makeCatalog(), { runningCount: 0 });
    expect(
      container
        .querySelector(".pocket-sidebar-host .status-pip")!
        .getAttribute("data-tone"),
    ).toBe("stopped");
    expect(screen.getByText("0 running")).toBeTruthy();
  });

  it("renders outside Tauri, where only the meters are absent", () => {
    // §3.5: the row is static text plus the App's trustworthy count — no
    // native sample involved, so the browser fallback still shows it.
    const { container } = renderSidebar(makeCatalog());
    expect(container.querySelectorAll(".pocket-sidebar-host")).toHaveLength(1);
    expect(container.querySelectorAll(".pocket-sidebar-meter")).toHaveLength(0);
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

describe("pocket sidebar layout contract", () => {
  it("lays the column out per §3 and pins the footer by margin-top: auto", () => {
    const sidebar = soleRuleBody(sidebarCss, ".pocket-sidebar");
    expect(sidebar).toContain("display: flex;");
    expect(sidebar).toContain("flex-direction: column;");
    expect(sidebar).toContain("background: var(--chrome);");
    expect(sidebar).toContain("height: 100%;");
    // The 52px title row is the column's top edge: the container carries no
    // padding or gap that could indent it or drop its border below the
    // neighbouring columns' (§1). The inset moved into the body.
    expect(sidebar).not.toContain("padding");
    expect(sidebar).not.toContain("gap");
    expect(soleRuleBody(sidebarCss, ".pocket-sidebar-body")).toContain(
      "padding: var(--space-8) var(--space-10) 0;",
    );
    // The 248px track and the right border are the `.sidebar` column in
    // styles.css; the component must not re-declare (or drift from) them.
    expect(sidebar).not.toMatch(/width: 2\d\dpx/);
    expect(sidebar).not.toContain("border-right");
    // Only the list scrolls; the footer rides the bottom.
    expect(soleRuleBody(sidebarCss, ".pocket-sidebar-list")).toContain(
      "overflow-y: auto;",
    );
    const footer = soleRuleBody(sidebarCss, ".pocket-sidebar-footer");
    expect(footer).toContain("margin-top: auto;");
    expect(footer).toContain("border-top: 1px solid var(--border);");
    expect(footer).toContain(
      "padding: var(--space-12) var(--space-10) var(--space-10);",
    );
    expect(footer).toContain("font-size: 12px;");
  });

  it("keeps the title row on the column's own chrome background", () => {
    // TitleBar's default is `--bg`; the sidebar column has always been
    // `--chrome` (v3 §1.4), so the override must out-specify it.
    expect(soleRuleBody(sidebarCss, ".pocket-sidebar .titlebar")).toContain(
      "background: var(--chrome);",
    );
  });

  it("draws the section rows as compact single-line navigation rows", () => {
    const section = soleRuleBody(
      sidebarCss,
      ".pocket-sidebar-body .pocket-sidebar-section",
    );
    expect(section).toContain("min-height: 34px;");
    expect(section).toContain("padding: var(--space-6) var(--space-10);");
    expect(
      soleRuleBody(sidebarCss, '.pocket-sidebar-section[aria-current="page"]'),
    ).toContain("background: var(--selected);");
    // The 40px New pocket button is gone (§3.4), not restyled.
    expect(sidebarCss).not.toContain(".pocket-sidebar-new");
  });

  it("draws the divider as a 1px border line with 8px vertical margins", () => {
    const divider = soleRuleBody(sidebarCss, ".pocket-sidebar-divider");
    expect(divider).toContain("height: 1px;");
    expect(divider).toContain("margin: var(--space-8) 0;");
    expect(divider).toContain("background: var(--border);");
  });

  it("lays the host row out label-left, pip-right, in --text-dim", () => {
    const host = soleRuleBody(sidebarCss, ".pocket-sidebar-host");
    expect(host).toContain("justify-content: space-between;");
    expect(host).toContain("color: var(--text-dim);");
  });

  it("styles the sidebar brand mark + wordmark in the title row", () => {
    const brand = soleRuleBody(sidebarCss, ".pocket-sidebar-brand");
    expect(brand).toContain("font-weight: 600;");
    expect(brand).toContain("color: var(--text);");
    // The old duplicate list heading and uppercase metadata label are retired.
    expect(sidebarCss).not.toContain(".pocket-sidebar-title");
    expect(sidebarCss).not.toContain(".pocket-sidebar-label");
  });

  it("pins the 44px two-line rows and the background-only selection", () => {
    const item = soleRuleBody(
      sidebarCss,
      ".pocket-sidebar-body .pocket-sidebar-item",
    );
    expect(item).toContain("min-height: 44px;");
    expect(item).toContain("padding: var(--space-8) var(--space-10);");
    expect(item).toContain("gap: 1px;");
    expect(
      soleRuleBody(sidebarCss, '.pocket-sidebar-item[aria-current="true"]'),
    ).toContain("background: var(--selected);");
    expect(
      soleRuleBody(
        sidebarCss,
        '.pocket-sidebar-item[aria-current="true"] .pocket-sidebar-name',
      ),
    ).toContain("font-weight: 500;");
  });

  it("indents lineage drafts under their parent on the spacing scale", () => {
    // The group wrapper carries no geometry of its own: the only layout
    // change for a grouped row is the deeper left padding, on the existing
    // spacing scale.
    const child = soleRuleBody(
      sidebarCss,
      ".pocket-sidebar-body .pocket-sidebar-children .pocket-sidebar-item",
    );
    expect(child).toContain("padding-left: var(--space-24);");
  });

  it("pins the honest meter geometry: 4px bar, 5px gap, no memory bar styling", () => {
    expect(soleRuleBody(sidebarCss, ".pocket-sidebar-meter")).toContain(
      "gap: 5px;",
    );
    const bar = soleRuleBody(sidebarCss, ".pocket-sidebar-meter-bar");
    expect(bar).toContain("height: 4px;");
    expect(bar).toContain("background: var(--selected);");
    expect(soleRuleBody(sidebarCss, ".pocket-sidebar-meter-fill")).toContain(
      "background: var(--text-dim);",
    );
    expect(soleRuleBody(sidebarCss, ".pocket-sidebar-meter-value")).toContain(
      "color: var(--text-2);",
    );
  });

  it("stays flat, token-only, square and free of stacked layouts", () => {
    // The shell is columns only at every width (the header toggles collapse
    // the side columns): this stylesheet must not carry a responsive block.
    expect(sidebarCss).not.toMatch(/@media/);
    // Flat, token-only contract: no raw hex colors, no shadows.
    expect(sidebarCss).not.toMatch(/#[0-9a-fA-F]{3,8}\b/);
    expect(sidebarCss).not.toMatch(/box-shadow/);
    // v2 §4 shape rule: radius 0 everywhere except the conversation.
    expect(sidebarCss).not.toMatch(/border-radius: [1-9]/);
    expect(sidebarCss).not.toMatch(/var\(--radius\)/);
  });
});
