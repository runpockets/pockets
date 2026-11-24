import { useCallback, useEffect, useMemo, useRef, useState } from "react";
import { invoke } from "./api";
import AuthGateModal from "./AuthGateModal";
import FolderPickerModal from "./FolderPickerModal";
import CreationPanel, {
  type LoadState,
  type PocketCatalog,
  type PocketStatus,
  type Project,
} from "./CreationPanel";
import PocketSidebar, {
  type PocketUsage,
  type ResourceSnapshot,
} from "./PocketSidebar";
import ExternalWorkspace from "./ExternalWorkspace";
import NewProject from "./NewProject";
import PreviewPanel, { type PreviewBounds } from "./PreviewPanel";
import { useProjectPreview } from "./useProjectPreview";
import ResourceMonitor from "./ResourceMonitor";
import { TailscaleStatusPanel, type TailscaleReport } from "./TailscaleShare";
import { usePocketRoute } from "./usePocketRoute";
import { IconButton, Segmented } from "./Status";
import { IconPanelLeft, IconPanelRight } from "./Icons";
import { TitleBar } from "./TitleBar";
import { ColumnDivider } from "./ColumnDivider";
import { FullscreenProvider, useFullscreen } from "./useFullscreen";
import { OverlayProvider } from "./OverlayContext";
import {
  CHAT_MIN_WIDTH,
  PREVIEW_MIN_WIDTH,
  loadLayout,
  saveLayout,
  type LayoutState,
} from "./layout";
import {
  applyTheme,
  loadThemeMode,
  saveThemeMode,
  watchSystemTheme,
  type ThemeMode,
} from "./theme";

/** Bundled manifest returned by the native `get_manifest` command. */
interface PocketManifest {
  apiVersion: string;
  kind: string;
  metadata: {
    id: string;
    name: string;
    version: string;
    description: string;
  };
  runtime: {
    image: string;
    architectures: string[];
  };
  web: {
    port: number;
    healthcheck: string;
  };
  storage: {
    name: string;
    path: string;
  };
}

/** Podman detection report returned by the native `detect_runtime` command. */
interface DetectionReport {
  state: "missing" | "unavailable" | "ready";
  message: string;
  version: string | null;
}

/**
 * `LoadState` and `PocketStatus` are shared with the catalog (`pocket_status`
 * and `project_status` return the same shape). The native `open_*` commands
 * open the local site in the system browser.
 */

type PocketOperation = "starting" | "stopping" | "opening";

const OPERATION_COMMANDS: Record<PocketOperation, string> = {
  starting: "start_pocket",
  stopping: "stop_pocket",
  opening: "open_pocket",
};

const OPERATION_LABELS: Record<PocketOperation, string> = {
  starting: "Starting…",
  stopping: "Stopping…",
  opening: "Opening…",
};

const STATE_LABELS: Record<DetectionReport["state"], string> = {
  ready: "Ready",
  unavailable: "Unavailable",
  missing: "Missing",
};

/**
 * The cross-platform web daemon serves the frontend over loopback HTTP, so
 * the local Pockets backend is always available in the browser.
 */
const inTauri = true;

function errorMessage(err: unknown): string {
  if (typeof err === "string") return err;
  if (err instanceof Error) return err.message;
  return "The request failed for an unknown reason.";
}

/**
 * Catalog snapshots. CreationPanel is the single owner of catalog state and
 * publishes snapshots through `onCatalogChange`; App only mirrors the latest
 * one for the sidebar, the header count and not-found detection. Until the
 * first snapshot lands the store is honestly loading; in a plain browser
 * nothing will ever publish, so the sidebar gets an honest empty instead of
 * a spinner that can never finish.
 */
const LOADING_CATALOG: PocketCatalog = {
  projects: [],
  statuses: {},
  loading: true,
  error: null,
  buildingProjectId: null,
};

const EMPTY_CATALOG: PocketCatalog = { ...LOADING_CATALOG, loading: false };

/**
 * Wires the preview panel's reserved rect to the native pane
 * (docs/live-preview.md "The preview pane"): every rect PreviewPanel reports
 * on mount or resize is forwarded to `project_preview_set_bounds`, and the pane
 * is torn down with `project_preview_hide` when the panel unmounts, the preview
 * session switches, or the pane is told to step aside (`visible`, v3 §9 —
 * an in-app overlay is open and the native view would draw on top of it).
 * App feeds this hook the route-effective session id, so moving to another
 * pocket or to the management page hides the pane exactly like a session
 * switch does.
 *
 * Becoming visible again re-reports the most recent rect, so an overlay
 * closing restores the pane exactly where the panel says it belongs (v3
 * §9.3); reports arriving while hidden are stored, not dropped, and
 * PreviewPanel's rAF coalescing keeps them honest.
 *
 * Deliberately unthrottled beyond the panel's own rAF pass: the spike
 * measured a native move round trip at median 1 ms (spikes/pane/RESULTS.md),
 * and native decides each report against the preview registry (create, move,
 * reclaim, or skip). Rejections are silent because placement is advisory:
 * the panel keeps its in-card fallback UI, and the next report corrects a
 * missed one. A null session — the state until the generation slice hands
 * its id over — has no pane to address, so both directions no-op.
 */
