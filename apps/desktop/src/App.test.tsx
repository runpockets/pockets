import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import {
  act,
  cleanup,
  fireEvent,
  render,
  renderHook,
  screen,
  within,
} from "@testing-library/react";
import { invoke } from "./api";
import App, { usePreviewPane } from "./App";
import type { PreviewBounds } from "./PreviewPanel";
import type { PocketCatalog } from "./CreationPanel";
import type { ProjectPreview } from "./useProjectPreview";

// The shell and the hook only ever talk to native through `invoke`; the real
// module would fail outside Tauri, so mock the whole module (same convention
// as PreviewPanel.test.tsx).
vi.mock("./api", () => ({
  invoke: vi.fn(),
  listen: vi.fn(() => Promise.resolve(() => {})),
  registerFolderPicker: vi.fn(),
  registerAuthRequired: vi.fn(),
  authenticateSession: vi.fn(),
}));

// Values the mocked child components need at module-factory time (factories
// are hoisted above the consts below).
const fixtures = vi.hoisted(() => ({
  session: "3f2a9c8e51b64d7fa0c1d2e3f4a5b6c7",
  savedProjectId: "saved-project-id",
  /** What the mocked "emit session" button reports, letting one test drive
   *  a bind → rebind (fork handover) sequence through the real callback. */
  nextSessionId: null as string | null,
  /** The origin the mocked "emit session" button reports with. `undefined`
   *  keeps every existing test's bind origin-less — exactly what the send-
   *  order fork tests drive — while `loadBind`/`sendBind` tag theirs. */
  nextSessionOrigin: undefined as "load" | "send" | undefined,
  creationProps: null as Record<string, unknown> | null,
  previewProps: null as { preview: ProjectPreview } | null,
  /** What the mocked window module reports for full screen, and every
   *  native resize listener registered (Tauri 2 has no full-screen event;
   *  useFullscreen re-reads `isFullscreen()` after each resize). */
  fullscreen: false,
  resizeListeners: [] as Array<() => void>,
}));

// The new shell children (sidebar list, pocket chat, catalog grid) and the
// preview panel are replaced by prop-capturing stand-ins: this suite pins
// how App coordinates them (route, title-row props, selection, preview
// session), while each component's own suite pins its internals.
vi.mock("./CreationPanel", async () => {
  const { createElement } = await import("react");
  return {
    default: (props: Record<string, unknown>) => {
      fixtures.creationProps = props;
      return createElement("div", { "data-testid": "creation-panel" });
    },
  };
});

vi.mock("./PocketSidebar", async () => {
  const { createElement } = await import("react");
  return {
    default: (props: {
      catalog: {
        loading: boolean;
        projects: Array<{
          id: string;
          manifest: { metadata: { name: string } };
        }>;
      };
      selectedProjectId: string | null;
      onSelectProject: (id: string) => void;
      onNewPocket: () => void;
      page: "workspace" | "live" | "drafts";
      onShowLive: () => void;
      onShowDrafts: () => void;
      onToggleCollapsed: () => void;
      hostLabel: string;
      runningCount: number | null;
    }) =>
      createElement(
        "div",
        {
          "data-testid": "pocket-sidebar",
          "data-loading": String(props.catalog.loading),
          // v3 §3: the sidebar is now also the page navigator and the host
          // footer. The mock renders exactly what App passes so App-level
          // assertions can read it: the active route and the trustworthy
          // running count ("none" when no status is known, exactly when
          // the real footer shows the host alone).
          "data-page": props.page,
          "data-running-count": String(props.runningCount ?? "none"),
        },
        createElement("span", { className: "pocket-sidebar-brand" }, "Pockets"),
        // The section rows that replaced the nav rail (v3 §3.1).
        createElement(
          "button",
          { type: "button", onClick: props.onShowLive },
          "Live pockets",
        ),
        createElement(
          "button",
          { type: "button", onClick: props.onShowDrafts },
          "Drafts",
        ),
        // The footer's host word (v3 §3.5); the count itself is the data
        // attribute above.
        createElement(
          "span",
          { className: "pocket-sidebar-host-label" },
          props.hostLabel,
        ),
        props.catalog.projects.map((project) =>
          createElement(
            "button",
            {
              key: project.id,
              type: "button",
              "aria-current":
                project.id === props.selectedProjectId ? "true" : undefined,
              onClick: () => props.onSelectProject(project.id),
            },
            project.manifest.metadata.name,
          ),
        ),
        createElement(
          "button",
          { type: "button", onClick: props.onNewPocket },
          "New pocket",
        ),
        // The column's own title-row toggle (v3 §3): the real sidebar
        // carries the pressed state (asserted in PocketSidebar.test.tsx);
        // App.test pins that the toggle exists and what collapsing does.
        createElement(
          "button",
          {
            type: "button",
            "aria-label": "Toggle sidebar",
            onClick: props.onToggleCollapsed,
          },
          "⌜⌝",
        ),
      ),
  };
});

vi.mock("./PreviewPanel", async () => {
  const { createElement } = await import("react");
  return {
    default: (props: {
      preview: ProjectPreview;
      onTogglePreview?: () => void;
      overlayOpen?: boolean;
      pocketName?: string | null;
      snapshotUrl?: string | null;
    }) => {
      fixtures.previewProps = props;
      return createElement(
        "div",
        {
          "data-testid": "preview-panel",
          "data-session": props.preview.projectId ?? "none",
          // The overlay stand-in signal (v3 §9): App hides the native pane
          // while a popup is open and tells the panel to show its stand-in.
          "data-overlay-open": String(props.overlayOpen === true),
        },
        // The preview's own title row owns the collapse toggle (v3 §6.1).
        props.onTogglePreview
          ? createElement(
              "button",
              {
                type: "button",
                "aria-label": "Toggle preview",
                onClick: props.onTogglePreview,
              },
              "⌟⌞",
            )
          : null,
      );
    },
  };
});

const invokeMock = vi.mocked(invoke);

/** Session ids in the shape native `validate_session_id` accepts. */
const SESSION = fixtures.session;
const OTHER_SESSION = "a1b2c3d4e5f60718293a4b5c6d7e8f90";

