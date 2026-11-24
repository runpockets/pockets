import { useCallback, useEffect, useState } from "react";

/**
 * Hash route hook for the desktop shell.
 *
 * The app is served through a Tauri custom protocol, so deep links must ride
 * the URL fragment (a path change would hit the protocol handler); the router
 * is therefore a small hand-rolled hash parser with no package dependency.
 *
 * Routes:
 * - `#/`          external-first project creation (`projectId: null`)
 * - `#/p/:id`     selected-pocket workspace with an opaque native project id
 * - `#/live`      live-pockets management (installed records)
 * - `#/drafts`    drafts management (draft records + the new-pocket cell)
 * - `#/pockets`   legacy hash for the single all-pockets page — lands on
 *                 Live pockets so old bookmarks still open a real page
 */
export type PocketRoute =
  | { page: "workspace"; projectId: string | null }
  | { page: "live" }
  | { page: "drafts" };

const NEW_POCKET_HASH = "#/";
const LIVE_HASH = "#/live";
const DRAFTS_HASH = "#/drafts";
const PROJECT_HASH_PREFIX = "#/p/";

/** Route the shell falls back to for anything it cannot trust. */
const SAFE_FALLBACK_ROUTE: PocketRoute = {
  page: "workspace",
  projectId: null,
};

/**
 * Whether two routes describe the same place. `readPocketRoute` returns a
 * fresh object on every call (except the fallback), so this is the equality
 * the hook needs to keep `route` — and everything keyed on it — stable when
 * an event or a repeat navigation changes nothing.
 */
function sameRoute(current: PocketRoute, next: PocketRoute): boolean {
  if (current.page !== next.page) return false;
  return current.page === "workspace" && next.page === "workspace"
    ? current.projectId === next.projectId
    : true;
}

/**
 * Decode one path segment to an opaque project id, or `null` when malformed.
 * The decoded id is only ever carried as data (never used for navigation), so
 * any non-empty decode is acceptable and round-trips back through
 * `encodeURIComponent` when written to the hash.
 */
function decodeProjectId(segment: string): string | null {
  if (segment === "") return null;
  try {
    return decodeURIComponent(segment);
  } catch {
    return null;
  }
}

/**
 * Parse a location hash (with or without the leading `#`) into a route.
 * Malformed encodings, missing ids and extra path segments all fall back to
 * the safe new-pocket workspace; no input can produce an arbitrary target.
 */
export function readPocketRoute(hash: string): PocketRoute {
  const path = hash.startsWith("#") ? hash.slice(1) : hash;
  if (path === "" || path === "/") return SAFE_FALLBACK_ROUTE;
  if (!path.startsWith("/")) return SAFE_FALLBACK_ROUTE;

  const segments = path.slice(1).split("/");
  if (segments.length === 1 && segments[0] === "live") {
    return { page: "live" };
  }
  // The pre-split single page (`#/pockets`) survives as an alias for Live
  // pockets rather than falling back to the workspace: it always led with
  // the running records, and a stale bookmark should land somewhere real.
  if (segments.length === 1 && segments[0] === "pockets") {
    return { page: "live" };
  }
  if (segments.length === 1 && segments[0] === "drafts") {
    return { page: "drafts" };
  }
  if (segments.length === 2 && segments[0] === "p") {
    const projectId = decodeProjectId(segments[1]);
    if (projectId !== null) return { page: "workspace", projectId };
  }
  return SAFE_FALLBACK_ROUTE;
}

/** Current route plus stable navigation callbacks for the shell chrome. */
export interface PocketRouteController {
  route: PocketRoute;
  /** Open the selected-pocket workspace for a native project id. */
  selectProject: (id: string) => void;
  /** Open the new-pocket workspace. */
  newPocket: () => void;
  /** Open the live-pockets management page. */
  showLivePockets: () => void;
  /** Open the drafts management page. */
  showDrafts: () => void;
}

/**
 * Track the current hash route. Navigation writes `window.location.hash` and
 * also updates state immediately, because the `hashchange` event fires on a
 * later task and may not fire at all when the hash is unchanged. A `hashchange`
 * listener keeps back/forward navigation in sync.
 */
export function usePocketRoute(): PocketRouteController {
  const [route, setRoute] = useState<PocketRoute>(() =>
    readPocketRoute(window.location.hash),
  );

  useEffect(() => {
    const onHashChange = () => {
      setRoute((current) => {
        const next = readPocketRoute(window.location.hash);
        // `readPocketRoute` builds a fresh object every call, so without this
        // an event that changed nothing still re-renders the whole shell —
        // and re-runs every effect keyed on `route`. An event that lands
        // late (jsdom queues `hashchange` as a task, so a test run can
        // deliver a pile of them at once) must be free.
        return sameRoute(current, next) ? current : next;
      });
    };
    window.addEventListener("hashchange", onHashChange);
    return () => window.removeEventListener("hashchange", onHashChange);
  }, []);

  const navigate = useCallback((hash: string) => {
    // Read the target hash back rather than the event: it is canonical for
    // the route even when no `hashchange` will fire (same-hash navigation).
    window.location.hash = hash;
    setRoute((current) => {
      const next = readPocketRoute(hash);
      return sameRoute(current, next) ? current : next;
    });
  }, []);

  const selectProject = useCallback(
    (id: string) => {
      // An empty id is not a project; navigating there would produce a hash
      // that cannot decode back to a workspace, so ignore it.
      if (id === "") return;
      navigate(PROJECT_HASH_PREFIX + encodeURIComponent(id));
    },
    [navigate],
  );

  const newPocket = useCallback(() => navigate(NEW_POCKET_HASH), [navigate]);

  const showLivePockets = useCallback(() => navigate(LIVE_HASH), [navigate]);

  const showDrafts = useCallback(() => navigate(DRAFTS_HASH), [navigate]);

  return { route, selectProject, newPocket, showLivePockets, showDrafts };
}