export function usePreviewPane(
  previewSessionId: string | null,
  visible: boolean,
) {
  const latestBoundsRef = useRef<PreviewBounds | null>(null);

  const send = useCallback((sessionId: string, bounds: PreviewBounds) => {
    void invoke("project_preview_set_bounds", {
      projectId: sessionId,
      x: bounds.x,
      y: bounds.y,
      width: bounds.width,
      height: bounds.height,
    }).catch(() => {
      // Advisory placement: the next ResizeObserver report retries.
    });
  }, []);

  const handlePreviewBounds = useCallback(
    (bounds: PreviewBounds) => {
      if (previewSessionId === null) return;
      latestBoundsRef.current = bounds;
      if (!visible) return;
      send(previewSessionId, bounds);
    },
    [previewSessionId, visible, send],
  );

  useEffect(() => {
    if (previewSessionId === null) {
      latestBoundsRef.current = null;
      return;
    }
    const sessionId = previewSessionId;
    if (!visible) {
      // Step aside for an overlay: hide exactly like a collapse would; the
      // session itself stays bound and the panel keeps its state.
      void invoke("project_preview_hide", { projectId: sessionId }).catch(
        () => {
          /* best effort, same as the cleanup path */
        },
      );
      return;
    }
    // (Re)showing: hand the pane the freshest rect the panel reported.
    const latest = latestBoundsRef.current;
    if (latest !== null) send(sessionId, latest);
    // Runs on unmount and before every session switch or hide; native skips
    // the request when the visible pane belongs to another session.
    return () => {
      void invoke("project_preview_hide", { projectId: sessionId }).catch(
        () => {
          // Best effort: native also reclaims the pane when the preview
          // itself stops, and a late hide stays harmless.
        },
      );
    };
  }, [previewSessionId, visible, send]);

  return handlePreviewBounds;
}