const BOUNDS: PreviewBounds = { x: 120, y: 24.5, width: 480, height: 320 };

/** A bare ⌘ press on the window, exactly what App's page-shortcut listener
 *  (v3 §3: ⌘1 workspace, ⌘2 Live pockets, ⌘3 Drafts, ⌘N new pocket) reads. */
function pressMeta(
  key: string,
  modifiers: { ctrlKey?: boolean; altKey?: boolean } = {},
) {
  fireEvent.keyDown(window, { key, metaKey: true, ...modifiers });
}

beforeEach(() => {
  // The real Tauri `invoke` always returns a promise; keep the mock faithful
  // so `.catch()` chains in the hook behave as they do in production.
  // Rejection tests override this per-test.
  invokeMock.mockImplementation(async () => true);
  // The shell persists its collapse and preview-width choices; keep every
  // test's layout deterministic regardless of what an earlier test stored.
  localStorage.removeItem("pockets.layout");
  window.location.hash = "";
});

afterEach(() => {
  cleanup();
  invokeMock.mockReset();
  vi.unstubAllGlobals();
  delete (window as unknown as Record<string, unknown>).__TAURI_INTERNALS__;
  vi.resetModules();
  window.location.hash = "";
  // Restore the window width a divider-geometry test may have stubbed.
  Object.defineProperty(window, "innerWidth", {
    configurable: true,
    writable: true,
    value: 1024,
  });
  fixtures.fullscreen = false;
  fixtures.resizeListeners.length = 0;
});

describe("usePreviewPane bounds", () => {
  it("routes project preview bounds and teardown without an AI session", () => {
    const { result, unmount } = renderHook(() => usePreviewPane(SESSION, true));
    result.current(BOUNDS);
    expect(invokeMock).toHaveBeenCalledWith("project_preview_set_bounds", {
      projectId: SESSION,
      ...BOUNDS,
    });
    unmount();
    expect(invokeMock).toHaveBeenCalledWith("project_preview_hide", {
      projectId: SESSION,
    });
    expect(
      invokeMock.mock.calls.some(([command]) => command.startsWith("ai_")),
    ).toBe(false);
  });

  it("forwards each reported rect to project_preview_set_bounds unchanged", () => {
    const { result } = renderHook(
      ({ projectId }: { projectId: string | null }) =>
        usePreviewPane(projectId, true),
      { initialProps: { projectId: SESSION } },
    );

    // Rendering alone never invokes: the pane appears when the panel first
    // reports a rect, not when the hook mounts.
    expect(invokeMock).not.toHaveBeenCalled();

    result.current(BOUNDS);
    expect(invokeMock).toHaveBeenCalledWith("project_preview_set_bounds", {
      projectId: SESSION,
      x: BOUNDS.x,
      y: BOUNDS.y,
      width: BOUNDS.width,
      height: BOUNDS.height,
    });
    // The native command takes the rect only — no URL, port, or path ever
    // leaves the frontend (docs/live-preview.md Boundaries).
    const args = invokeMock.mock.calls[0][1] as Record<string, unknown>;
    expect(Object.keys(args).sort()).toEqual([
      "height",
      "projectId",
      "width",
      "x",
      "y",
    ]);

    // Every report is forwarded unthrottled (the spike measured a ~1 ms
    // native move round trip).
    invokeMock.mockClear();
    result.current({ ...BOUNDS, y: 30 });
    expect(invokeMock).toHaveBeenCalledTimes(1);
  });

  it("addresses reports to the current session after a switch", () => {
    const { result, rerender } = renderHook(
      ({ projectId }: { projectId: string | null }) =>
        usePreviewPane(projectId, true),
      { initialProps: { projectId: SESSION } },
    );
    rerender({ projectId: OTHER_SESSION });
    invokeMock.mockClear(); // drop the hide-from-cleanup for the old session

    result.current(BOUNDS);
    expect(invokeMock).toHaveBeenCalledWith("project_preview_set_bounds", {
      projectId: OTHER_SESSION,
      ...BOUNDS,
    });
  });

  it("is a no-op without a session", () => {
    const { result } = renderHook(() => usePreviewPane(null, true));
    result.current(BOUNDS);
    expect(invokeMock).not.toHaveBeenCalled();
  });

  it("swallows native rejections: placement is advisory", async () => {
    invokeMock.mockRejectedValue(new Error("pane unavailable"));
    const { result } = renderHook(() => usePreviewPane(SESSION, true));
    expect(() => result.current(BOUNDS)).not.toThrow();
    // Let the rejected round trip settle so an unhandled rejection would
    // surface before the test ends.
    await Promise.resolve();
    await Promise.resolve();
  });
});

describe("usePreviewPane overlay visibility", () => {
  // v3 §9: an in-app popup that can reach the preview area flips `visible`
  // off; the pane steps aside and comes back on its own — the session
  // never rebinds, because an overlay closing is not a session switch.
  it("hides the pane while an overlay is open and restores the latest rect", () => {
    const { result, rerender } = renderHook(
      ({ visible }: { visible: boolean }) => usePreviewPane(SESSION, visible),
      { initialProps: { visible: true } },
    );

    result.current(BOUNDS);
    expect(invokeMock).toHaveBeenCalledWith("project_preview_set_bounds", {
      projectId: SESSION,
      ...BOUNDS,
    });

    // The overlay opens: the pane hides exactly like a collapse would.
    rerender({ visible: false });
    expect(invokeMock).toHaveBeenCalledWith("project_preview_hide", {
      projectId: SESSION,
    });

    invokeMock.mockClear();
    // Reports the panel keeps sending while hidden are stored, not sent:
    // the native view is not there to move.
    result.current({ ...BOUNDS, y: 96 });
    expect(invokeMock).not.toHaveBeenCalled();

    // The overlay closes: the pane comes back at the freshest rect the
    // panel reported — one report, same session, nothing rebound.
    rerender({ visible: true });
    expect(invokeMock).toHaveBeenCalledTimes(1);
    expect(invokeMock).toHaveBeenCalledWith("project_preview_set_bounds", {
      projectId: SESSION,
      x: BOUNDS.x,
      y: 96,
      width: BOUNDS.width,
      height: BOUNDS.height,
    });
    expect(invokeMock).not.toHaveBeenCalledWith("project_preview_hide", {
      projectId: SESSION,
    });
  });

  it("hides the pane when the app mounts while an overlay is already open", () => {
    renderHook(() => usePreviewPane(SESSION, false));
    expect(invokeMock).toHaveBeenCalledWith("project_preview_hide", {
      projectId: SESSION,
    });
    expect(invokeMock).not.toHaveBeenCalledWith(
      "project_preview_set_bounds",
      expect.anything(),
    );
  });

  it("never invokes for a hidden pane without a session", () => {
    const { result } = renderHook(() => usePreviewPane(null, false));
    result.current(BOUNDS);
    expect(invokeMock).not.toHaveBeenCalled();
  });
});

describe("usePreviewPane teardown", () => {
  it("hides the pane on unmount", () => {
    const { unmount } = renderHook(() => usePreviewPane(SESSION, true));
    invokeMock.mockClear();
    unmount();
    expect(invokeMock).toHaveBeenCalledTimes(1);
    expect(invokeMock).toHaveBeenCalledWith("project_preview_hide", {
      projectId: SESSION,
    });
  });

  it("hides the previous session's pane on a session switch", () => {
    const { rerender, unmount } = renderHook(
      ({ projectId }: { projectId: string | null }) =>
        usePreviewPane(projectId, true),
      { initialProps: { projectId: SESSION } },
    );
    rerender({ projectId: OTHER_SESSION });
    expect(invokeMock).toHaveBeenCalledWith("project_preview_hide", {
      projectId: SESSION,
    });
    // The new session's teardown must not fire yet...
    expect(invokeMock).not.toHaveBeenCalledWith("project_preview_hide", {
      projectId: OTHER_SESSION,
    });
    invokeMock.mockClear();
    unmount();
    expect(invokeMock).toHaveBeenCalledWith("project_preview_hide", {
      projectId: OTHER_SESSION,
    });
  });

  it("never hides without a session", () => {
    const { unmount } = renderHook(() => usePreviewPane(null, true));
    unmount();
    expect(invokeMock).not.toHaveBeenCalled();
  });

  it("swallows hide rejections", async () => {
    invokeMock.mockRejectedValue(new Error("teardown raced a stop"));
    const { unmount } = renderHook(() => usePreviewPane(SESSION, true));
    expect(() => unmount()).not.toThrow();
    await Promise.resolve();
    await Promise.resolve();
  });
});

/* ------------------------------------------------------------------ shell */

type CatalogProject = PocketCatalog["projects"][number];
type StatusEntry = PocketCatalog["statuses"][string];

function project(id: string, name: string): CatalogProject {
  return {
    id,
    manifest: {
      metadata: { id, name, version: "0.1.0", description: "" },
      runtime: {
        image: `localhost/pockets/${id}:0.1.0`,
        architectures: ["arm64"],
      },
      web: { port: 3000, healthcheck: "/healthz" },
      storage: { name: `${id}-data`, path: "/data" },
    },
    phase: "installed",
    builtImageId: "sha256:abc",
    files: [],
    totalBytes: 0,
    containerfile: "FROM scratch",
    sourceDigest: "sha256:def",
  };
}

const RUNNING: StatusEntry = {
  status: "ready",
  value: { state: "running", url: null, message: "", ready: true },
};

const STOPPED: StatusEntry = {
  status: "ready",
  value: { state: "stopped", url: null, message: "", ready: false },
};

/** A trustworthy snapshot: two projects, exactly one running read. */
function catalogFixture(projects: CatalogProject[] | null = null) {
  return {
    projects: projects ?? [
      project("alpha", "Habit Tracker"),
      project("beta", "Link Shelf"),
    ],
    statuses: { alpha: RUNNING },
    loading: false,
    error: null,
    buildingProjectId: null,
  } satisfies PocketCatalog;
}

/** Drive the mocked CreationPanel's `onCatalogChange` as the real panel does
 *  whenever its catalog state moves. */
function publishCatalog(catalog: PocketCatalog) {
  const props = fixtures.creationProps as {
    onCatalogChange: (next: PocketCatalog) => void;
  } | null;
  if (!props) throw new Error("CreationPanel never mounted");
  act(() => {
    props.onCatalogChange(catalog);
  });
}

function selectSidebarPocket(name: string) {
  fireEvent.click(within(screen.getByTestId("pocket-sidebar")).getByText(name));
}