export default function App() {
  const { route, selectProject, newPocket, showLivePockets, showDrafts } =
    usePocketRoute();
  // Theme mode (pockets-design-update-v2.md §9): "system" follows
  // prefers-color-scheme and keeps following it live; a manual override is
  // persisted. Both switch by changing the data-theme attribute only — no
  // reload, and nothing hard-codes a hex value.
  const [themeMode, setThemeMode] = useState<ThemeMode>(() => loadThemeMode());
  useEffect(() => {
    applyTheme(themeMode);
    if (themeMode !== "system") return;
    return watchSystemTheme(() => applyTheme("system"));
  }, [themeMode]);
  const changeThemeMode = useCallback((mode: ThemeMode) => {
    saveThemeMode(mode);
    setThemeMode(mode);
  }, []);
  const isWorkspace = route.page === "workspace";
  // The two catalog pages ("Live pockets", "Drafts") share the shell
  // plumbing below (mounted-but-hidden panel, secondary management); which
  // record each shows is the panel's `page` prop, and which words lead the
  // page's title row follows the same route.
  const isCatalogPage = route.page === "live" || route.page === "drafts";
  const routeProjectId = isWorkspace ? route.projectId : null;

  // Column layout (pockets-design-update-v2.md §5, v3 §6/§8): the shell is
  // always columns — pockets sidebar, chat, divider, preview rail — and
  // never reflows panels on top of each other. The two side columns collapse
  // through the title-row toggles; the divider sets the preview width in px
  // or resets to the built-in ratio (`null`). Choices persist across
  // launches.
  const [layout, setLayout] = useState<LayoutState>(() => loadLayout());
  const toggleSidebar = useCallback(() => {
    setLayout((current) => {
      const next: LayoutState = {
        ...current,
        sidebarCollapsed: !current.sidebarCollapsed,
      };
      saveLayout(next);
      return next;
    });
  }, []);
  const togglePreview = useCallback(() => {
    setLayout((current) => {
      const next: LayoutState = {
        ...current,
        previewCollapsed: !current.previewCollapsed,
      };
      saveLayout(next);
      return next;
    });
  }, []);
  const setPreviewWidth = useCallback((width: number | null) => {
    setLayout((current) => {
      if (current.previewWidth === width) return current;
      const next: LayoutState = { ...current, previewWidth: width };
      saveLayout(next);
      return next;
    });
  }, []);

  // Divider geometry (v3 §6.2): the preview floor is 340px and the chat
  // floor 360px, so the live maximum follows the window width — with the
  // sidebar open the pockets track (248px + 1px border) is off the table
  // too. `window.innerWidth` is the honest stand-in for the workspace
  // track in both directions: the shell is the full window at every width.
  const [windowWidth, setWindowWidth] = useState(() => window.innerWidth);
  useEffect(() => {
    const onResize = () => setWindowWidth(window.innerWidth);
    window.addEventListener("resize", onResize);
    return () => window.removeEventListener("resize", onResize);
  }, []);
  // The live drag value stays out of `layout` (and out of storage) until the
  // interaction ends — a 60 Hz drag must not hammer localStorage (v3 §8.2).
  const [dragPreviewWidth, setDragPreviewWidth] = useState<number | null>(null);
  const workspaceWidth = Math.max(
    0,
    windowWidth - (layout.sidebarCollapsed ? 0 : 249),
  );
  const dividerMax = Math.max(
    PREVIEW_MIN_WIDTH,
    workspaceWidth - 1 - CHAT_MIN_WIDTH,
  );
  const clampPreviewWidth = useCallback(
    (value: number) =>
      Math.min(dividerMax, Math.max(PREVIEW_MIN_WIDTH, Math.round(value))),
    [dividerMax],
  );
  // Explicit width while set (persisted or mid-drag); otherwise the built-in
  // `1fr 1.2fr` ratio, mirrored in px for the divider's aria value and for
  // the width a drag starts from.
  const explicitPreviewWidth = dragPreviewWidth ?? layout.previewWidth;
  const dividerWidth = clampPreviewWidth(
    explicitPreviewWidth ?? ((workspaceWidth - 1) * 1.2) / 2.2,
  );
  const commitPreviewWidth = useCallback(
    (width: number | null) => {
      setDragPreviewWidth(null);
      setPreviewWidth(width);
    },
    [setPreviewWidth],
  );

  // Overlay counter (v3 §9): any popup that can reach the preview area
  // registers here; while the count is above 0 the native pane steps aside
  // (hidden exactly like a collapse — the session itself stays bound) and
  // comes back with the panel's freshest bounds when the count returns to
  // zero. The delete confirmation opened from the chat header is one of
  // these overlays and lives in App itself.
  const [overlayCount, setOverlayCount] = useState(0);
  const overlayApi = useMemo(
    () => ({
      setOverlay: (open: boolean) =>
        setOverlayCount((count) => (open ? count + 1 : Math.max(0, count - 1))),
    }),
    [],
  );
  const paneHidden = overlayCount > 0;

  // macOS full screen (v3 §2.3): the traffic lights move into the menu bar,
  // so title rows drop the 80px reserve — surfaced on `.app` for CSS and
  // read per-row by TitleBar itself.
  const fullscreen = useFullscreen();

  const [manifest, setManifest] = useState<LoadState<PocketManifest>>({
    status: "loading",
  });
  const [detection, setDetection] = useState<LoadState<DetectionReport>>({
    status: "loading",
  });
  const [pocket, setPocket] = useState<LoadState<PocketStatus>>({
    status: "loading",
  });
  const [refreshingRuntime, setRefreshingRuntime] = useState(false);
  const [refreshingStatus, setRefreshingStatus] = useState(false);
  // Only one lifecycle operation may be in flight at a time; `busy` below
  // serializes every user-initiated native call.
  const [operation, setOperation] = useState<PocketOperation | null>(null);
  const [operationError, setOperationError] = useState<string | null>(null);

  // Mirrors of the catalog CreationPanel publishes (its page stays mounted,
  // hidden on the workspace, so builds and lifecycle ops survive navigation).
  const [storedCatalog, setStoredCatalog] =
    useState<PocketCatalog>(LOADING_CATALOG);
  // Bumped after chat-side writes so CreationPanel refetches in place — a
  // `key` remount would destroy active builds, so the panel is never
  // remounted for this.
  const [refreshKey, setRefreshKey] = useState(0);
  // A native creation/link result is already registered. Retain it through
  // the catalog's in-flight/stale snapshots so opening it never mounts the
  // a missing-project screen while waiting for the list to catch up.
  const [developmentHandoff, setDevelopmentHandoff] = useState<Project | null>(
    null,
  );
  const catalog = useMemo(() => {
    if (!inTauri) return EMPTY_CATALOG;
    if (
      developmentHandoff !== null &&
      !storedCatalog.projects.some(
        (project) =>
          project.id === developmentHandoff.id && project.developmentLinked,
      )
    ) {
      return {
        ...storedCatalog,
        projects: [
          ...storedCatalog.projects.filter(
            (project) => project.id !== developmentHandoff.id,
          ),
          developmentHandoff,
        ],
      };
    }
    return storedCatalog;
  }, [storedCatalog, developmentHandoff]);

  const [workspaceControls, setWorkspaceControls] =
    useState<HTMLDivElement | null>(null);
  const [resourceMonitorOpen, setResourceMonitorOpen] = useState(false);
  const [tailscaleReport, setTailscaleReport] = useState<
    | { status: "idle" }
    | { status: "loading" }
    | { status: "error"; message: string }
    | { status: "ready"; value: TailscaleReport }
  >({ status: "idle" });
  const [refreshingTailscale, setRefreshingTailscale] = useState(false);
  const [usageByProject, setUsageByProject] = useState<
    Record<string, PocketUsage>
  >({});

  const applyResourceSnapshot = useCallback((snapshot: ResourceSnapshot) => {
    const next: Record<string, PocketUsage> = {};
    for (const entry of snapshot.pockets ?? []) {
      next[entry.id] = entry;
    }
    setUsageByProject(next);
  }, []);

  const busy = operation !== null || refreshingRuntime || refreshingStatus;
  const runtimeReady =
    detection.status === "ready" && detection.value.state === "ready";

  // Latest route for callbacks that may fire after the user has moved on
  // (a generation finishing while the workspace shows another page).
  const routeRef = useRef(route);
  routeRef.current = route;

  // The pocket the route points at, or null. `null` while the catalog is
  // loading is different from `null` after a trustworthy load: only the
  // latter means "known missing".
  const selectedProject =
    routeProjectId === null
      ? null
      : (catalog.projects.find((project) => project.id === routeProjectId) ??
        null);

  // While the sidebar is open, PocketSidebar's 5s poll feeds `usageByProject`
  // with zero duplicate IPC. When the sidebar is collapsed on a view that
  // renders live Pocket cards (Live pockets page or an installed workspace),
  // keep the same 5s cadence running here so per-card CPU/memory stay fresh.
  const needsFallbackUsagePoll =
    inTauri &&
    layout.sidebarCollapsed &&
    (route.page === "live" ||
      (isWorkspace && selectedProject?.phase === "installed"));
  useEffect(() => {
    if (!needsFallbackUsagePoll) return;
    let active = true;
    let timer: number | null = null;
    let inFlight = false;

    const schedule = () => {
      if (timer !== null) {
        window.clearTimeout(timer);
        timer = null;
      }
      if (document.visibilityState === "hidden") return;
      timer = window.setTimeout(() => void sample(), 5000);
    };

    const sample = async () => {
      if (!active || inFlight || document.visibilityState === "hidden") return;
      inFlight = true;
      try {
        const value = await invoke<ResourceSnapshot>("resource_snapshot");
        if (active) applyResourceSnapshot(value);
      } catch {
        // Deliberately silent: unmeasured metrics stay absent.
      } finally {
        inFlight = false;
        if (active) schedule();
      }
    };

    const onVisibility = () => {
      if (document.visibilityState === "hidden") {
        if (timer !== null) {
          window.clearTimeout(timer);
          timer = null;
        }
      } else {
        void sample();
      }
    };

    document.addEventListener("visibilitychange", onVisibility);
    void sample();
    return () => {
      active = false;
      if (timer !== null) window.clearTimeout(timer);
      document.removeEventListener("visibilitychange", onVisibility);
    };
  }, [needsFallbackUsagePoll, applyResourceSnapshot]);
  const catalogTrustworthy = !catalog.loading && catalog.error === null;
  const knownMissing =
    routeProjectId !== null && catalogTrustworthy && selectedProject === null;
  const previewProjectId = selectedProject?.developmentLinked
    ? selectedProject.id
    : null;
  const preview = useProjectPreview(previewProjectId);
  const previewRevisionRef = useRef<{
    projectId: string | null;
    revision: number | null;
  }>({ projectId: null, revision: null });
  useEffect(() => {
    const revision = preview.status?.revision ?? null;
    const previous = previewRevisionRef.current;
    previewRevisionRef.current = { projectId: previewProjectId, revision };
    if (
      previewProjectId !== null &&
      previous.projectId === previewProjectId &&
      previous.revision !== null &&
      revision !== null &&
      revision > previous.revision
    ) {
      setRefreshKey((value) => value + 1);
    }
  }, [previewProjectId, preview.status?.revision]);
  const handlePreviewBounds = usePreviewPane(
    layout.previewCollapsed ? null : previewProjectId,
    !paneHidden,
  );
  const previewPocketName = selectedProject?.manifest.metadata.name ?? null;
  const [overlaySnapshot, setOverlaySnapshot] = useState<string | null>(null);
  useEffect(() => {
    if (!paneHidden || previewProjectId === null || !inTauri) {
      setOverlaySnapshot(null);
      return;
    }
    let cancelled = false;
    void invoke<{ dataUrl: string } | null>("pocket_thumb", {
      projectId: previewProjectId,
    })
      .then((thumb) => {
        if (!cancelled) setOverlaySnapshot(thumb?.dataUrl ?? null);
      })
      .catch(() => {
        // No honest snapshot, no picture: PreviewPanel falls back to the
        // name on `--bg`.
        if (!cancelled) setOverlaySnapshot(null);
      });
    return () => {
      cancelled = true;
    };
  }, [paneHidden, previewProjectId]);

  const refreshPocketStatus = useCallback(async () => {
    setRefreshingStatus(true);
    try {
      const value = await invoke<PocketStatus>("pocket_status");
      setPocket({ status: "ready", value });
    } catch (err) {
      setPocket({ status: "error", message: errorMessage(err) });
    } finally {
      setRefreshingStatus(false);
    }
  }, []);

  const refreshRuntime = useCallback(async () => {
    setOperationError(null);
    setRefreshingRuntime(true);
    try {
      const value = await invoke<DetectionReport>("detect_runtime");
      setDetection({ status: "ready", value });
      // Pocket status is only meaningful while the runtime is ready, so a
      // runtime refresh doubles as the status refresh whenever it is.
      if (value.state === "ready") await refreshPocketStatus();
    } catch (err) {
      setDetection({ status: "error", message: errorMessage(err) });
    } finally {
      setRefreshingRuntime(false);
    }
  }, [refreshPocketStatus]);

  const bootstrapRuntime = useCallback(async () => {
    setOperationError(null);
    setRefreshingRuntime(true);
    try {
      const value = await invoke<DetectionReport>("bootstrap_runtime");
      setDetection({ status: "ready", value });
      if (value.state === "ready") await refreshPocketStatus();
    } catch (err) {
      setDetection({ status: "error", message: errorMessage(err) });
    } finally {
      setRefreshingRuntime(false);
    }
  }, [refreshPocketStatus]);

  const refreshTailscale = useCallback(async () => {
    setRefreshingTailscale(true);
    try {
      const value = await invoke<TailscaleReport>("detect_tailscale");
      setTailscaleReport({ status: "ready", value });
    } catch (err) {
      setTailscaleReport({ status: "error", message: errorMessage(err) });
    } finally {
      setRefreshingTailscale(false);
    }
  }, []);

  useEffect(() => {
    if (!inTauri) return;
    let cancelled = false;

    invoke<PocketManifest>("get_manifest")
      .then((value) => {
        if (!cancelled) setManifest({ status: "ready", value });
      })
      .catch((err) => {
        if (!cancelled)
          setManifest({ status: "error", message: errorMessage(err) });
      });

    void refreshRuntime();

    return () => {
      cancelled = true;
    };
  }, [refreshRuntime]);

  const runPocketOperation = useCallback(
    async (next: PocketOperation) => {
      setOperation(next);
      setOperationError(null);
      try {
        if (next === "opening") {
          // Native validates readiness and opens the URL itself; the
          // frontend never navigates to or links the returned URL.
          await invoke<void>(OPERATION_COMMANDS[next]);
        } else {
          const value = await invoke<PocketStatus>(OPERATION_COMMANDS[next]);
          setPocket({ status: "ready", value });
        }
      } catch (err) {
        setOperationError(errorMessage(err));
        // Re-read the real status so a started-but-unhealthy container can
        // still be stopped after a failed operation.
        await refreshPocketStatus();
      } finally {
        setOperation(null);
      }
    },
    [refreshPocketStatus],
  );

  const handleOperation = useCallback(
    (next: PocketOperation) => {
      void runPocketOperation(next);
    },
    [runPocketOperation],
  );

  const handleRefreshStatus = useCallback(() => {
    setOperationError(null);
    void refreshPocketStatus();
  }, [refreshPocketStatus]);

  const handleCatalogChange = useCallback((next: PocketCatalog) => {
    setDevelopmentHandoff((current) =>
      current !== null &&
      next.projects.some(
        (project) => project.id === current.id && project.developmentLinked,
      )
        ? null
        : current,
    );
    setStoredCatalog(next);
  }, []);

  const handleDevelopmentCreated = useCallback(
    (project: Project) => {
      setDevelopmentHandoff(project);
      setRefreshKey((key) => key + 1);
      const current = routeRef.current;
      if (current.page === "workspace" && current.projectId === null)
        selectProject(project.id);
    },
    [selectProject],
  );

  // The nav rail's Workspace entry returns to the pocket the user last had
  // open, and only while it is still a known project; otherwise it opens a
  // new chat. It is the only way back to the workspace from a catalog page —
  // the topbar breadcrumb no longer navigates.
  const lastProjectIdRef = useRef<string | null>(null);
  useEffect(() => {
    if (route.page === "workspace" && route.projectId !== null) {
      lastProjectIdRef.current = route.projectId;
    }
  }, [route]);

  const handleBackToWorkspace = useCallback(() => {
    const id = lastProjectIdRef.current;
    // While the catalog is untrustworthy its absence proves nothing, so
    // only a trustworthy load that lacks the id sends the user to a new
    // chat instead of the last pocket.
    const knownGone =
      id !== null &&
      catalogTrustworthy &&
      !catalog.projects.some((project) => project.id === id);
    if (id === null || knownGone) {
      newPocket();
    } else {
      selectProject(id);
    }
  }, [catalog.projects, catalogTrustworthy, selectProject, newPocket]);

  // Page keyboard shortcuts (v3 §3): ⌘1 returns to the workspace exactly
  // like the old rail entry did, ⌘2/⌘3 open the two catalog pages, ⌘N is
  // the sidebar's New pocket — the tooltips on those controls promise them,
  // so the app owes them. Only a bare ⌘ (no Ctrl/Alt) counts, so native
  // input editing stays intact.
  useEffect(() => {
    const onKey = (event: KeyboardEvent) => {
      if (!event.metaKey || event.ctrlKey || event.altKey) return;
      // A modal owns the window while an overlay is open (v3 §9): no
      // shortcut navigates out from under it.
      if (paneHidden) return;
      if (event.key === "1") {
        event.preventDefault();
        handleBackToWorkspace();
      } else if (event.key === "2") {
        event.preventDefault();
        showLivePockets();
      } else if (event.key === "3") {
        event.preventDefault();
        showDrafts();
      } else if (event.key === "n" || event.key === "N") {
        event.preventDefault();
        newPocket();
      } else if (event.key === "b" || event.key === "B") {
        event.preventDefault();
        toggleSidebar();
      }
    };
    window.addEventListener("keydown", onKey);
    return () => window.removeEventListener("keydown", onKey);
  }, [
    handleBackToWorkspace,
    showLivePockets,
    showDrafts,
    newPocket,
    toggleSidebar,
    paneHidden,
  ]);

  // Sidebar footer live count: only ever from trustworthy catalog statuses
  // (ready reads from `project_status`). No statuses read → nothing
  // claimed (the footer then shows the host alone); a host metric or
  // invented count never appears here.
  let knownStatuses = 0;
  let runningPockets = 0;
  for (const entry of Object.values(catalog.statuses)) {
    if (entry.status === "ready") {
      knownStatuses += 1;
      if (entry.value.state === "running" && entry.value.ready) {
        runningPockets += 1;
      }
    }
  }

  // A column that fell back to a placeholder still owns its slice of the
  // title row (v3 §1.4): the 52px band and its bottom border continue
  // across it, and — when the pockets column is collapsed and this is the
  // window's top-left — the sidebar toggle stays reachable from here. The
  // preview column is never the top-left, so its empty row reserves
  // nothing. Both are shared by the two ways a column can end up empty
  // (a route to a deleted pocket, and running outside the desktop app).
  const chatHeadEmpty = (
    <TitleBar
      ariaLabel="Chat"
      reserveTrafficLights={layout.sidebarCollapsed}
      leading={
        layout.sidebarCollapsed ? (
          <IconButton
            label="Toggle sidebar"
            title="Toggle sidebar ⌘B"
            onClick={toggleSidebar}
          >
            <IconPanelLeft />
          </IconButton>
        ) : undefined
      }
      // The preview toggle lives here only
      // while the preview is hidden, so a column showing a placeholder is
      // never a dead end — with the rail collapsed, this is its only way back
      // open.
      trailing={
        layout.previewCollapsed ? (
          <IconButton label="Toggle preview" onClick={togglePreview}>
            <IconPanelRight />
          </IconButton>
        ) : undefined
      }
    />
  );
  // The preview column without a session (or the browser fallback) still owns
  // its 52px slice of the row, and — like PreviewPanel itself while the rail
  // is open — carries the collapse control, since nothing else can reach it
  // from here.
  const previewHeadEmpty = (
    <TitleBar
      ariaLabel="Preview"
      trailing={
        <IconButton label="Toggle preview" pressed onClick={togglePreview}>
          <IconPanelRight />
        </IconButton>
      }
    />
  );

  return (
    <OverlayProvider value={overlayApi}>
      <FullscreenProvider value={fullscreen}>
        <div
          className="app"
          data-fullscreen={fullscreen ? "true" : undefined}
        >
          {/* Inside Tauri on macOS the native title bar is merged into the row
            (tauri.conf.json sets titleBarStyle "Overlay" with hiddenTitle),
            and since v3 §2 there is exactly one 52px title row across the
            window: each column's own `TitleBar` claims its slice of it. The
            row drags the window through `useTitlebarDrag` (explicit
            mousedown → `startDragging`, double click → maximize; the
            capability grants `core:window:allow-start-dragging`), and the
            traffic lights sit above the sidebar's column — every row that
            can be the window's top-left passes `reserveTrafficLights` and
            TitleBar applies the 80px inset only inside Tauri and only out
            of full screen. Outside Tauri nothing drags and nothing is
            reserved. */}
          {!inTauri && (
            <p className="notice">
              This browser preview cannot reach the local runtime. Open the
              Tauri desktop application with <code>npm run dev</code>.
            </p>
          )}

          <main className="body">
            {/* The v3 §1 shell: the pockets column and one content column that
            swaps between the workspace and the management page — the nav
            rail is gone, the two catalog pages navigate from the sidebar's
            section rows. Columns only — panels never stack on top of each
            other at any width; the two side columns collapse instead. */}
            <div
              className="shell"
              data-sidebar-collapsed={String(layout.sidebarCollapsed)}
            >
              {!layout.sidebarCollapsed && (
                <div className="sidebar">
                  {/* Pocket selection plus (v3 §3) the two section rows and the
                  host footer; runtime, lifecycle and monitoring live on the
                  management page, not in this column. */}
                  <PocketSidebar
                    catalog={catalog}
                    selectedProjectId={routeProjectId}
                    onSelectProject={selectProject}
                    onNewPocket={newPocket}
                    page={route.page}
                    onShowLive={showLivePockets}
                    onShowDrafts={showDrafts}
                    onToggleCollapsed={toggleSidebar}
                    hostLabel="pockets.local"
                    runningCount={knownStatuses > 0 ? runningPockets : null}
                    themeMode={themeMode}
                    onChangeThemeMode={changeThemeMode}
                    onResourceSnapshot={applyResourceSnapshot}
                  />
                </div>
              )}

              <div className="shell-main">
                {/* The management page's title row (v3 §5): the page name leads
                where the old crumb lived, the sidebar toggle appears only
                when the pockets column is collapsed (the sidebar's own row
                owns it otherwise), and — like the chat row — the traffic
                lights are reserved exactly when this column is the
                window's top-left. */}
                {isCatalogPage && (
                  <TitleBar
                    ariaLabel="Management"
                    reserveTrafficLights={layout.sidebarCollapsed}
                    leading={
                      layout.sidebarCollapsed ? (
                        <IconButton
                          label="Toggle sidebar"
                          title="Toggle sidebar ⌘B"
                          onClick={toggleSidebar}
                        >
                          <IconPanelLeft />
                        </IconButton>
                      ) : undefined
                    }
                    title={
                      <h1 className="page-head-title">
                        {route.page === "live" ? "Live pockets" : "Drafts"}
                      </h1>
                    }
                  />
                )}

                {/* The workspace page mounts and unmounts with the route: the
                chat and the native preview must never exist (or reserve a
                native rect) on the hidden page. */}
                {isWorkspace && (
                  <div
                    className="workspace"
                    data-preview-collapsed={String(layout.previewCollapsed)}
                    style={
                      !layout.previewCollapsed && explicitPreviewWidth !== null
                        ? {
                            gridTemplateColumns: `minmax(${CHAT_MIN_WIDTH}px, 1fr) 1px ${dividerWidth}px`,
                          }
                        : undefined
                    }
                  >
                    <section
                      className="pane pane-chat"
                      aria-label="Development"
                    >
                      {knownMissing ? (
                        /* The route points at a project the trustworthy catalog does
                   not contain: no stale chat, no stale preview — just honest
                   words and the two ways forward. The column still owns its
                   slice of the title row, because with the pockets column
                   collapsed this is the window's top-left and the only place
                   its toggle can live. */
                        <>
                          {chatHeadEmpty}
                          <div className="pane-placeholder">
                            <p className="placeholder-title">
                              Pocket not found
                            </p>
                            <p className="muted">
                              No loaded Pocket matches this address — it may
                              have been deleted.
                            </p>
                            <div className="actions placeholder-actions">
                              <button type="button" onClick={newPocket}>
                                New pocket
                              </button>
                              <button type="button" onClick={showLivePockets}>
                                Live pockets
                              </button>
                            </div>
                          </div>
                        </>
                      ) : inTauri && routeProjectId === null ? (
                        <NewProject
                          sidebarCollapsed={layout.sidebarCollapsed}
                          previewCollapsed={layout.previewCollapsed}
                          onToggleSidebar={toggleSidebar}
                          onTogglePreview={togglePreview}
                          onCreated={handleDevelopmentCreated}
                        />
                      ) : inTauri && selectedProject !== null ? (
                        <ExternalWorkspace
                          key={selectedProject.id}
                          projectId={selectedProject.id}
                          projectName={selectedProject.manifest.metadata.name}
                          linked={selectedProject.developmentLinked === true}
                          previewApiAccess={preview.apiAccess}
                          previewApis={selectedProject.manifest.previewApis}
                          building={catalog.buildingProjectId !== null}
                          sidebarCollapsed={layout.sidebarCollapsed}
                          previewCollapsed={layout.previewCollapsed}
                          onToggleSidebar={toggleSidebar}
                          onTogglePreview={togglePreview}
                          onSourceChanged={() =>
                            setRefreshKey((value) => value + 1)
                          }
                          controlsRef={setWorkspaceControls}
                        />
                      ) : inTauri ? (
                        <>
                          {chatHeadEmpty}
                          <div className="pane-placeholder">
                            <p className="placeholder-title">Loading project</p>
                            {catalog.error && (
                              <p className="error">{catalog.error}</p>
                            )}
                          </div>
                        </>
                      ) : (
                        <>
                          {chatHeadEmpty}
                          <div className="pane-placeholder">
                            <p className="placeholder-title">
                              Develop in Pockets
                            </p>
                            <p className="muted">
                              Create or link a project in the desktop
                              application, then edit its source in your
                              preferred coding tool.
                            </p>
                          </div>
                        </>
                      )}
                    </section>

                    {/* The divider (v3 §6) sits between chat and preview and
                collapses with the column. It only ever reports widths
                inside the live clamp, and the value lands in storage when
                the interaction ends — never mid-drag. Collapsing the column
                unmounts the panel and drops the route-effective session to
                null, which hides the native pane;
                the preview session itself stays bound, so expanding brings
                the same preview back. */}
                    {!layout.previewCollapsed && (
                      <>
                        <ColumnDivider
                          width={dividerWidth}
                          min={PREVIEW_MIN_WIDTH}
                          max={dividerMax}
                          onResize={setDragPreviewWidth}
                          onCommit={commitPreviewWidth}
                        />
                        <section
                          className="pane pane-preview preview-rail"
                          aria-label="Preview"
                        >
                          {inTauri ? (
                            <PreviewPanel
                              preview={preview}
                              newProject={routeProjectId === null}
                              onBounds={handlePreviewBounds}
                              onTogglePreview={togglePreview}
                              overlayOpen={paneHidden}
                              pocketName={previewPocketName}
                              snapshotUrl={overlaySnapshot}
                            />
                          ) : (
                            <>
                              {previewHeadEmpty}
                              <div className="pane-placeholder">
                                <p className="placeholder-title">
                                  No pocket selected
                                </p>
                                <p className="muted">
                                  The live preview is a native pane inside the
                                  desktop application.
                                </p>
                              </div>
                            </>
                          )}
                        </section>
                      </>
                    )}
                  </div>
                )}

                {/* The All-pockets management page. The host stays mounted on the
            workspace (hidden, not unmounted) so CreationPanel keeps active
            builds, lifecycle polls and dialogs alive and keeps publishing
            the catalog; only its visibility follows the route. */}
                <div className="pockets-host" hidden={!isCatalogPage}>
                  <div className="pockets-page">
                    <div className="pockets-primary">
                      {inTauri ? (
                        <CreationPanel
                          runtimeReady={runtimeReady}
                          page={route.page === "drafts" ? "drafts" : "live"}
                          onCatalogChange={handleCatalogChange}
                          onSelectProject={selectProject}
                          onNewPocket={newPocket}
                          refreshKey={refreshKey}
                          usageByProject={usageByProject}
                          workspace={
                            selectedProject && workspaceControls
                              ? {
                                  project: selectedProject,
                                  host: workspaceControls,
                                }
                              : undefined
                          }
                        />
                      ) : (
                        <div className="pane-placeholder">
                          <p className="placeholder-title">No pocket catalog</p>
                          <p className="muted">
                            The pocket catalog reads the local runtime and is
                            only available in the Tauri desktop application.
                          </p>
                        </div>
                      )}
                    </div>

                    {/* Secondary management: real, but one deliberate click away
                under the grid (pockets-design-handoff.md: not why the
                window is open). Mounted only while this page shows, so no
                background polling follows the user into the workspace.
                Appearance is the one pure-frontend block, so it also
                carries the theme override outside Tauri (v2 §9 — no
                command palette exists, so this is where the manual
                System / Light / Dark choice is offered and persisted). */}
                    {isCatalogPage && (
                      <section
                        className="pockets-secondary"
                        aria-label="Pocket management"
                      >
                        <details className="pane-details">
                          <summary>Appearance</summary>
                          <div className="appearance">
                            <p className="hint">
                              Follow the system theme, or pick light or dark.
                              The choice is remembered.
                            </p>
                            <Segmented
                              ariaLabel="Theme"
                              value={themeMode}
                              onChange={(id) => {
                                if (
                                  id === "system" ||
                                  id === "light" ||
                                  id === "dark"
                                ) {
                                  changeThemeMode(id);
                                }
                              }}
                              items={[
                                { id: "system", label: "System" },
                                { id: "light", label: "Light" },
                                { id: "dark", label: "Dark" },
                              ]}
                            />
                          </div>
                        </details>
                        {inTauri && (
                          <>
                            <details className="pane-details">
                              <summary>Container runtime</summary>
                              <RuntimePanel
                                detection={detection}
                                refreshing={refreshingRuntime}
                                busy={busy}
                                onRefresh={refreshRuntime}
                                onBootstrap={bootstrapRuntime}
                              />
                            </details>
                            <details
                              className="pane-details"
                              onToggle={(event) => {
                                if (
                                  event.currentTarget.open &&
                                  tailscaleReport.status === "idle"
                                ) {
                                  void refreshTailscale();
                                }
                              }}
                            >
                              <summary>Tailscale</summary>
                              <TailscaleStatusPanel
                                report={tailscaleReport}
                                refreshing={refreshingTailscale}
                                onRefresh={() => void refreshTailscale()}
                              />
                            </details>
                            <details className="pane-details">
                              <summary>Example Pocket</summary>
                              <PocketPanel
                                manifest={manifest}
                                pocket={pocket}
                                runtimeReady={runtimeReady}
                                runtimeChecking={detection.status === "loading"}
                                refreshingStatus={refreshingStatus}
                                busy={busy}
                                operation={operation}
                                operationError={operationError}
                                onOperation={handleOperation}
                                onRefreshStatus={handleRefreshStatus}
                              />
                            </details>
                            <details
                              className="pane-details"
                              open={resourceMonitorOpen}
                              onToggle={(event) =>
                                setResourceMonitorOpen(event.currentTarget.open)
                              }
                            >
                              <summary>Resource monitor</summary>
                              <ResourceMonitor
                                controlledOpen={resourceMonitorOpen}
                                embedded
                              />
                            </details>
                          </>
                        )}
                      </section>
                    )}
                  </div>
                </div>
              </div>
            </div>
          </main>
          <FolderPickerModal />
          <AuthGateModal />
        </div>
      </FullscreenProvider>
    </OverlayProvider>
  );
}