describe("App shell in the browser (no Tauri runtime)", () => {
  it("renders the honest workspace outline without touching native", () => {
    const { container } = render(<App />);

    // v3 §1: the global chrome is gone from App itself — no full-width top
    // bar, no logo, no breadcrumb, no nav rail. Each column's title row
    // belongs to the column, and the workspace columns are components with
    // suites of their own, so a plain browser sees honest landmarks only.
    expect(screen.queryByRole("banner")).toBeNull();
    expect(container.querySelector(".topbar")).toBeNull();
    expect(container.querySelector(".logo")).toBeNull();
    expect(container.querySelector(".crumb-current")).toBeNull();
    expect(screen.queryByTestId("app-nav")).toBeNull();
    expect(screen.getByRole("main")).toBeTruthy();
    const sidebarColumn = screen.getByTestId("pocket-sidebar").parentElement;
    expect(sidebarColumn?.tagName).toBe("DIV");
    expect(sidebarColumn?.className).toBe("sidebar");
    expect(screen.getByRole("region", { name: "Development" })).toBeTruthy();
    expect(screen.getByRole("region", { name: "Preview" })).toBeTruthy();
    expect(screen.queryByRole("contentinfo")).toBeNull();

    // No fabricated session anywhere.
    expect(container.textContent).not.toMatch(/session [0-9a-f]/);

    // The browser-hosted Pockets dashboard connects directly to the daemon
    // with no "cannot reach local runtime" notice.
    expect(screen.queryByText(/cannot reach the local runtime/i)).toBeNull();
    expect(screen.getByText("Create a project")).toBeTruthy();

    const sidebar = screen.getByTestId("pocket-sidebar");
    expect(sidebar.getAttribute("data-page")).toBe("workspace");
    expect(within(sidebar).getByText("pockets.local")).toBeTruthy();
    expect(sidebar.getAttribute("data-running-count")).toBe("none");
    expect(container.querySelector(".head-status")).toBeNull();
    expect(container.querySelector(".statusbar")).toBeNull();

    // The management page is present but hidden on the workspace route.
    expect(
      container.querySelector(".pockets-host")?.hasAttribute("hidden"),
    ).toBe(true);

    // Boot commands fire against the local daemon.
    expect(invokeMock).toHaveBeenCalledWith("get_manifest");
    expect(invokeMock).toHaveBeenCalledWith("detect_runtime");
  });

  it("navigates between the workspace and the two management pages", () => {
    const { container } = render(<App />);
    const sidebar = () => screen.getByTestId("pocket-sidebar");

    expect(sidebar().getAttribute("data-page")).toBe("workspace");
    fireEvent.click(
      within(sidebar()).getByRole("button", { name: "Live pockets" }),
    );
    expect(window.location.hash).toBe("#/live");
    expect(sidebar().getAttribute("data-page")).toBe("live");
    expect(
      container.querySelector(".pockets-host")?.hasAttribute("hidden"),
    ).toBe(false);

    expect(screen.queryByTestId("pocket-chat")).toBeNull();
    expect(screen.queryByRole("region", { name: "Preview" })).toBeNull();
    expect(screen.getByTestId("pocket-sidebar")).toBeTruthy();
    expect(screen.getByText("Appearance")).toBeTruthy();
    expect(screen.getAllByText("Container runtime").length).toBeGreaterThan(0);

    const titleRow = screen.getByRole("toolbar", { name: "Management" });
    expect(container.querySelector("h1.page-head-title")?.textContent).toBe(
      "Live pockets",
    );
    expect(
      within(titleRow).queryByRole("button", { name: "Toggle sidebar" }),
    ).toBeNull();
    expect(titleRow.getAttribute("data-reserve-lights")).toBeNull();
    expect(within(sidebar()).getByText("Pockets")).toBeTruthy();

    fireEvent.click(
      within(sidebar()).getByRole("button", { name: "Toggle sidebar" }),
    );
    expect(screen.queryByTestId("pocket-sidebar")).toBeNull();
    fireEvent.click(
      within(titleRow).getByRole("button", { name: "Toggle sidebar" }),
    );
    expect(screen.getByTestId("pocket-sidebar")).toBeTruthy();
    expect(
      within(titleRow).queryByRole("button", { name: "Toggle sidebar" }),
    ).toBeNull();

    fireEvent.click(within(sidebar()).getByRole("button", { name: "Drafts" }));
    expect(window.location.hash).toBe("#/drafts");
    expect(sidebar().getAttribute("data-page")).toBe("drafts");
    expect(container.querySelector("h1.page-head-title")?.textContent).toBe(
      "Drafts",
    );

    pressMeta("1");
    expect(window.location.hash).toBe("#/");
    expect(sidebar().getAttribute("data-page")).toBe("workspace");
    expect(screen.queryByRole("toolbar", { name: "Management" })).toBeNull();
    expect(container.querySelector("h1.page-head-title")).toBeNull();
    expect(screen.getByText("Create a project")).toBeTruthy();
  });

  it("moves between the pages and a new pocket on ⌘1/⌘2/⌘3/⌘N", () => {
    render(<App />);

    pressMeta("2");
    expect(window.location.hash).toBe("#/live");
    expect(screen.getByTestId("pocket-sidebar").getAttribute("data-page")).toBe(
      "live",
    );
    pressMeta("3");
    expect(window.location.hash).toBe("#/drafts");

    pressMeta("n");
    expect(window.location.hash).toBe("#/");
    pressMeta("2");
    pressMeta("N");
    expect(window.location.hash).toBe("#/");

    pressMeta("3");
    pressMeta("1");
    expect(window.location.hash).toBe("#/");

    pressMeta("2", { ctrlKey: true });
    pressMeta("2", { altKey: true });
    fireEvent.keyDown(window, { key: "2" });
    expect(window.location.hash).toBe("#/");

    expect(screen.getByTestId("pocket-sidebar")).toBeTruthy();
    pressMeta("b");
    expect(screen.queryByTestId("pocket-sidebar")).toBeNull();
    pressMeta("B");
    expect(screen.getByTestId("pocket-sidebar")).toBeTruthy();
  });
  it("collapses the pockets column from its header and persists the choice", () => {
    const { container } = render(<App />);
    const shell = container.querySelector(".shell");
    const workspace = container.querySelector(".workspace");

    expect(shell?.getAttribute("data-sidebar-collapsed")).toBe("false");
    expect(workspace?.getAttribute("data-preview-collapsed")).toBe("false");

    fireEvent.click(
      within(screen.getByTestId("pocket-sidebar")).getByRole("button", {
        name: "Toggle sidebar",
      }),
    );
    expect(screen.queryByTestId("pocket-sidebar")).toBeNull();
    expect(container.querySelector(".sidebar")).toBeNull();
    expect(shell?.getAttribute("data-sidebar-collapsed")).toBe("true");
    const newProjectRow = screen.getByRole("toolbar", { name: "New project" });
    fireEvent.click(
      within(newProjectRow).getByRole("button", { name: "Toggle sidebar" }),
    );
    expect(screen.getByTestId("pocket-sidebar")).toBeTruthy();
    expect(shell?.getAttribute("data-sidebar-collapsed")).toBe("false");
    expect(
      within(newProjectRow).queryByRole("button", { name: "Toggle sidebar" }),
    ).toBeNull();

    fireEvent.click(
      within(screen.getByTestId("pocket-sidebar")).getByRole("button", {
        name: "Toggle sidebar",
      }),
    );
    expect(JSON.parse(localStorage.getItem("pockets.layout") ?? "{}")).toEqual({
      sidebarCollapsed: true,
      previewCollapsed: false,
      previewWidth: null,
    });
    cleanup();
    render(<App />);
    expect(screen.queryByTestId("pocket-sidebar")).toBeNull();
  });
});

describe("App shell in the native workspace", () => {
  /**
   * App decides Tauri-vs-browser from `__TAURI_INTERNALS__` at module
   * scope, so each test re-imports a fresh module graph with the marker
   * present, exactly as a Tauri webview provides it. A fresh graph re-runs
   * the mock factories: assert against this graph's `invoke`.
   */
  async function renderNativeApp() {
    vi.resetModules();
    (window as unknown as Record<string, unknown>).__TAURI_INTERNALS__ = {};
    fixtures.creationProps = null;
    fixtures.previewProps = null;
    fixtures.nextSessionId = null;
    fixtures.nextSessionOrigin = undefined;
    fixtures.fullscreen = false;
    fixtures.resizeListeners.length = 0;
    const api = await import("./api");
    const nativeInvoke = vi.mocked(api.invoke);
    nativeInvoke.mockImplementation(async (cmd) => {
      switch (cmd) {
        case "get_manifest":
          return {
            apiVersion: "1",
            kind: "pocket",
            metadata: {
              id: "example-note",
              name: "Example Note",
              version: "0.1.0",
              description: "A tiny note Pocket.",
            },
            runtime: {
              image: "pockets/example-note:0.1.0",
              architectures: ["arm64"],
            },
            web: { port: 3179, healthcheck: "/healthz" },
            storage: { name: "example-note-data", path: "/data" },
          };
        case "detect_runtime":
          return {
            state: "ready",
            message: "podman 5.4.2 detected",
            version: "5.4.2",
          };
        case "pocket_status":
          return {
            state: "stopped",
            url: null,
            message: "the Pocket is stopped",
            ready: false,
          };
        case "project_preview_status":
          return {
            state: "stopped",
            revision: 0,
            nextCursor: 0,
            eventsSince: [],
          };
        case "project_preview_network":
          return { hosts: [], previewApis: [] };
        case "resource_snapshot":
          // Management-page monitor sample: a realistic shape so the card
          // renders real values, not a failure notice.
          return {
            sampledAtMs: Date.now(),
            pockets: [],
            totals: { cpuPercent: 9, memoryBytes: 1_281_234_944 },
            warnings: [],
          };
        case "pocket_thumb":
          // The overlay stand-in's snapshot read (v3 §9.2): an honest "no
          // thumb stored" answer, so the panel must fall back to the pocket
          // name rather than any fabricated picture.
          return null;
        case "delete_project":
          // The chat's delete request, confirmed by the App-owned dialog.
          return true;
        default:
          // Any other surface the children poll must fail the way a missing
          // command does natively — the components catch this into inline
          // notices, never a crash.
          throw new Error(`unexpected native command: ${String(cmd)}`);
      }
    });
    const { default: NativeApp } = await import("./App");
    const view = render(<NativeApp />);
    return { ...view, nativeInvoke };
  }

  it("waits for project metadata before choosing the external workspace, without mounting AI", async () => {
    window.location.hash = `#/p/${SESSION}`;
    const { nativeInvoke } = await renderNativeApp();
    expect(screen.getByText("Loading project")).toBeTruthy();
    const original = nativeInvoke.getMockImplementation()!;
    nativeInvoke.mockImplementation((command, args) =>
      command === "development_status"
        ? Promise.resolve({
            linked: true,
            folder: "/projects/tracker",
            available: true,
            error: null,
          })
        : original(command, args),
    );
    publishCatalog(
      catalogFixture([
        { ...project(SESSION, "Tracker"), developmentLinked: true },
      ]),
    );
    expect(await screen.findByText("Develop in your coding tool")).toBeTruthy();
    expect(
      nativeInvoke.mock.calls.some(([command]) => command.startsWith("ai_")),
    ).toBe(false);
  });

  it("opens external projects with preview API approvals in the development workspace", async () => {
    const { nativeInvoke } = await renderNativeApp();
    const original = nativeInvoke.getMockImplementation()!;
    nativeInvoke.mockImplementation(async (command, args) => {
      if (command === "development_status") {
        return {
          linked: true,
          folder: "/projects/tracker",
          available: true,
          error: null,
        };
      }
      if (command === "project_preview_status") {
        return {
          state: "ready",
          revision: 1,
          nextCursor: 1,
          eventsSince: [],
          apiAccess: {
            token: "kraken-review",
            services: [
              {
                host: "api.kraken.com",
                name: "Kraken pricing",
                purpose: "Show live BTC prices.",
              },
            ],
          },
        };
      }
      return original(command, args);
    });
    const external = {
      ...project(SESSION, "Tracker"),
      developmentLinked: true,
    };
    publishCatalog(catalogFixture([external]));
    selectSidebarPocket("Tracker");
    expect(await screen.findByText("Develop in your coding tool")).toBeTruthy();
    expect(screen.queryByTestId("pocket-chat")).toBeNull();
    expect(fixtures.previewProps?.preview.projectId).toBe(SESSION);
    expect(await screen.findByText("/projects/tracker")).toBeTruthy();
    const approve = await screen.findByRole("button", {
      name: "Allow",
    });
    expect(approve.closest(".external-workspace-content")).toBeTruthy();
    const access = approve.closest("details");
    expect(access?.open).toBe(true);
    expect(access?.getAttribute("data-approval-needed")).toBe("true");

    fireEvent.click(screen.getByRole("button", { name: "Toggle preview" }));
    expect(screen.queryByTestId("preview-panel")).toBeNull();
    expect(screen.getByRole("button", { name: "Allow" })).toBe(approve);
  });

  it("mounts the workspace and hides the management page, not the catalog", async () => {
    const { container, nativeInvoke } = await renderNativeApp();

    // Native boot commands fire.
    expect(nativeInvoke).toHaveBeenCalledWith("get_manifest");
    expect(nativeInvoke).toHaveBeenCalledWith("detect_runtime");

    // Standard browser chrome — no macOS Tauri native-chrome flag.
    expect(
      container.querySelector(".app")?.getAttribute("data-native-chrome"),
    ).toBeNull();

    // The management host stays mounted (hidden) so CreationPanel keeps
    // publishing the catalog while the workspace shows.
    expect(screen.getByTestId("creation-panel")).toBeTruthy();
    expect(
      container.querySelector(".pockets-host")?.hasAttribute("hidden"),
    ).toBe(true);

    // Before the first snapshot the sidebar is honestly loading, and no
    // runtime/lifecycle card pollutes the workspace.
    expect(
      screen.getByTestId("pocket-sidebar").getAttribute("data-loading"),
    ).toBe("true");
    expect(screen.queryByText("Container runtime")).toBeNull();
    expect(screen.queryByText("Example Pocket")).toBeNull();

    // The sidebar footer claims no count until a trustworthy catalog
    // status exists (v3 §3.5), and no host metric ever appears — the old
    // header's `.head-status` went away with the topbar itself.
    expect(
      screen.getByTestId("pocket-sidebar").getAttribute("data-running-count"),
    ).toBe("none");
    expect(container.querySelector(".head-status")).toBeNull();

    expect(
      screen.getByRole("heading", { name: "Create a project" }),
    ).toBeTruthy();
    expect(screen.queryByTestId("pocket-chat")).toBeNull();
    fireEvent.click(
      within(screen.getByTestId("pocket-sidebar")).getByRole("button", {
        name: "Live pockets",
      }),
    );
    expect(window.location.hash).toBe("#/live");
  });

  it("publishes the catalog into the sidebar and development workspace", async () => {
    const { container } = await renderNativeApp();
    publishCatalog(catalogFixture());

    // The running count is App's trustworthy count, now a sidebar prop
    // (v3 §3.5): one running read means the footer says "1 running".
    expect(
      screen.getByTestId("pocket-sidebar").getAttribute("data-running-count"),
    ).toBe("1");

    const sidebar = screen.getByTestId("pocket-sidebar");
    expect(within(sidebar).getByText("Habit Tracker")).toBeTruthy();
    expect(within(sidebar).getByText("Link Shelf")).toBeTruthy();

    selectSidebarPocket("Habit Tracker");
    expect(window.location.hash).toBe("#/p/alpha");
    expect(
      screen.getByRole("toolbar", { name: "Development" }).textContent,
    ).toContain("Habit Tracker");
    expect(fixtures.previewProps?.preview.projectId).toBeNull();
    expect(
      within(sidebar).getByText("Habit Tracker").getAttribute("aria-current"),
    ).toBe("true");
    expect(container.querySelector(".crumb-current")).toBeNull();

    // The count follows the trust rules exactly as the old header did: a
    // trustworthy non-running read says "0", no trustworthy read says
    // nothing at all, and a host metric never appears.
    publishCatalog({ ...catalogFixture(), statuses: { alpha: STOPPED } });
    expect(
      screen.getByTestId("pocket-sidebar").getAttribute("data-running-count"),
    ).toBe("0");
    publishCatalog({ ...catalogFixture(), statuses: {} });
    expect(
      screen.getByTestId("pocket-sidebar").getAttribute("data-running-count"),
    ).toBe("none");
    publishCatalog(catalogFixture());

    // The catalog pages keep their secondary management behind details, and
    // the catalog panel is never remounted for navigation. The live route
    // puts the panel in live mode.
    fireEvent.click(
      within(sidebar).getByRole("button", { name: "Live pockets" }),
    );
    expect(screen.getByTestId("creation-panel")).toBeTruthy();
    expect((fixtures.creationProps as { page: string }).page).toBe("live");
    const summaries = Array.from(
      container.querySelectorAll(".pockets-secondary summary"),
    ).map((node) => node.textContent);
    // The four management sections, each with its own nested detail folds.
    expect(summaries).toEqual(
      expect.arrayContaining([
        "Container runtime",
        "Example Pocket",
        "Resource monitor",
      ]),
    );
  });

  it("titles the management page in its own title row", async () => {
    const { container } = await renderNativeApp();
    publishCatalog(catalogFixture());

    // The page name leads the management column's `TitleBar` (v3 §5), fed
    // by the same route that drives the panel mode; the sidebar heading
    // stays the static word.
    fireEvent.click(
      within(screen.getByTestId("pocket-sidebar")).getByRole("button", {
        name: "Live pockets",
      }),
    );
    expect(window.location.hash).toBe("#/live");
    expect((fixtures.creationProps as { page: string }).page).toBe("live");
    const titleRow = screen.getByRole("toolbar", { name: "Management" });
    expect(container.querySelector("h1.page-head-title")?.textContent).toBe(
      "Live pockets",
    );
    // Sidebar open: the toggle stays in the sidebar's own row and the
    // traffic lights are not this row's side of the window (v3 §1.5).
    expect(
      within(titleRow).queryByRole("button", { name: "Toggle sidebar" }),
    ).toBeNull();
    expect(titleRow.getAttribute("data-reserve-lights")).toBeNull();

    // The Drafts page is its own route and its own words, without a
    // remount — builds and status polls keep running across the switch.
    fireEvent.click(
      within(screen.getByTestId("pocket-sidebar")).getByRole("button", {
        name: "Drafts",
      }),
    );
    expect(window.location.hash).toBe("#/drafts");
    expect((fixtures.creationProps as { page: string }).page).toBe("drafts");
    expect(screen.getByTestId("creation-panel")).toBeTruthy();
    expect(container.querySelector("h1.page-head-title")?.textContent).toBe(
      "Drafts",
    );

    // Collapse the sidebar: the toggle moves into this row, which is now
    // the window's left edge — inside Tauri it reserves the traffic-light
    // side behind the lights (v3 §3/§5).
    fireEvent.click(
      within(screen.getByTestId("pocket-sidebar")).getByRole("button", {
        name: "Toggle sidebar",
      }),
    );
    const toggle = within(titleRow).getByRole("button", {
      name: "Toggle sidebar",
    });
    expect(titleRow.getAttribute("data-reserve-lights")).toBe("true");
    fireEvent.click(toggle);
    expect(screen.getByTestId("pocket-sidebar")).toBeTruthy();
    expect(
      within(titleRow).queryByRole("button", { name: "Toggle sidebar" }),
    ).toBeNull();
    expect(titleRow.getAttribute("data-reserve-lights")).toBeNull();
  });

  it("omits native-chrome window flags in the browser-hosted dashboard", async () => {
    localStorage.setItem(
      "pockets.layout",
      JSON.stringify({ sidebarCollapsed: true, previewCollapsed: false }),
    );
    const { container } = await renderNativeApp();
    expect(
      container.querySelector(".app")?.getAttribute("data-native-chrome"),
    ).toBeNull();
    pressMeta("2");
    const titleRow = screen.getByRole("toolbar", { name: "Management" });
    expect(titleRow).toBeTruthy();
  });

  it("routes the native pane by linked project and hides it when switching to a snapshot", async () => {
    const { nativeInvoke } = await renderNativeApp();
    publishCatalog(
      catalogFixture([
        { ...project(SESSION, "Habit Tracker"), developmentLinked: true },
        project("beta", "Link Shelf"),
      ]),
    );
    selectSidebarPocket("Habit Tracker");
    expect(fixtures.previewProps?.preview.projectId).toBe(SESSION);
    selectSidebarPocket("Link Shelf");
    expect(fixtures.previewProps?.preview.projectId).toBeNull();
    expect(nativeInvoke).toHaveBeenCalledWith("project_preview_hide", {
      projectId: SESSION,
    });

    selectSidebarPocket("Habit Tracker");
    fireEvent.click(screen.getByRole("button", { name: "Toggle preview" }));
    expect(screen.queryByTestId("preview-panel")).toBeNull();
    fireEvent.click(
      within(screen.getByRole("toolbar", { name: "Development" })).getByRole(
        "button",
        { name: "Toggle preview" },
      ),
    );
    expect(fixtures.previewProps?.preview.projectId).toBe(SESSION);
  });

  it("collapses the side columns from their column headers and persists the choice", async () => {
    const { container, nativeInvoke } = await renderNativeApp();
    const shell = container.querySelector(".shell");
    const workspace = container.querySelector(".workspace");

    // Open by default: both columns show. The toggles live in the columns
    // themselves now (v3 §3/§4), so their pressed state belongs to the
    // component suites; App pins the effect of pressing them. The divider
    // sits between chat and preview with a mirrored aria width.
    expect(shell?.getAttribute("data-sidebar-collapsed")).toBe("false");
    expect(workspace?.getAttribute("data-preview-collapsed")).toBe("false");
    const divider = screen.getByRole("separator", { name: "Resize preview" });
    expect(divider.getAttribute("aria-valuenow")).not.toBeNull();
    // No explicit width yet, so the built-in ratio governs and the grid
    // carries no inline override (v3 §8.2).
    expect(workspace?.getAttribute("style")).toBeNull();

    // Collapsing the sidebar removes the whole column — columns never
    // stack or shrink-to-nothing behind an overlay — and the chat header
    // picks up the hidden column's toggle.
    nativeInvoke.mockClear();
    fireEvent.click(
      within(screen.getByTestId("pocket-sidebar")).getByRole("button", {
        name: "Toggle sidebar",
      }),
    );
    expect(screen.queryByTestId("pocket-sidebar")).toBeNull();
    expect(container.querySelector(".sidebar")).toBeNull();
    expect(shell?.getAttribute("data-sidebar-collapsed")).toBe("true");
    expect(
      within(screen.getByRole("toolbar", { name: "New project" })).getByRole(
        "button",
        {
          name: "Toggle sidebar",
        },
      ),
    ).toBeTruthy();
    // Nothing about the preview pane changed: no native traffic.
    expect(nativeInvoke).not.toHaveBeenCalled();

    // Collapsing the preview rail unmounts the panel and its divider
    // outright; the chat header carries that toggle too.
    fireEvent.click(
      within(screen.getByTestId("preview-panel")).getByRole("button", {
        name: "Toggle preview",
      }),
    );
    expect(screen.queryByTestId("preview-panel")).toBeNull();
    expect(
      screen.queryByRole("separator", { name: "Resize preview" }),
    ).toBeNull();
    expect(workspace?.getAttribute("data-preview-collapsed")).toBe("true");
    expect(
      within(screen.getByRole("toolbar", { name: "New project" })).getByRole(
        "button",
        {
          name: "Toggle preview",
        },
      ),
    ).toBeTruthy();

    // The choices persist across a remount (a relaunch)…
    expect(JSON.parse(localStorage.getItem("pockets.layout") ?? "{}")).toEqual({
      sidebarCollapsed: true,
      previewCollapsed: true,
      previewWidth: null,
    });
    cleanup();
    await renderNativeApp();
    expect(screen.queryByTestId("pocket-sidebar")).toBeNull();
    expect(screen.queryByTestId("preview-panel")).toBeNull();

    // …and expanding from the chat header brings both columns back.
    fireEvent.click(
      within(screen.getByRole("toolbar", { name: "New project" })).getByRole(
        "button",
        {
          name: "Toggle sidebar",
        },
      ),
    );
    fireEvent.click(
      within(screen.getByRole("toolbar", { name: "New project" })).getByRole(
        "button",
        {
          name: "Toggle preview",
        },
      ),
    );
    expect(screen.getByTestId("pocket-sidebar")).toBeTruthy();
    expect(screen.getByTestId("preview-panel")).toBeTruthy();
    expect(JSON.parse(localStorage.getItem("pockets.layout") ?? "{}")).toEqual({
      sidebarCollapsed: false,
      previewCollapsed: false,
      previewWidth: null,
    });
  });

  it("mirrors the persisted preview width into the divider and the grid", async () => {
    // A stored width lands on the divider's aria value and the inline grid
    // (v3 §8.2). Collapsed sidebar keeps 520 inside the clamp at jsdom's
    // 1024px window.
    localStorage.setItem(
      "pockets.layout",
      JSON.stringify({
        sidebarCollapsed: true,
        previewCollapsed: false,
        previewWidth: 520,
      }),
    );
    const { container } = await renderNativeApp();
    const workspace = container.querySelector(".workspace");
    expect(workspace?.getAttribute("style")).toBe(
      "grid-template-columns: minmax(360px, 1fr) 1px 520px;",
    );
    expect(
      screen
        .getByRole("separator", { name: "Resize preview" })
        .getAttribute("aria-valuenow"),
    ).toBe("520");
  });

  it("offers the preview toggle only where a preview column exists", async () => {
    await renderNativeApp();
    // Workspace, preview open: the preview's own title row owns the
    // toggle; the chat row only shows it while the column is hidden.
    expect(
      within(screen.getByTestId("preview-panel")).getByRole("button", {
        name: "Toggle preview",
      }),
    ).toBeTruthy();
    expect(
      within(screen.getByRole("toolbar", { name: "New project" })).queryByRole(
        "button",
        {
          name: "Toggle preview",
        },
      ),
    ).toBeNull();
    expect(
      within(screen.getByTestId("pocket-sidebar")).getByRole("button", {
        name: "Toggle sidebar",
      }),
    ).toBeTruthy();

    fireEvent.click(
      within(screen.getByTestId("pocket-sidebar")).getByRole("button", {
        name: "Live pockets",
      }),
    );
    expect(window.location.hash).toBe("#/live");
    // A catalog page has no preview column (and no chat), so there is no
    // preview toggle anywhere; the sidebar column and its own toggle
    // persist across the page.
    expect(screen.queryByRole("button", { name: "Toggle preview" })).toBeNull();
    expect(screen.getByTestId("pocket-sidebar")).toBeTruthy();
    expect(
      within(screen.getByTestId("pocket-sidebar")).getByRole("button", {
        name: "Toggle sidebar",
      }),
    ).toBeTruthy();
  });

  it("opens a newly created project without mounting AI while the catalog catches up", async () => {
    const { nativeInvoke } = await renderNativeApp();
    const original = nativeInvoke.getMockImplementation()!;
    const created = {
      ...project(SESSION, "Recipe Box"),
      developmentLinked: true,
    };
    nativeInvoke.mockImplementation((command, args) => {
      if (command === "create_development_project")
        return Promise.resolve(created);
      if (command === "development_status")
        return Promise.resolve({
          linked: true,
          folder: "/projects/recipe-box",
          available: true,
          error: null,
        });
      return original(command, args);
    });
    publishCatalog(catalogFixture());

    fireEvent.click(
      within(screen.getByTestId("pocket-sidebar")).getByRole("button", {
        name: "New pocket",
      }),
    );
    expect(window.location.hash).toBe("#/");
    expect(screen.queryByTestId("pocket-chat")).toBeNull();
    expect(
      screen.getByRole("heading", { name: "Create a project" }),
    ).toBeTruthy();

    const refreshKeyBefore = (fixtures.creationProps as { refreshKey: number })
      .refreshKey;
    fireEvent.change(screen.getByLabelText("Project name"), {
      target: { value: "Recipe Box" },
    });
    fireEvent.click(screen.getByRole("button", { name: "Create project…" }));
    await screen.findByText("Develop in your coding tool");

    // The catalog refetches in place (refreshKey bumped, no remount), and
    // the new-pocket workspace moves onto the saved id without ever
    // flashing "not found" while the refetch is in flight.
    expect((fixtures.creationProps as { refreshKey: number }).refreshKey).toBe(
      refreshKeyBefore + 1,
    );
    expect(screen.getByTestId("creation-panel")).toBeTruthy();
    expect(window.location.hash).toBe(`#/p/${SESSION}`);
    expect(screen.queryByTestId("pocket-chat")).toBeNull();
    expect(fixtures.previewProps?.preview.projectId).toBe(SESSION);
    expect(screen.queryByText("Pocket not found")).toBeNull();

    // Late status/list snapshots may still omit the newly registered project.
    // Neither can replace its development workspace with a legacy AI chat.
    publishCatalog(catalogFixture());
    publishCatalog({ ...catalogFixture(), loading: true });
    expect(screen.getByText("Develop in your coding tool")).toBeTruthy();
    expect(screen.queryByTestId("pocket-chat")).toBeNull();

    // The refreshed catalog resolves the name where the chat header reads
    // it (the topbar breadcrumb is gone: this prop is the name's route).
    publishCatalog(
      catalogFixture([
        project("alpha", "Habit Tracker"),
        project("beta", "Link Shelf"),
        created,
      ]),
    );
    expect(
      screen.getByRole("toolbar", { name: "Development" }).textContent,
    ).toContain("Recipe Box");

    // Deleting it from the management page leaves no stale chat or preview:
    // the route now names a pocket the trustworthy catalog does not know.
    publishCatalog(catalogFixture([project("alpha", "Habit Tracker")]));
    expect(screen.getByText("Pocket not found")).toBeTruthy();
    expect(screen.queryByTestId("pocket-chat")).toBeNull();
    expect(
      screen.getByTestId("preview-panel").getAttribute("data-session"),
    ).toBe("none");
    fireEvent.click(
      within(screen.getByRole("region", { name: "Development" })).getByRole(
        "button",
        {
          name: "Live pockets",
        },
      ),
    );
    expect(window.location.hash).toBe("#/live");
  });

  it("refreshes a late creation result without navigating away from the user's current page", async () => {
    const { nativeInvoke } = await renderNativeApp();
    let finish!: (value: unknown) => void;
    const original = nativeInvoke.getMockImplementation()!;
    nativeInvoke.mockImplementation((command, args) =>
      command === "create_development_project"
        ? new Promise((resolve) => {
            finish = resolve;
          })
        : original(command, args),
    );
    fireEvent.change(screen.getByLabelText("Project name"), {
      target: { value: "Tracker" },
    });
    fireEvent.click(screen.getByRole("button", { name: "Create project…" }));
    fireEvent.click(
      within(screen.getByTestId("pocket-sidebar")).getByRole("button", {
        name: "Live pockets",
      }),
    );
    const refreshBefore = fixtures.creationProps?.refreshKey as number;
    await act(async () => finish(project(SESSION, "Tracker")));
    expect(window.location.hash).toBe("#/live");
    expect(fixtures.creationProps?.refreshKey).toBe(refreshBefore + 1);
    expect(
      within(screen.getByTestId("pocket-sidebar")).getByText("Tracker"),
    ).toBeTruthy();
  });
});