function RuntimePanel({
  detection,
  refreshing,
  busy,
  onRefresh,
  onBootstrap,
}: {
  detection: LoadState<DetectionReport>;
  refreshing: boolean;
  busy: boolean;
  onRefresh: () => void;
  onBootstrap: () => void;
}) {
  return (
    <section className="card" aria-labelledby="runtime-heading">
      <div className="card-top">
        <h2 id="runtime-heading" className="visually-hidden">
          Container runtime
        </h2>
        <div
          className="runtime-status"
          aria-live="polite"
          aria-busy={refreshing}
        >
          {detection.status === "loading" && (
            <p className="muted">Checking for Podman…</p>
          )}
          {detection.status === "error" && (
            <p className="error">
              Could not check the runtime. {detection.message}
            </p>
          )}
          {detection.status === "ready" && (
            <p>
              <span className={`badge badge-${detection.value.state}`}>
                {STATE_LABELS[detection.value.state]}
              </span>
            </p>
          )}
        </div>
        <div className="actions">
          {detection.status === "ready" &&
            detection.value.state === "unavailable" && (
              <button
                type="button"
                className="button-primary"
                onClick={onBootstrap}
                disabled={busy}
              >
                {refreshing ? "Starting…" : "Start Podman"}
              </button>
            )}
          <button type="button" onClick={onRefresh} disabled={busy}>
            {refreshing ? "Checking…" : "Refresh"}
          </button>
        </div>
      </div>
      {detection.status === "ready" && (
        <details className="card-details">
          <summary>detail</summary>
          <p className="muted">{detection.value.message}</p>
        </details>
      )}
    </section>
  );
}

function PocketPanel({
  manifest,
  pocket,
  runtimeReady,
  runtimeChecking,
  refreshingStatus,
  busy,
  operation,
  operationError,
  onOperation,
  onRefreshStatus,
}: {
  manifest: LoadState<PocketManifest>;
  pocket: LoadState<PocketStatus>;
  runtimeReady: boolean;
  runtimeChecking: boolean;
  refreshingStatus: boolean;
  busy: boolean;
  operation: PocketOperation | null;
  operationError: string | null;
  onOperation: (operation: PocketOperation) => void;
  onRefreshStatus: () => void;
}) {
  const status = pocket.status === "ready" ? pocket.value : null;
  const running = status?.state === "running";
  const canStart =
    !busy &&
    manifest.status === "ready" &&
    runtimeReady &&
    status?.state === "stopped";
  const canOpen =
    !busy && status !== null && status.state === "running" && status.ready;
  // Native Stop validates container ownership itself, so it stays available
  // even when status inspection failed (e.g. an unsafe or missing port
  // mapping) and the container is known to be Pockets-owned. Start and Open
  // stay disabled until a trustworthy status is available.
  const canStop =
    !busy &&
    runtimeReady &&
    (status?.state === "running" || pocket.status === "error");
  const canRefresh = !busy && runtimeReady && pocket.status !== "loading";

  return (
    <section className="card" aria-labelledby="pocket-heading">
      <h2 id="pocket-heading" className="visually-hidden">
        Example Pocket
      </h2>

      {manifest.status === "loading" && (
        <p className="muted">Loading the bundled Pocket…</p>
      )}
      {manifest.status === "error" && (
        <p className="error">
          Could not load the bundled Pocket manifest. {manifest.message}
        </p>
      )}
      {manifest.status === "ready" && (
        <>
          <div className="pocket-heading">
            <h3 className="pocket-name">{manifest.value.metadata.name}</h3>
            <span className="chip">v{manifest.value.metadata.version}</span>
          </div>
          <p className="pocket-description">
            {manifest.value.metadata.description}
          </p>
          {/* Manifest facts and the volume note are reference detail: they
              belong on the management page but not in its first screenful. */}
          <details className="card-details">
            <summary>pocket details</summary>
            <dl className="facts">
              <dt>Identifier</dt>
              <dd>
                <code>{manifest.value.metadata.id}</code>
              </dd>
              <dt>Image</dt>
              <dd>
                <code>{manifest.value.runtime.image}</code>
              </dd>
              <dt>Architectures</dt>
              <dd>{manifest.value.runtime.architectures.join(", ")}</dd>
              <dt>Web</dt>
              <dd>
                port <code>{manifest.value.web.port}</code>, healthcheck{" "}
                <code>{manifest.value.web.healthcheck}</code>
              </dd>
              <dt>Storage</dt>
              <dd>
                volume <code>{manifest.value.storage.name}</code> mounted at{" "}
                <code>{manifest.value.storage.path}</code>
              </dd>
            </dl>
            <p className="note">
              The <code>{manifest.value.storage.name}</code> volume is a
              dedicated local persistent volume: the Pocket&apos;s data survives
              container restarts.
            </p>
          </details>
        </>
      )}

      <div className="lifecycle" aria-live="polite" aria-busy={busy}>
        {!runtimeReady && (
          <p className="hint">
            {runtimeChecking
              ? "Checking the container runtime before querying the Pocket…"
              : "Pocket controls appear once the container runtime is ready."}
          </p>
        )}
        {runtimeReady && pocket.status === "loading" && (
          <p className="hint">Checking the Pocket…</p>
        )}
        {runtimeReady && pocket.status === "error" && (
          <>
            <p className="error">
              Could not load the Pocket status. {pocket.message}
            </p>
            <p className="hint">
              You can try Stop to safely stop a Pockets-owned container;
              unrelated containers will not be changed.
            </p>
          </>
        )}
        {runtimeReady && status && (
          <p>
            <span className={`badge badge-${status.state}`}>
              {running ? "Running" : "Stopped"}
            </span>{" "}
            {status.message}
            {running && !status.ready && (
              <span className="muted"> (not answering HTTP yet)</span>
            )}
          </p>
        )}
        {runtimeReady && status?.ready && status.url && (
          <p>
            <code>{status.url}</code>
          </p>
        )}
        {operation && (
          <p className="hint strong">
            {OPERATION_LABELS[operation]}
            {operation === "starting" &&
              " — waiting for the Pocket's HTTP health check, which can take about 30 seconds."}
          </p>
        )}
        {operationError && (
          <p className="error">The operation failed. {operationError}</p>
        )}

        {runtimeReady && (
          <div className="actions">
            <button
              type="button"
              onClick={() => onOperation("starting")}
              disabled={!canStart}
            >
              {operation === "starting" ? OPERATION_LABELS.starting : "Start"}
            </button>
            <button
              type="button"
              onClick={() => onOperation("opening")}
              disabled={!canOpen}
            >
              {operation === "opening" ? OPERATION_LABELS.opening : "Open"}
            </button>
            <button
              type="button"
              onClick={() => onOperation("stopping")}
              disabled={!canStop}
            >
              {operation === "stopping" ? OPERATION_LABELS.stopping : "Stop"}
            </button>
            <button
              type="button"
              onClick={onRefreshStatus}
              disabled={!canRefresh}
            >
              {refreshingStatus ? "Refreshing…" : "Refresh"}
            </button>
          </div>
        )}
        {runtimeReady && (
          <p className="hint">
            Closing this window does not stop a running Pocket — use Stop.
            Stopping keeps the Pocket&apos;s dedicated volume, so its data is
            preserved.
          </p>
        )}
      </div>
    </section>
  );
}
