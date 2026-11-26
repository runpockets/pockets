import {
  useCallback,
  useEffect,
  useRef,
  useState,
  type KeyboardEvent as ReactKeyboardEvent,
} from "react";
import { invoke, listen } from "./api";
import { createPortal } from "react-dom";
import RevisionHistory from "./RevisionHistory";
import UpstreamUpdates from "./UpstreamUpdates";
import Versions from "./Versions";
import { formatBytes, formatPercent } from "./formatBytes";
import { useOverlay } from "./OverlayContext";
import type { PocketUsage } from "./PocketSidebar";
import { type PipTone } from "./Status";
import { IconExternalLink, IconPlus } from "./Icons";
import { FileIcon } from "./FileIcon";
import {
  QrCodeSvg,
  type TailscaleReport,
  type TailscaleShare,
} from "./TailscaleShare";
import "./CreationPanel.css";

/** Source-controlled preview API declaration from `pocket.yaml`. */
export interface PreviewApi {
  host: string;
  name: string;
  purpose: string;
}

export interface RuntimeVariable {
  name: string;
  kind: "plain" | "secret";
  purpose: string;
  required?: boolean;
}

/** Manifest embedded in a persisted project record. */
export interface ProjectManifest {
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
  previewApis?: PreviewApi[];
  runtimeVariables?: RuntimeVariable[];
}

/**
 * Persisted project record returned by the native `list_projects` command,
 * by `import_project`, and by `build_project`. `containerfile` and `files`
 * describe the imported source snapshot, not the user's live folder.
 *
 * `manifest`, `sourceDigest` and `files` are the *editable working* source:
 * chat writes revise it in place. `installed` pins the *runtime* separately
 * — what is actually built and running. When `installed.sourceDigest` differs
 * from `sourceDigest`, the working source has pending changes and the card
 * offers Update (which rebuilds and replaces the same Pocket's installed
 * version; the data volume is never reverted). Legacy records built before
 * the pin exist without `installed`: their snapshot is assumed current until
 * native stamps a pin. `builtImageId` remains the legacy image mirror.
 */
export interface Project {
  id: string;
  developmentLinked?: boolean;
  /** True when the linked development folder on disk differs from Pockets'
   *  saved snapshot (and therefore from the current installed build). */
  unsyncedChanges?: boolean;
  manifest: ProjectManifest;
  phase: "draft" | "installed";
  /** Parent lineage, stamped natively only when this draft was imported as
   *  a copy of an installed/built pocket: that parent record's id. Absent
   *  on ordinary drafts, never present on installed records. */
  editedFrom?: string | null;
  builtImageId: string | null;
  /** The pinned installed runtime, or null/undefined on drafts and legacy
   *  records that predate the pin. */
  installed?: {
    imageId: string;
    manifest: ProjectManifest;
    sourceDigest: string;
    revisionId?: string | null;
    builtAtSeconds?: number | null;
  } | null;
  files: string[];
  totalBytes: number;
  containerfile: string;
  sourceDigest: string;
}

function pocketLogoInitial(name: string): string {
  const trimmed = name.trim();
  const alnum = /[A-Za-z0-9]/.exec(trimmed);
  if (alnum) return alnum[0].toUpperCase();
  const first = Array.from(trimmed)[0];
  return first ? first.toUpperCase() : "P";
}

function PocketCardLogo({
  projectId,
  name,
  cacheKey,
}: {
  projectId: string;
  name: string;
  cacheKey: string;
}) {
  const [state, setState] = useState<"loading" | "loaded" | "missing">(
    "loading",
  );
  const src = `/api/projects/${encodeURIComponent(projectId)}/favicon?v=${encodeURIComponent(cacheKey)}`;

  useEffect(() => {
    setState("loading");
  }, [src]);

  return (
    <span
      className="creation-card-logo"
      data-loaded={state === "loaded" ? "true" : undefined}
      aria-hidden="true"
    >
      {state !== "loaded" && (
        <span className="creation-card-logo-letter">
          {pocketLogoInitial(name)}
        </span>
      )}
      {state !== "missing" && (
        <img
          className="creation-card-logo-img"
          src={src}
          alt=""
          data-loaded={state === "loaded" ? "true" : undefined}
          onLoad={() => setState("loaded")}
          onError={() => setState("missing")}
        />
      )}
    </span>
  );
}

/** True when a record has an installed runtime: the new `installed` pin or
 *  the legacy `builtImageId` mirror. Drafts have neither. */
function isBuilt(project: Project): boolean {
  return project.installed != null || project.builtImageId !== null;
}

/**
 * True when the working source moved past the installed snapshot (chat
 * edited it since the last build/Update). A built record with no `installed`
 * pin is treated as current until the backend stamps one, so legacy fixtures
 * never show a phantom pending state. Unbuilt records are never "pending" —
 * their action is the first Build.
 */
function hasPendingChanges(project: Project): boolean {
  if (!isBuilt(project) || project.installed == null) return false;
  return project.installed.sourceDigest !== project.sourceDigest;
}

/** The image the Pocket currently runs from: the pin first, legacy mirror
 *  as fallback. Null when nothing is built. */
function installedImageId(project: Project): string | null {
  return project.installed?.imageId ?? project.builtImageId;
}

/**
 * Build progress returned by the native `project_build_status` command. The
 * log is native-owned, already truncated when oversized, and rendered as
 * escaped text only.
 */
interface BuildStatus {
  state: "idle" | "running" | "succeeded" | "failed";
  log: string;
  truncated: boolean;
  error: string | null;
}

/** Panel-local view state for the one build operation allowed at a time. */
interface ActiveBuild {
  id: string;
  running: boolean;
  status: BuildStatus | null;
  error: string | null;
}

/**
 * Lifecycle status returned by `project_status`, `start_project` and
 * `stop_project`. `url` is the installed Pocket's local site address.
 * Navigation goes through the native `open_project` command.
 */
export interface PocketStatus {
  state: "stopped" | "running";
  url: string | null;
  message: string;
  ready: boolean;
  tailscale?: TailscaleShare | null;
}

type TailscaleReportState =
  | { status: "idle" }
  | { status: "loading" }
  | { status: "error"; message: string }
  | { status: "ready"; value: TailscaleReport };

/** User-initiated lifecycle operations; one across the panel at a time. */
type LifecycleOperation = "starting" | "opening" | "stopping";

const LIFECYCLE_LABELS: Record<LifecycleOperation, string> = {
  starting: "Starting…",
  opening: "Opening…",
  stopping: "Stopping…",
};

/**
 * The same word with the Pocket's name inside it — `Starting Notes Pocket…` —
 * for the one panel-level line (see `panelLifecycle`). Derived from
 * {@link LIFECYCLE_LABELS} rather than a second list of words, so the card's
 * label and the panel's line can never drift apart.
 */
function namedLifecycle(operation: LifecycleOperation, name: string): string {
  return `${LIFECYCLE_LABELS[operation].replace(/…$/, "")} ${name}…`;
}

/** Why starting takes a moment, said once in one set of words: the card's
 *  per-card label and the panel-level line both append it. */
const STARTING_NOTE =
  " — waiting for the Pocket's HTTP health check, which can take about 30 seconds.";

export type LoadState<T> =
  | { status: "loading" }
  | { status: "error"; message: string }
  | { status: "ready"; value: T };

/**
 * Truthful catalog snapshot published through `onCatalogChange` whenever the
 * panel's catalog state moves. Nothing here is invented: `projects` is the
 * latest `list_projects` result (empty while loading or on error), `statuses`
 * only holds entries for projects that exist and have been built (drafts
 * never have one), `buildingProjectId` is the id of the one running build,
 * and `loading`/`error` mirror the list request itself. The panel owns the
 * data; consumers (the App's pocket-selection sidebar) only read this.
 */
export interface PocketCatalog {
  projects: Project[];
  statuses: Record<string, LoadState<PocketStatus>>;
  loading: boolean;
  error: string | null;
  buildingProjectId: string | null;
}

function errorMessage(err: unknown): string {
  if (typeof err === "string") return err;
  if (err instanceof Error) return err.message;
  return "The request failed for an unknown reason.";
}

const FOCUSABLE_SELECTOR =
  'button:not(:disabled), [href], input:not(:disabled), select:not(:disabled), textarea:not(:disabled), [tabindex]:not([tabindex="-1"])';

/** Keeps Tab / Shift+Tab cycling inside an open modal dialog. */
function trapModalFocus(event: ReactKeyboardEvent<HTMLElement>) {
  if (event.key !== "Tab") return;
  const focusable = Array.from(
    event.currentTarget.querySelectorAll<HTMLElement>(FOCUSABLE_SELECTOR),
  );
  if (focusable.length === 0) return;
  const first = focusable[0];
  const last = focusable[focusable.length - 1];
  if (event.shiftKey && document.activeElement === first) {
    event.preventDefault();
    last.focus();
  } else if (!event.shiftKey && document.activeElement === last) {
    event.preventDefault();
    first.focus();
  }
}

/** Shortens a content digest for display; the full value stays in storage. */
function shortDigest(digest: string): string {
  return digest.replace(/^sha256:/, "").slice(0, 12);
}

/** Shortens an image id for the technical-details line; the full id stays in
 *  native storage and a user can still copy it from there. */
function shortImageId(imageId: string): string {
  const bare = imageId.replace(/^sha256:/, "");
  return bare.length <= 12 ? bare : `${bare.slice(0, 12)}…`;
}

/**
 * The card caption for a stored capture: when the picture is from, at the
 * precision a glance needs ("Captured 3h ago", "Captured 18 Nov"). The full
 * moment rides on the element's `title` (see `formatCapturedFull`).
 * A capture stamped in the future (clock skew) reads as "just now".
 */
export function formatCapturedAt(capturedAt: number, now = Date.now()): string {
  const seconds = Math.max(0, Math.round((now - capturedAt) / 1000));
  if (seconds < 60) return "Captured just now";
  const minutes = Math.floor(seconds / 60);
  if (minutes < 60) return `Captured ${minutes} min ago`;
  const hours = Math.floor(minutes / 60);
  if (hours < 24) return `Captured ${hours}h ago`;
  if (hours < 48) return "Captured yesterday";
  const date = new Date(capturedAt);
  const sameYear = date.getFullYear() === new Date(now).getFullYear();
  return `Captured ${date.toLocaleDateString(undefined, {
    day: "numeric",
    month: "short",
    ...(sameYear ? {} : { year: "numeric" }),
  })}`;
}

/** The exact capture moment to the minute, for the tooltip and the
 *  enlarged view ("21 Nov 2026, 5:14 pm"). */
export function formatCapturedFull(capturedAt: number): string {
  return new Date(capturedAt).toLocaleString(undefined, {
    dateStyle: "medium",
    timeStyle: "short",
  });
}

/** CPU above this share of one core is called out next to the status. */
const HIGH_CPU_PERCENT = 25;

/** A Pocket address without the scheme: "pocket-x.localhost:18788". */
export function displayAddress(url: string): string {
  return url.replace(/^https?:\/\//, "").replace(/\/$/, "");
}

function IconCopy() {
  return (
    <svg
      width="14"
      height="14"
      viewBox="0 0 24 24"
      fill="none"
      stroke="currentColor"
      strokeWidth="2"
      aria-hidden="true"
    >
      <rect x="9" y="9" width="11" height="11" />
      <path d="M5 15V4h11" />
    </svg>
  );
}

function IconCheckSmall() {
  return (
    <svg
      width="14"
      height="14"
      viewBox="0 0 24 24"
      fill="none"
      stroke="currentColor"
      strokeWidth="2.5"
      aria-hidden="true"
    >
      <path d="M5 12l5 5L19 7" />
    </svg>
  );
}

/** Three small squares: the card's ⋯ menu. */
function IconMore() {
  return (
    <svg
      width="16"
      height="16"
      viewBox="0 0 24 24"
      fill="currentColor"
      aria-hidden="true"
    >
      <rect x="4" y="11" width="3" height="3" />
      <rect x="10.5" y="11" width="3" height="3" />
      <rect x="17" y="11" width="3" height="3" />
    </svg>
  );
}

/** Formats the installed pin's real build timestamp (local date + time)
 *  for the meta row. */
export function formatBuiltDate(seconds: number): string {
  return new Date(seconds * 1000).toLocaleString(undefined, {
    dateStyle: "short",
    timeStyle: "short",
  });
}

/** The stored card thumbnail as `pocket_thumb` returns it: an inline PNG
 *  data URL plus the moment it was captured — native reads that from the
 *  stored file's own mtime, so it names when the picture is *from* and a
 *  thumb written minutes ago cannot claim to be now. */
interface ThumbInfo {
  dataUrl: string;
  capturedAt: number;
}

/** The card thumbnail frozen for the enlarge overlay. Opening the overlay
 *  is a view of one specific stored capture, so it carries that capture's
 *  bytes and timestamp rather than tracking the live `thumbs` map — if a
 *  refresh lands a newer capture while the overlay is open, the overlay
 *  keeps showing (and dating) the picture it was opened with. */
interface EnlargedCapture {
  projectId: string;
  name: string;
  dataUrl: string;
  capturedAt: number;
}

/** Only accepts what a card may render honestly: a PNG data URL (no other
 *  scheme ever lands in `src`, mirroring the CSP that already allows just
 *  `self` and `data:`) and a real timestamp. A missing thumb, a failed
 *  command, or a malformed answer all collapse to "no thumb", which draws
 *  no figure at all — absence is the only honest fallback. */
function isThumbInfo(value: unknown): value is ThumbInfo {
  if (typeof value !== "object" || value === null) return false;
  const thumb = value as ThumbInfo;
  return (
    typeof thumb.dataUrl === "string" &&
    thumb.dataUrl.startsWith("data:image/png;base64,") &&
    Number.isFinite(thumb.capturedAt) &&
    Number.isFinite(new Date(thumb.capturedAt).getTime())
  );
}

export interface CreationPanelProps {
  runtimeReady: boolean;
  /**
   * Which management page this panel is: "live" lists the installed
   * records, "drafts" lists the draft records and closes the grid with the
   * dashed new-pocket cell. The two pages are separate routes with their
   * own AppNav entries; a record can only ever appear on the one its
   * persisted `phase` names, so the split shows no more and no less than
   * the catalog holds.
   */
  page: "live" | "drafts";
  /**
   * Receives a fresh {@link PocketCatalog} whenever the catalog state moves
   * (list load, lifecycle statuses, build start/finish). Published from an
   * effect, never during render, and only when the content actually changed,
   * so an unstable callback identity can never cause a loop.
   */
  onCatalogChange?: (catalog: PocketCatalog) => void;
  /**
   * When provided, every card renders a "Manage" action that calls this
   * with the card's project id (App switches the workspace onto that Pocket).
   */
  onSelectProject?: (id: string) => void;
  /**
   * When provided, the header's primary New pocket button and — on the
   * Drafts page — the dashed new-pocket cell both render; the cell stays
   * last even when the page holds no draft records (the grid keeps the
   * new-pocket cell, v2 §8). A new pocket becomes a draft, so the cell
   * belongs to the Drafts page only. These view actions call back without arguments and
   * never invoke native. Omit the prop and neither renders.
   */
  onNewPocket?: () => void;
  /**
   * Bump to refetch the catalog without remounting the panel — in-flight
   * builds, status polling and open dialogs survive. Used after chat-side
   * writes (a finished generation lands a new Draft) where a `key` remount
   * would destroy active state.
   */
  refreshKey?: number;
  /** Latest measured per-Pocket resource usage keyed by project id, fed from
   *  the shell's shared `resource_snapshot` poll. Absent entries render no
   *  metrics — never fabricated zeros. */
  usageByProject?: Record<string, PocketUsage>;
  workspace?: { project: Project; host: HTMLElement };
}

/**
 * A Pocket management page (pockets-design-update-v2.md §8), in a straight
 * Add → Build → Open line: import a source snapshot, build it with one
 * click (native packages it and records it as installed), then run the
 * managed Visit/Stop workflow. The `page` prop picks which half of the
 * catalog this route shows — "live" lists the installed records, "drafts"
 * lists the draft records and ends with the new-pocket cell — each in its
 * own responsive card grid in catalog order; search is intentionally not
 * shipped yet. With `onNewPocket` the header
 * gains its primary button and the Drafts grid always ends in the dashed
 * new-pocket cell. A draft whose persisted `editedFrom` names an installed
 * record in this catalog says so in one muted caption on its card ("Edited
 * from <parent name>"); nothing resolves, nothing is claimed. The
 * destructive delete keeps its typed-name confirmation for installed
 * pockets; a draft confirms with the Delete Pocket button alone. Update applies the
 * edited working source to the same Pocket's installed version (data is
 * never reverted); settings and uninstall remain out of scope by design.
 *
 * The panel is the single owner of catalog state and stays mounted while the
 * App hides its page; it publishes {@link PocketCatalog} snapshots through
 * `onCatalogChange` for the pocket-selection sidebar to read.
 */
export default function CreationPanel({
  runtimeReady,
  page,
  onCatalogChange,
  onSelectProject,
  onNewPocket,
  refreshKey = 0,
  usageByProject,
  workspace,
}: CreationPanelProps) {
  const { setOverlay } = useOverlay();
  const [projects, setProjects] = useState<LoadState<Project[]>>({
    status: "loading",
  });
  const [filterQuery, setFilterQuery] = useState("");
  const [listing, setListing] = useState(true);
  const [importing, setImporting] = useState(false);
  const [linking, setLinking] = useState(false);
  const [importError, setImportError] = useState<string | null>(null);
  const [activeBuild, setActiveBuild] = useState<ActiveBuild | null>(null);
  // Lifecycle status per built project, plus the one lifecycle operation
  // allowed across the panel at a time and its error.
  const [statuses, setStatuses] = useState<
    Record<string, LoadState<PocketStatus>>
  >({});
  // The pocket's display `name` rides along so the operation can be stated
  // in words even after its card has left this page — a fresh draft's card is
  // gone the moment its build promotes the record, and the chained start still
  // has to be said somewhere (see `panelLifecycle`).
  const [lifecycle, setLifecycle] = useState<{
    id: string;
    name: string;
    operation: LifecycleOperation;
    openAfterStart?: boolean;
  } | null>(null);
  const [lifecycleError, setLifecycleError] = useState<{
    id: string;
    /** Display name, carried for the panel-level line (see
     *  `panelLifecycleError`); it never crosses to native. */
    name: string;
    operation: LifecycleOperation;
    message: string;
    /** True when the failed operation is the chained open after this page's
     *  own successful build, which is the one case that may say the Pocket is
     *  already built and installed. */
    afterBuild?: boolean;
  } | null>(null);
  // Destructive delete with a confirmation whose strength follows the
  // persisted phase: an installed pocket follows the GitHub-repo pattern
  // (the exact name must be typed), a draft confirms with the Delete Pocket
  // button alone — the typed gate guards the pocket a user depends on, not
  // the drafts page's working material. The targeted record, what the user
  // has typed, and the state of the one delete allowed at a time. Opening
  // the modal invokes nothing — only an explicit confirm ever reaches native.
  const [deleteTarget, setDeleteTarget] = useState<Project | null>(null);
  const [deleteConfirm, setDeleteConfirm] = useState("");
  const [deleting, setDeleting] = useState(false);
  const [deleteError, setDeleteError] = useState<string | null>(null);
  const [resetTarget, setResetTarget] = useState<Project | null>(null);
  const [resetting, setResetting] = useState(false);
  const [resetError, setResetError] = useState<string | null>(null);
  const [restoreTarget, setRestoreTarget] = useState<Project | null>(null);
  const [restoringData, setRestoringData] = useState(false);
  const [restoreDataError, setRestoreDataError] = useState<string | null>(null);
  const [exportingDataId, setExportingDataId] = useState<string | null>(null);
  const [dataFeedback, setDataFeedback] = useState<{
    id: string;
    tone: "ok" | "error";
    message: string;
  } | null>(null);
  const [tailscaleBusyId, setTailscaleBusyId] = useState<string | null>(null);
  const [tailscaleError, setTailscaleError] = useState<{
    id: string;
    message: string;
  } | null>(null);
  const [tailscaleReport, setTailscaleReport] = useState<TailscaleReportState>({
    status: "idle",
  });
  const [qrTarget, setQrTarget] = useState<{
    name: string;
    url: string;
  } | null>(null);
  const [qrCopied, setQrCopied] = useState(false);
  // The confirmation's strength follows the record's persisted phase: only
  // an installed pocket pays the typed-name gate; a draft's confirmation is
  // satisfied by the Delete Pocket button alone. Read by the confirm guard
  // and the dialog alike, so the button can never unlock a gate that is not
  // being asked for.
  const deleteNeedsName =
    deleteTarget !== null && deleteTarget.phase !== "draft";
  // An unbuilt draft has never had a container or a data volume, so the
  // warning names only what actually dies with it: the record and its
  // snapshot. A legacy built draft keeps the full warning — it can own a
  // volume like any built Pocket.
  const deleteIsUnbuiltDraft =
    deleteTarget !== null &&
    deleteTarget.phase === "draft" &&
    !isBuilt(deleteTarget);

  // Monotonic run token: every new build (and unmount) invalidates the
  // previous run's in-flight status reads and settle handlers, so a late
  // response can never overwrite — or regress — fresher progress.
  const buildSeqRef = useRef(0);
  const buildTimerRef = useRef<number | null>(null);
  // Same idea for lifecycle statuses: bumped by every authoritative write and
  // bulk load so older bulk results cannot clobber fresher command results.
  const statusSeqRef = useRef(0);

  const buildRunning = activeBuild?.running === true;
  const lifecycleRunning = lifecycle !== null;
  const busy =
    listing ||
    importing ||
    linking ||
    buildRunning ||
    lifecycleRunning ||
    deleting ||
    resetting ||
    restoringData ||
    exportingDataId !== null ||
    tailscaleBusyId !== null;

  const stopPolling = useCallback(() => {
    if (buildTimerRef.current !== null) {
      window.clearInterval(buildTimerRef.current);
      buildTimerRef.current = null;
    }
  }, []);

  // Card thumbnails keyed by project id; null means nothing honest to
  // show — never fetched, absent, failed, or malformed are all the same
  // quiet no-figure card. `pocket_thumb` is fire-safe and read-only, so
  // one batch per list load is enough: the caption carries the stored
  // file's own mtime, which keeps an older picture truthful until the
  // next capture replaces it.
  const [thumbs, setThumbs] = useState<Record<string, ThumbInfo | null>>({});
  // The capture currently enlarged in the overlay (see
  // `EnlargedCapture`), or null when no overlay is mounted. Display data
  // frozen at click time — closing is local state only, and nothing about
  // opening or closing ever talks to native.
  const [enlarged, setEnlarged] = useState<EnlargedCapture | null>(null);
  // Same monotonic-guard idiom as statuses: a superseded batch drops whole,
  // it never merges into (or regresses) fresher state.
  const thumbSeqRef = useRef(0);

  function pickFresherThumb(
    current: ThumbInfo | null | undefined,
    incoming: ThumbInfo | null,
  ): ThumbInfo | null {
    if (incoming === null) return null;
    if (current && current.capturedAt > incoming.capturedAt) return current;
    return incoming;
  }

  const loadThumbs = useCallback(async (list: Project[]) => {
    const seq = ++thumbSeqRef.current;
    // allSettled by design: one project's failed read must not blank the
    // thumbs the other reads returned, and a rejecting mock or command is
    // just that project's "no thumb".
    const settled = await Promise.allSettled(
      list.map((project) =>
        invoke<ThumbInfo | null>("pocket_thumb", { projectId: project.id }),
      ),
    );
    if (seq !== thumbSeqRef.current) return;
    setThumbs((prev) => {
      const next: Record<string, ThumbInfo | null> = {};
      settled.forEach((entry, index) => {
        const id = list[index].id;
        const value = entry.status === "fulfilled" ? entry.value : null;
        next[id] = pickFresherThumb(
          prev[id],
          isThumbInfo(value) ? value : null,
        );
      });
      return next;
    });
  }, []);

  // Live thumbnail push from native captures (start, visit, update, preview):
  // those captures run asynchronously after a command returns, so pushing the
  // stored file when it finishes updates cards immediately without polling.
  useEffect(() => {
    if (typeof window === "undefined" || !("__TAURI_INTERNALS__" in window)) {
      return;
    }
    let active = true;
    let unlisten: (() => void) | null = null;
    void listen<{ projectId?: unknown; thumb?: unknown }>(
      "pocket-thumb-updated",
      (event) => {
        if (!active) return;
        const projectId = event.payload?.projectId;
        const thumb = event.payload?.thumb;
        if (typeof projectId !== "string" || !isThumbInfo(thumb)) return;
        setThumbs((prev) => ({
          ...prev,
          [projectId]: pickFresherThumb(prev[projectId], thumb),
        }));
      },
    )
      .then((stop) => {
        if (active) unlisten = stop;
        else stop();
      })
      .catch(() => {
        // Advisory push only; catalog refreshes still read the stored thumb.
      });
    return () => {
      active = false;
      unlisten?.();
    };
  }, []);

  const refresh = useCallback(
    async (silent = false) => {
      if (!silent) setListing(true);
      try {
        const value = await invoke<Project[]>("list_projects");
        setProjects({ status: "ready", value });
        // Thumbs ride the catalog refresh: every path that re-reads the list
        // (mount, refreshKey, explicit Refresh, a finished build) re-reads
        // them too, so a fresh capture shows up without inventing its own
        // polling loop. Never awaited on — a slow thumb read must not hold
        // up the list that is already on screen.
        void loadThumbs(value);
      } catch (err) {
        if (!silent) {
          setProjects({ status: "error", message: errorMessage(err) });
        }
      } finally {
        if (!silent) setListing(false);
      }
    },
    [loadThumbs],
  );

  // Only mounted inside the in-Tauri branch of App, so invoke always works.
  // Bumping `refreshKey` refetches in place — a remount is not needed for
  // chat-side writes to show up, so active builds and dialogs survive.
  useEffect(() => {
    void refresh();
  }, [refresh, refreshKey]);

  // Re-check linked folders silently when the window regains focus or tab
  // becomes visible (e.g. switching back from an external editor or SSH
  // session) so unsynced local edits surface right away without disabling
  // buttons or background polling.
  useEffect(() => {
    if (typeof window === "undefined") {
      return;
    }
    const onFocus = () => {
      void refresh(true);
    };
    const onVisibility = () => {
      if (document.visibilityState === "visible") {
        void refresh(true);
      }
    };
    window.addEventListener("focus", onFocus);
    document.addEventListener("visibilitychange", onVisibility);
    return () => {
      window.removeEventListener("focus", onFocus);
      document.removeEventListener("visibilitychange", onVisibility);
    };
  }, [refresh]);

  // Publish the catalog whenever its content moves. The callback lives
  // outside the dependency set (kept in a ref) and the effect short-circuits
  // on an unchanged content signature, so neither an unstable callback nor
  // the 1s build polling can publish duplicates or loop.
  const onCatalogChangeRef = useRef(onCatalogChange);
  useEffect(() => {
    onCatalogChangeRef.current = onCatalogChange;
  });
  const publishedSignatureRef = useRef<string | null>(null);
  useEffect(() => {
    const ready = projects.status === "ready" ? projects.value : [];
    // Truthful by construction: statuses only ship for projects that still
    // exist, so a deleted Pocket never lingers in the published view.
    const publishedStatuses: Record<string, LoadState<PocketStatus>> = {};
    for (const project of ready) {
      const entry = statuses[project.id];
      if (entry !== undefined) publishedStatuses[project.id] = entry;
    }
    const catalog: PocketCatalog = {
      projects: ready,
      statuses: publishedStatuses,
      loading: listing,
      error: projects.status === "error" ? projects.message : null,
      buildingProjectId: activeBuild?.running === true ? activeBuild.id : null,
    };
    const signature = JSON.stringify(catalog);
    if (signature === publishedSignatureRef.current) return;
    publishedSignatureRef.current = signature;
    onCatalogChangeRef.current?.(catalog);
  }, [projects, statuses, listing, activeBuild]);

  // Unmount: invalidate any in-flight build and status reads and stop the
  // build-log interval.
  useEffect(
    () => () => {
      buildSeqRef.current += 1;
      statusSeqRef.current += 1;
      stopPolling();
    },
    [stopPolling],
  );

  /** Authoritative status write that invalidates older bulk reads. */
  const applyStatus = useCallback(
    (id: string, entry: LoadState<PocketStatus>) => {
      statusSeqRef.current += 1;
      setStatuses((current) => ({ ...current, [id]: entry }));
    },
    [],
  );

  const loadStatus = useCallback(
    async (id: string) => {
      try {
        const value = await invoke<PocketStatus>("project_status", { id });
        applyStatus(id, { status: "ready", value });
      } catch (err) {
        applyStatus(id, { status: "error", message: errorMessage(err) });
      }
    },
    [applyStatus],
  );

  /** Bulk status refresh for built projects; each record fails on its own. */
  const refreshStatuses = useCallback(async (ids: string[]) => {
    if (ids.length === 0) return;
    const seq = ++statusSeqRef.current;
    setStatuses((current) => {
      const next = { ...current };
      for (const id of ids) next[id] = { status: "loading" };
      return next;
    });
    const results = await Promise.allSettled(
      ids.map((id) => invoke<PocketStatus>("project_status", { id })),
    );
    // An authoritative command result (or newer load) superseded this read.
    if (seq !== statusSeqRef.current) return;
    setStatuses((current) => {
      const next = { ...current };
      results.forEach((result, index) => {
        next[ids[index]] =
          result.status === "fulfilled"
            ? { status: "ready", value: result.value }
            : { status: "error", message: errorMessage(result.reason) };
      });
      return next;
    });
  }, []);

  const handleImport = useCallback(async () => {
    setImporting(true);
    setImportError(null);
    try {
      // Native opens the folder picker itself; the webview never supplies a
      // path. `null` means the user cancelled and nothing was persisted.
      const imported = await invoke<Project | null>("import_project");
      if (imported) {
        // Each import persists a new record with its own id; re-importing an
        // edited folder does not replace an earlier snapshot. A fresh list
        // lands the new record in the right group without assuming its
        // contents.
        await refresh();
      }
    } catch (err) {
      setImportError(errorMessage(err));
    } finally {
      setImporting(false);
    }
  }, [refresh]);

  const handleLinkProject = useCallback(async () => {
    setLinking(true);
    setImportError(null);
    try {
      const linked = await invoke<Project | null>("link_development_folder", {
        id: null,
      });
      if (linked) await refresh();
    } catch (err) {
      setImportError(errorMessage(err));
    } finally {
      setLinking(false);
    }
  }, [refresh]);

  /**
   * Open as one action: `start_project` starts if needed and waits for the
   * HTTP health check, then `open_project` has native re-validate health and
   * open the system browser. The webview sends only the project ID.
   *
   * The one implementation behind both the card's Open button and the chained
   * open a fresh draft's Build and start runs, so the two paths cannot drift:
   * same commands, same busy/progress, same re-read on failure. `name` is
   * display data only — it lets the panel state the operation while this page
   * no longer renders the record as a card (see `panelLifecycle`) — and
   * `afterBuild` marks the chained case, the one place allowed to say the
   * Pocket is already built and installed.
   */
  const startAndOpen = useCallback(
    async (id: string, name: string, afterBuild = false) => {
      setLifecycle({ id, name, operation: "starting", openAfterStart: true });
      setLifecycleError(null);
      let operation: LifecycleOperation = "starting";
      try {
        const started = await invoke<PocketStatus>("start_project", { id });
        applyStatus(id, { status: "ready", value: started });
        operation = "opening";
        setLifecycle({ id, name, operation, openAfterStart: true });
        await invoke<void>("open_project", { id });
        setLifecycle(null);
        // Refetch the persisted record, then re-read the live status, so the
        // card reflects the native state of record after the round trip.
        await refresh();
        await refreshStatuses([id]);
      } catch (err) {
        setLifecycle(null);
        setLifecycleError({
          id,
          name,
          operation,
          message: errorMessage(err),
          afterBuild,
        });
        // Re-read the real status so a started-but-unhealthy Pocket can
        // still be stopped after a failed operation.
        await loadStatus(id);
      }
    },
    [applyStatus, loadStatus, refresh, refreshStatuses],
  );

  /** The card's Open button: the shared sequence, for the record in hand. */
  const handleOpen = useCallback(
    (project: Project) =>
      startAndOpen(project.id, project.manifest.metadata.name),
    [startAndOpen],
  );

  /** Starts a stopped Pocket without opening it in a browser tab. */
  const handleStart = useCallback(
    async (project: Project) => {
      const id = project.id;
      setLifecycle({
        id,
        name: project.manifest.metadata.name,
        operation: "starting",
        openAfterStart: false,
      });
      setLifecycleError(null);
      try {
        const value = await invoke<PocketStatus>("start_project", { id });
        applyStatus(id, { status: "ready", value });
      } catch (err) {
        setLifecycleError({
          id,
          name: project.manifest.metadata.name,
          operation: "starting",
          message: errorMessage(err),
        });
        await loadStatus(id);
      } finally {
        setLifecycle(null);
      }
    },
    [applyStatus, loadStatus],
  );

  /**
   * Starts the one build operation allowed at a time — the same command
   * serves a fresh draft's Build and start and a built Pocket's Update. The
   * webview sends only the project id; native packages the current working
   * source with Podman, owns the build, and resolves with the persisted
   * installed record — a resolve is always a success and a rejection always a
   * failure. An Update replaces the same Pocket's installed image and its
   * pin; the data volume is untouched. Progress comes from independent 1s
   * `project_build_status` polling, not from this promise.
   *
   * `openAfter` is what makes a fresh draft one action instead of two: on
   * success it runs the same open sequence the card's Open button uses, so one
   * click builds, installs, starts and opens. Native build only installs, so
   * the start is this frontend chaining its own existing commands — ids still
   * the only thing that crosses. An Update passes false: applying a new
   * version to a Pocket the user is looking at must not hijack their browser.
   */
  const handleBuild = useCallback(
    async (project: Project, openAfter: boolean) => {
      const seq = ++buildSeqRef.current;
      stopPolling();
      setActiveBuild({
        id: project.id,
        running: true,
        status: null,
        error: null,
      });

      // Only one status read is ever in flight, so responses arrive in order
      // and the displayed log cannot regress within a run.
      let inFlight = false;
      // Polling keeps running for as long as the build promise is pending.
      // Until this run has been seen idle or running, a terminal snapshot
      // may still be the previous run's status — native keeps it until the
      // new build actually starts — so it is neither displayed nor treated
      // as the end of polling. The settle-time one-shot read is what freezes
      // the final log.
      let observedActive = false;
      const poll = async () => {
        if (inFlight || seq !== buildSeqRef.current) return;
        inFlight = true;
        try {
          const status = await invoke<BuildStatus>("project_build_status", {
            id: project.id,
          });
          if (seq !== buildSeqRef.current) return;
          if (status.state === "idle" || status.state === "running") {
            observedActive = true;
          } else if (!observedActive) {
            // Stale terminal status from an earlier run: keep polling and
            // wait for this run's idle/running progress instead of showing
            // an old failure as if it were the new one.
            return;
          }
          // Ignore anything after the run settles so the final progress
          // stays final.
          setActiveBuild((current) =>
            current && current.id === project.id && current.running
              ? { ...current, status }
              : current,
          );
        } catch {
          // A transient status-read failure is not a build failure; the next
          // tick simply retries.
        } finally {
          inFlight = false;
        }
      };
      buildTimerRef.current = window.setInterval(() => void poll(), 1000);
      void poll();

      // Terminal helper: stop polling, take one last status read (freshest
      // log, never stale), then freeze the panel at that progress.
      const finish = async (error: string | null) => {
        stopPolling();
        if (seq !== buildSeqRef.current) return;
        let status: BuildStatus | null = null;
        try {
          status = await invoke<BuildStatus>("project_build_status", {
            id: project.id,
          });
        } catch {
          // Keep the last polled progress if the final read fails.
        }
        if (seq !== buildSeqRef.current) return;
        setActiveBuild((current) =>
          current && current.id === project.id
            ? {
                ...current,
                running: false,
                status: status ?? current.status,
                error,
              }
            : current,
        );
      };

      try {
        const builtProject = await invoke<Project>("build_project", {
          id: project.id,
        });
        await finish(null);
        // The record is persisted as installed; refetch so the summary and
        // the catalog pages reflect the native state of record.
        if (seq === buildSeqRef.current) await refresh();
        // One action, one button: a fresh draft's click keeps going and opens
        // what it just installed. The refetch above already moved the record
        // to the installed page, so the shared sequence's own status writes
        // are what the Live card shows, and the panel-level line covers this
        // page in the meantime. A settled-or-superseded run never opens: a
        // failed build or a newer operation leaves the record where it is.
        if (openAfter && seq === buildSeqRef.current) {
          await startAndOpen(
            project.id,
            builtProject?.manifest?.metadata?.name ??
              project.manifest.metadata.name,
            true,
          );
        }
      } catch (err) {
        await finish(errorMessage(err));
      }
    },
    [refresh, startAndOpen, stopPolling],
  );

  const handleStop = useCallback(
    async (project: Project) => {
      const id = project.id;
      setLifecycle({
        id,
        name: project.manifest.metadata.name,
        operation: "stopping",
      });
      setLifecycleError(null);
      try {
        const value = await invoke<PocketStatus>("stop_project", { id });
        applyStatus(id, { status: "ready", value });
      } catch (err) {
        setLifecycleError({
          id,
          name: project.manifest.metadata.name,
          operation: "stopping",
          message: errorMessage(err),
        });
        await loadStatus(id);
      } finally {
        setLifecycle(null);
      }
    },
    [applyStatus, loadStatus],
  );

  const deleteReturnFocusRef = useRef<HTMLElement | null>(null);
  const resetReturnFocusRef = useRef<HTMLElement | null>(null);
  const restoreReturnFocusRef = useRef<HTMLElement | null>(null);
  const enlargeReturnFocusRef = useRef<HTMLElement | null>(null);
  const qrReturnFocusRef = useRef<HTMLElement | null>(null);

  // Reset catalog search when switching between Live pockets and Drafts.
  useEffect(() => {
    setFilterQuery("");
  }, [page]);

  // Step the native preview pane aside while any modal dialog is open (e.g.,
  // deleting, restoring, resetting, or showing a Tailscale QR code).
  const modalOpen =
    deleteTarget !== null ||
    resetTarget !== null ||
    restoreTarget !== null ||
    enlarged !== null ||
    qrTarget !== null;
  useEffect(() => {
    if (!modalOpen) return;
    setOverlay(true);
    return () => setOverlay(false);
  }, [modalOpen, setOverlay]);

  /**
   * Opens the delete confirmation. Nothing is invoked here: the destructive
   * command runs only after an explicit confirm — the typed Pocket name for
   * an installed record, the Delete Pocket button alone for a draft.
   */
  const handleAskDelete = useCallback((project: Project) => {
    deleteReturnFocusRef.current =
      document.activeElement instanceof HTMLElement
        ? document.activeElement
        : null;
    setDeleteTarget(project);
    setDeleteConfirm("");
    setDeleteError(null);
  }, []);

  /** Closes the confirmation — but never mid-delete, when native has already
   *  begun tearing the Pocket down and the outcome must be seen. */
  const handleCloseDelete = useCallback(() => {
    if (deleting) return;
    setDeleteTarget(null);
    setDeleteConfirm("");
    setDeleteError(null);
    const opener = deleteReturnFocusRef.current;
    deleteReturnFocusRef.current = null;
    if (opener && opener.isConnected) opener.focus();
  }, [deleting]);

  /**
   * Permanently deletes the confirmed Pocket. Only the id crosses to native,
   * which stops and removes the container, removes the data volume with all
   * the data the Pocket stored, and deletes the record and source snapshot.
   * A Pocket folder imported from elsewhere on the computer is never
   * touched — the snapshot inside Pockets is what dies here.
   */
  const handleConfirmDelete = useCallback(async () => {
    if (deleteTarget === null || deleting) return;
    if (
      deleteNeedsName &&
      deleteConfirm.trim() !== deleteTarget.manifest.metadata.name
    ) {
      return;
    }
    setDeleting(true);
    setDeleteError(null);
    try {
      await invoke("delete_project", { id: deleteTarget.id });
      setDeleteTarget(null);
      setDeleteConfirm("");
      // The card disappearing comes from the refetched list, never from an
      // optimistic splice: native is the only source of what exists.
      await refresh();
    } catch (err) {
      setDeleteError(errorMessage(err));
    } finally {
      setDeleting(false);
    }
  }, [deleteConfirm, deleteNeedsName, deleteTarget, deleting, refresh]);

  // Escape closes the confirmation like Cancel does; mid-delete, neither can
  // dismiss what is already happening natively.
  useEffect(() => {
    if (deleteTarget === null) return;
    const onKey = (event: KeyboardEvent) => {
      if (event.key === "Escape") handleCloseDelete();
    };
    window.addEventListener("keydown", onKey);
    return () => window.removeEventListener("keydown", onKey);
  }, [deleteTarget, handleCloseDelete]);

  const handleExportData = useCallback(async (project: Project) => {
    setExportingDataId(project.id);
    setDataFeedback(null);
    try {
      const savedName = await invoke<string | null>("export_project_data", {
        id: project.id,
      });
      if (savedName !== null) {
        setDataFeedback({
          id: project.id,
          tone: "ok",
          message: `Backup saved to ${savedName}.`,
        });
      }
    } catch (err) {
      setDataFeedback({
        id: project.id,
        tone: "error",
        message: errorMessage(err),
      });
    } finally {
      setExportingDataId(null);
    }
  }, []);

  const handleAskResetData = useCallback((project: Project) => {
    resetReturnFocusRef.current =
      document.activeElement instanceof HTMLElement
        ? document.activeElement
        : null;
    setResetTarget(project);
    setResetError(null);
  }, []);

  const handleCloseResetData = useCallback(() => {
    if (resetting) return;
    setResetTarget(null);
    setResetError(null);
    const opener = resetReturnFocusRef.current;
    resetReturnFocusRef.current = null;
    if (opener && opener.isConnected) opener.focus();
  }, [resetting]);

  const handleConfirmResetData = useCallback(async () => {
    if (resetTarget === null || resetting) return;
    const target = resetTarget;
    setResetting(true);
    setResetError(null);
    setDataFeedback(null);
    try {
      const status = await invoke<PocketStatus>("reset_project_data", {
        id: target.id,
      });
      applyStatus(target.id, { status: "ready", value: status });
      setResetTarget(null);
      setDataFeedback({
        id: target.id,
        tone: "ok",
        message:
          "Data reset. Podman will create a fresh volume the next time this Pocket starts.",
      });
      const opener = resetReturnFocusRef.current;
      resetReturnFocusRef.current = null;
      if (opener && opener.isConnected) opener.focus();
    } catch (err) {
      setResetError(errorMessage(err));
      await loadStatus(target.id);
    } finally {
      setResetting(false);
    }
  }, [applyStatus, loadStatus, resetTarget, resetting]);

  useEffect(() => {
    if (resetTarget === null) return;
    const onKey = (event: KeyboardEvent) => {
      if (event.key === "Escape") handleCloseResetData();
    };
    window.addEventListener("keydown", onKey);
    return () => window.removeEventListener("keydown", onKey);
  }, [resetTarget, handleCloseResetData]);

  const handleAskRestoreData = useCallback((project: Project) => {
    restoreReturnFocusRef.current =
      document.activeElement instanceof HTMLElement
        ? document.activeElement
        : null;
    setRestoreTarget(project);
    setRestoreDataError(null);
  }, []);

  const handleCloseRestoreData = useCallback(() => {
    if (restoringData) return;
    setRestoreTarget(null);
    setRestoreDataError(null);
    const opener = restoreReturnFocusRef.current;
    restoreReturnFocusRef.current = null;
    if (opener && opener.isConnected) opener.focus();
  }, [restoringData]);

  const handleConfirmRestoreData = useCallback(async () => {
    if (restoreTarget === null || restoringData) return;
    const target = restoreTarget;
    setRestoringData(true);
    setRestoreDataError(null);
    setDataFeedback(null);
    try {
      const result = await invoke<{
        fileName: string;
        status: PocketStatus;
      } | null>("restore_project_data", {
        id: target.id,
      });
      if (result === null) {
        // User cancelled the native file picker; keep the dialog closed cleanly.
        setRestoreTarget(null);
        const opener = restoreReturnFocusRef.current;
        restoreReturnFocusRef.current = null;
        if (opener && opener.isConnected) opener.focus();
        return;
      }
      applyStatus(target.id, { status: "ready", value: result.status });
      setRestoreTarget(null);
      setDataFeedback({
        id: target.id,
        tone: "ok",
        message: `Backup restored from ${result.fileName}. Start this Pocket to use the restored data.`,
      });
      const opener = restoreReturnFocusRef.current;
      restoreReturnFocusRef.current = null;
      if (opener && opener.isConnected) opener.focus();
    } catch (err) {
      setRestoreDataError(errorMessage(err));
      await loadStatus(target.id);
    } finally {
      setRestoringData(false);
    }
  }, [applyStatus, loadStatus, restoreTarget, restoringData]);

  useEffect(() => {
    if (restoreTarget === null) return;
    const onKey = (event: KeyboardEvent) => {
      if (event.key === "Escape") handleCloseRestoreData();
    };
    window.addEventListener("keydown", onKey);
    return () => window.removeEventListener("keydown", onKey);
  }, [restoreTarget, handleCloseRestoreData]);

  // Closing the enlarge overlay is pure local state: one click (or Escape,
  // same idiom as the delete confirmation) and the card view returns; the
  // frozen capture it was showing goes with it.
  const handleCloseEnlarge = useCallback(() => {
    setEnlarged(null);
    const opener = enlargeReturnFocusRef.current;
    enlargeReturnFocusRef.current = null;
    if (opener && opener.isConnected) opener.focus();
  }, []);
  useEffect(() => {
    if (enlarged === null) return;
    const onKey = (event: KeyboardEvent) => {
      if (event.key === "Escape") handleCloseEnlarge();
    };
    window.addEventListener("keydown", onKey);
    return () => window.removeEventListener("keydown", onKey);
  }, [enlarged, handleCloseEnlarge]);

  const handleCheckTailscale = useCallback(async () => {
    setTailscaleReport({ status: "loading" });
    try {
      const value = await invoke<TailscaleReport>("detect_tailscale");
      setTailscaleReport({ status: "ready", value });
    } catch (err) {
      setTailscaleReport({ status: "error", message: errorMessage(err) });
    }
  }, []);

  const handleToggleTailscale = useCallback(
    async (project: Project, enabled: boolean) => {
      setTailscaleBusyId(project.id);
      setTailscaleError(null);
      try {
        const value = await invoke<PocketStatus>("set_project_tailscale", {
          id: project.id,
          enabled,
        });
        applyStatus(project.id, { status: "ready", value });
      } catch (err) {
        setTailscaleError({ id: project.id, message: errorMessage(err) });
        await loadStatus(project.id);
      } finally {
        setTailscaleBusyId(null);
      }
    },
    [applyStatus, loadStatus],
  );

  const handleShowTailscaleQr = useCallback((project: Project, url: string) => {
    qrReturnFocusRef.current =
      document.activeElement instanceof HTMLElement
        ? document.activeElement
        : null;
    setQrCopied(false);
    setQrTarget({ name: project.manifest.metadata.name, url });
  }, []);

  const handleCloseQr = useCallback(() => {
    setQrTarget(null);
    setQrCopied(false);
    const opener = qrReturnFocusRef.current;
    qrReturnFocusRef.current = null;
    if (opener && opener.isConnected) opener.focus();
  }, []);

  useEffect(() => {
    if (qrTarget === null) return;
    const onKey = (event: KeyboardEvent) => {
      if (event.key === "Escape") handleCloseQr();
    };
    window.addEventListener("keydown", onKey);
    return () => window.removeEventListener("keydown", onKey);
  }, [qrTarget, handleCloseQr]);

  useEffect(() => {
    if (!qrCopied) return;
    const timer = window.setTimeout(() => setQrCopied(false), 1500);
    return () => window.clearTimeout(timer);
  }, [qrCopied]);

  const allProjects = projects.status === "ready" ? projects.value : [];
  // The two pages split the catalog on the persisted phase, the only
  // lifecycle axis known for every record, drafts included. Each page keeps
  // the list's own order — nothing is re-sorted, and the same record can
  // only ever appear on one page because `phase` is a single persisted
  // value.
  const installedProjects = allProjects.filter(
    (project) => project.phase === "installed",
  );
  const draftProjects = allProjects.filter(
    (project) => project.phase === "draft",
  );
  const pageProjects = page === "live" ? installedProjects : draftProjects;
  // The one operation this page has to narrate in its own voice: a lifecycle
  // op on a record it does not render as a card. That is the normal shape of
  // a fresh draft's Build and start — the build promotes the record, its card
  // moves to the Live pockets page, and the chained start/open still has to be
  // said somewhere — and the `some` guard is what keeps this line from ever
  // duplicating the card's own label on the page that does render it.
  const panelLifecycle =
    lifecycle !== null && !pageProjects.some((p) => p.id === lifecycle.id)
      ? lifecycle
      : null;
  // The same guard for the operation's failure, limited to the chained open
  // (the only one that may say the Pocket is already installed): a card that
  // is on this page reports its own error.
  const panelLifecycleError =
    lifecycleError !== null &&
    lifecycleError.afterBuild === true &&
    !pageProjects.some((p) => p.id === lifecycleError.id)
      ? lifecycleError
      : null;
  // Eager honest capture: a running-and-ready installed Pocket whose thumb
  // read came back empty has a rendered UI behind its mapped port right
  // now — the same fact a Start or Visit capture rests on — so ask native
  // for exactly that one capture (the command re-checks the fact before
  // building a webview). One attempt per project per mount: a refusal —
  // busy claim, non-macOS build, stopped since the read — leaves the quiet
  // no-figure card and the next real capture moment, never a retry loop.
  // Drafts are never asked: their UI exists only inside a live preview
  // session, which the native refresh hook captures on its own.
  const thumbAttemptsRef = useRef(new Set<string>());
  useEffect(() => {
    if (page !== "live") return;
    for (const project of installedProjects) {
      // `null` means the read answered "no thumb"; `undefined` means no
      // read has landed yet, and the card simply waits.
      if (thumbs[project.id] !== null) continue;
      if (thumbAttemptsRef.current.has(project.id)) continue;
      const entry = statuses[project.id];
      if (
        entry?.status !== "ready" ||
        entry.value.state !== "running" ||
        !entry.value.ready
      ) {
        continue;
      }
      thumbAttemptsRef.current.add(project.id);
      void invoke<ThumbInfo | null>("pocket_thumb_capture", {
        projectId: project.id,
      })
        .then((value) => {
          if (isThumbInfo(value)) {
            setThumbs((prev) => ({ ...prev, [project.id]: value }));
          }
        })
        .catch(() => {
          // A failed capture is the same quiet outcome as no capture ever
          // happening: the card stays plain and nothing is re-asked.
        });
    }
  }, [page, installedProjects, statuses, thumbs]);
  // Subtitle honesty (v2 §8): counts come only from what this component
  // actually knows — the loaded list plus trustworthy `project_status`
  // reads. The live page counts the installed records and their running
  // part (shown only once at least one read landed); the drafts page
  // counts drafts and nothing else, because drafts are never read and
  // never counted as anything. "pockets.local" elsewhere in the shell is a
  // static label, nothing measured, so no host claim rides this line.
  let trustworthyReads = 0;
  let runningCount = 0;
  for (const project of allProjects) {
    const entry = statuses[project.id];
    if (entry?.status === "ready") {
      trustworthyReads += 1;
      if (entry.value.state === "running" && entry.value.ready) {
        runningCount += 1;
      }
    }
  }
  const subtitle =
    projects.status !== "ready" || pageProjects.length === 0
      ? null
      : page === "live"
        ? `${pageProjects.length} pocket${
            pageProjects.length === 1 ? "" : "s"
          }${trustworthyReads > 0 ? `, ${runningCount} running` : ""}`
        : `${pageProjects.length} draft${pageProjects.length === 1 ? "" : "s"}`;
  // Lineage caption resolution: the persisted names of this catalog's
  // installed records, keyed by id. A draft may claim a parent only through
  // its recorded `editedFrom` (see the `Project` docs); an id missing here —
  // a deleted parent — or one naming a draft resolves to nothing, and the
  // card then claims nothing. Matching is by recorded id only, never by
  // name or "looks like a copy" (same honesty rule as projectLineage.ts).
  const installedNames = new Map(
    installedProjects.map((project) => [
      project.id,
      project.manifest.metadata.name,
    ]),
  );
  const editedFromName = (project: Project): string | undefined => {
    if (project.phase !== "draft") return undefined;
    const parentId = project.editedFrom;
    if (parentId === undefined || parentId === null) return undefined;
    return installedNames.get(parentId);
  };
  // Only built records have a container lifecycle; unbuilt snapshots must not
  // trigger status commands.
  const builtKey =
    projects.status === "ready"
      ? projects.value
          .filter((project) => isBuilt(project))
          .map((project) => project.id)
          .join(",")
      : "";

  // Fetch statuses when the runtime becomes ready and whenever the built
  // set changes; each card shows its own busy, error, or not-ready state.
  useEffect(() => {
    if (!runtimeReady || builtKey === "") return;
    void refreshStatuses(builtKey.split(","));
  }, [runtimeReady, builtKey, refreshStatuses]);

  const handleRefreshAll = useCallback(() => {
    void refresh();
    if (runtimeReady && builtKey !== "") {
      void refreshStatuses(builtKey.split(","));
    }
  }, [refresh, refreshStatuses, runtimeReady, builtKey]);

  const normalizedFilter = filterQuery.trim().toLowerCase();
  const visiblePageProjects =
    normalizedFilter === ""
      ? pageProjects
      : pageProjects.filter((project) => {
          const name = project.manifest.metadata.name.toLowerCase();
          const desc = project.manifest.metadata.description.toLowerCase();
          return (
            name.includes(normalizedFilter) || desc.includes(normalizedFilter)
          );
        });

  // The one card renderer, used on both catalog pages and in the workspace
  // portal. In the workspace it focuses on Installed app lifecycle, history,
  // technical details, and delete without duplicating the project title.
  const card = (project: Project, inWorkspace = false) => (
    <ProjectCard
      project={project}
      inWorkspace={inWorkspace}
      thumb={inWorkspace ? null : (thumbs[project.id] ?? null)}
      onEnlargeThumb={
        thumbs[project.id] === null
          ? undefined
          : () => {
              enlargeReturnFocusRef.current =
                document.activeElement instanceof HTMLElement
                  ? document.activeElement
                  : null;
              setEnlarged({
                projectId: project.id,
                name: project.manifest.metadata.name,
                dataUrl: thumbs[project.id]!.dataUrl,
                capturedAt: thumbs[project.id]!.capturedAt,
              });
            }
      }
      editedFromName={editedFromName(project)}
      build={activeBuild && activeBuild.id === project.id ? activeBuild : null}
      status={statuses[project.id]}
      usage={usageByProject?.[project.id] ?? null}
      operation={
        lifecycle && lifecycle.id === project.id ? lifecycle.operation : null
      }
      startingOnly={
        lifecycle !== null &&
        lifecycle.id === project.id &&
        lifecycle.operation === "starting" &&
        !lifecycle.openAfterStart
      }
      operationError={
        lifecycleError && lifecycleError.id === project.id
          ? lifecycleError
          : null
      }
      busy={busy}
      runtimeReady={runtimeReady}
      onBuild={handleBuild}
      onOpen={handleOpen}
      onStart={handleStart}
      onStop={handleStop}
      onAskDelete={handleAskDelete}
      onExportData={handleExportData}
      onAskRestoreData={handleAskRestoreData}
      onAskResetData={handleAskResetData}
      exportingData={exportingDataId === project.id}
      dataFeedback={
        dataFeedback && dataFeedback.id === project.id ? dataFeedback : null
      }
      tailscaleBusy={tailscaleBusyId === project.id}
      tailscaleError={
        tailscaleError && tailscaleError.id === project.id
          ? tailscaleError.message
          : null
      }
      tailscaleReport={tailscaleReport}
      onCheckTailscale={() => void handleCheckTailscale()}
      onToggleTailscale={(target, enabled) =>
        void handleToggleTailscale(target, enabled)
      }
      onShowTailscaleQr={handleShowTailscaleQr}
      onSelectProject={inWorkspace ? undefined : onSelectProject}
      // A successful source restore lands new working digests: refetch the
      // catalog and statuses in place (no remount, so in-flight builds and
      // dialogs survive) and the card's Update affordance reappears from the
      // fresh data.
      onRestored={handleRefreshAll}
    />
  );

  return (
    <section
      className="card creation-panel"
      aria-labelledby="creation-heading"
      aria-busy={busy}
    >
      {workspace &&
        createPortal(
          card(workspace.project, true),
          workspace.host,
          workspace.project.id,
        )}
      <div className="creation-header">
        <div className="creation-heading-block">
          {/* v3 §5: the page name leads the shell's title row (App renders
              it), so this is the page's only visible text now — the h1
              stays, hidden, because the delete dialog and the grid are
              labelled by it, and hidden-with-clip keeps it in the
              accessibility tree (a `hidden` attribute would remove the
              label). */}
          <h2 id="creation-heading" className="creation-title visually-hidden">
            {page === "live" ? "Live pockets" : "Drafts"}
          </h2>
          {subtitle !== null && <p className="creation-subtitle">{subtitle}</p>}
        </div>
        {(pageProjects.length > 0 || filterQuery !== "") && (
          <input
            type="search"
            className="creation-search"
            aria-label={
              page === "live" ? "Filter live pockets" : "Filter drafts"
            }
            placeholder={
              page === "live" ? "Filter live pockets…" : "Filter drafts…"
            }
            value={filterQuery}
            onChange={(event) => setFilterQuery(event.currentTarget.value)}
          />
        )}
        {/* One action row beside the counts line: the quiet page actions,
            then the primary New pocket last, all on the same line. */}
        <div className="creation-header-actions">
          <button type="button" onClick={handleRefreshAll} disabled={busy}>
            {listing ? "Loading…" : "Refresh"}
          </button>
          <button type="button" onClick={handleImport} disabled={busy}>
            {importing ? "Importing…" : "Import pocket"}
          </button>
          <button type="button" onClick={handleLinkProject} disabled={busy}>
            {linking ? "Linking…" : "Link project"}
          </button>
          {onNewPocket !== undefined && (
            <button
              type="button"
              className="creation-primary button-primary"
              onClick={onNewPocket}
            >
              <IconPlus />
              New pocket
            </button>
          )}
        </div>
      </div>

      {/* Page-level build note stated once for the drafts page instead of
          repeated on every unbuilt card. */}
      {page === "drafts" && pageProjects.some((p) => !isBuilt(p)) && (
        <p className="hint creation-page-note">
          Build and start packages the draft with Podman — it may download
          dependencies — and then starts it, in one step. Your original project
          files are never changed.
        </p>
      )}

      <div className="creation-status" aria-live="polite">
        {projects.status === "loading" && (
          <p className="muted">Loading your created Pockets…</p>
        )}
        {projects.status === "error" && (
          <p className="error">
            Could not load your created Pockets. {projects.message}
          </p>
        )}
        {importing && (
          <p className="hint strong">
            Choose a folder in the native picker — Pockets is importing the
            snapshot…
          </p>
        )}
        {linking && (
          <p className="hint strong">
            Choose your development project folder in the native picker…
          </p>
        )}
        {importError && (
          <p className="error">The source operation failed. {importError}</p>
        )}
        {/* A Build and start outlives its own card here: the build promotes the
            record, so the card is on the Live pockets page while this page is
            still starting and opening it. Said once, at panel level, and only
            while no card on this page carries it (see `panelLifecycle`). */}
        {panelLifecycle !== null && (
          <p className="hint strong">
            {namedLifecycle(panelLifecycle.operation, panelLifecycle.name)}
            {panelLifecycle.operation === "starting" && STARTING_NOTE}
          </p>
        )}
        {panelLifecycleError !== null && (
          <>
            <p className="error">
              {panelLifecycleError.name} is built and installed, but it did not
              open. {panelLifecycleError.message}
            </p>
            <p className="hint">
              Nothing needs rebuilding — open it from the Live pockets page,
              where its card re-reads the status.
            </p>
          </>
        )}
      </div>

      {/* The page's one honest empty state. The live page is empty exactly
          when the catalog holds no installed record, and building is the
          real action that creates one; the drafts page keeps the handoff's
          fixed copy, with the dashed new-pocket cell as its way out too. */}
      {projects.status === "ready" &&
        pageProjects.length === 0 &&
        (page === "live" ? (
          <p className="muted creation-empty">
            No live Pockets yet. Build a Pocket to see it here.
          </p>
        ) : (
          <p className="muted creation-empty">
            No drafts yet. Create a new Pocket, link a project, or import one.
          </p>
        ))}

      {projects.status === "ready" &&
        pageProjects.length > 0 &&
        visiblePageProjects.length === 0 && (
          <div className="creation-filter-empty">
            <p className="muted">No Pockets match “{filterQuery.trim()}”.</p>
            <button type="button" onClick={() => setFilterQuery("")}>
              Clear filter
            </button>
          </div>
        )}

      {(visiblePageProjects.length > 0 ||
        (page === "drafts" &&
          normalizedFilter === "" &&
          onNewPocket !== undefined)) && (
        <ul className="creation-grid" aria-labelledby="creation-heading">
          {visiblePageProjects.map((project) => (
            <li key={project.id}>{card(project)}</li>
          ))}
          {page === "drafts" &&
            normalizedFilter === "" &&
            onNewPocket !== undefined && (
              <li>
                <button
                  type="button"
                  className="creation-new"
                  onClick={onNewPocket}
                >
                  <span className="creation-new-plus" aria-hidden="true">
                    <IconPlus />
                  </span>
                  <span className="creation-new-title">New pocket</span>
                  <span className="creation-new-description">
                    Start with a local source folder and a runnable starter.
                  </span>
                </button>
              </li>
            )}
        </ul>
      )}
      {deleteTarget &&
        createPortal(
          <div
            className="creation-modal-backdrop"
            onClick={(event) => {
              if (event.target === event.currentTarget) handleCloseDelete();
            }}
          >
            <div
              className="creation-modal"
              role="dialog"
              aria-modal="true"
              aria-labelledby="creation-delete-title"
              onKeyDown={trapModalFocus}
            >
              <h3 id="creation-delete-title" className="creation-modal-title">
                Delete “{deleteTarget.manifest.metadata.name}”
                {deleteTarget.phase === "draft" ? " draft" : ""}?
              </h3>
              <p className="creation-modal-warning">
                {deleteIsUnbuiltDraft
                  ? "This permanently removes the draft from Pockets: its record and its source snapshot. Source folders imported from elsewhere on your computer are never touched."
                  : "This permanently removes the Pocket from Pockets: its record, build snapshot, container, and data volume — including any data the Pocket saved while it ran. Source folders imported from elsewhere on your computer are never touched."}
              </p>
              {/* The typed-name gate is asked only of installed records; a
                  draft's confirmation is the button below. Focus lands on Cancel
                  there — never on the destructive action, so Enter in a dialog
                  with nothing to type cannot delete anything. */}
              {deleteNeedsName && (
                <>
                  <label
                    className="creation-modal-label"
                    htmlFor="creation-delete-confirm"
                  >
                    Type the Pocket’s name to confirm:{" "}
                    <code>{deleteTarget.manifest.metadata.name}</code>
                  </label>
                  <input
                    id="creation-delete-confirm"
                    type="text"
                    autoComplete="off"
                    value={deleteConfirm}
                    onChange={(event) => setDeleteConfirm(event.target.value)}
                    disabled={deleting}
                    autoFocus
                  />
                </>
              )}
              {deleteError && <p className="error">{deleteError}</p>}
              <div className="creation-modal-actions">
                <button
                  type="button"
                  onClick={handleCloseDelete}
                  disabled={deleting}
                  autoFocus={!deleteNeedsName}
                >
                  Cancel
                </button>
                <button
                  type="button"
                  className="creation-delete"
                  disabled={
                    deleting ||
                    (deleteNeedsName &&
                      deleteConfirm.trim() !==
                        deleteTarget.manifest.metadata.name)
                  }
                  onClick={() => void handleConfirmDelete()}
                >
                  {deleting ? "Deleting…" : "Delete Pocket"}
                </button>
              </div>
            </div>
          </div>,
          document.body,
        )}
      {resetTarget &&
        createPortal(
          <div
            className="creation-modal-backdrop"
            onClick={(event) => {
              if (event.target === event.currentTarget) handleCloseResetData();
            }}
          >
            <div
              className="creation-modal"
              role="dialog"
              aria-modal="true"
              aria-labelledby="creation-reset-title"
              onKeyDown={trapModalFocus}
            >
              <h3 id="creation-reset-title" className="creation-modal-title">
                Reset data for “{resetTarget.manifest.metadata.name}”?
              </h3>
              <p className="creation-modal-warning">
                This stops the Pocket if it is running and permanently deletes
                all data stored in its Podman volume. The Pocket app and your
                source folder are not touched.
              </p>
              {resetError && <p className="error">{resetError}</p>}
              <div className="creation-modal-actions">
                <button
                  type="button"
                  onClick={handleCloseResetData}
                  disabled={resetting}
                  autoFocus
                >
                  Cancel
                </button>
                <button
                  type="button"
                  className="creation-delete"
                  disabled={resetting}
                  onClick={() => void handleConfirmResetData()}
                >
                  {resetting ? "Resetting…" : "Reset data"}
                </button>
              </div>
            </div>
          </div>,
          document.body,
        )}
      {restoreTarget &&
        createPortal(
          <div
            className="creation-modal-backdrop"
            onClick={(event) => {
              if (event.target === event.currentTarget)
                handleCloseRestoreData();
            }}
          >
            <div
              className="creation-modal"
              role="dialog"
              aria-modal="true"
              aria-labelledby="creation-restore-title"
              onKeyDown={trapModalFocus}
            >
              <h3 id="creation-restore-title" className="creation-modal-title">
                Restore backup for “{restoreTarget.manifest.metadata.name}”?
              </h3>
              <p className="creation-modal-warning">
                Choosing a backup archive stops the Pocket if it is running and
                replaces all data currently in its Podman volume. The Pocket app
                and your source folder are not touched.
              </p>
              {restoreDataError && <p className="error">{restoreDataError}</p>}
              <div className="creation-modal-actions">
                <button
                  type="button"
                  onClick={handleCloseRestoreData}
                  disabled={restoringData}
                  autoFocus
                >
                  Cancel
                </button>
                <button
                  type="button"
                  className="button-primary"
                  disabled={restoringData}
                  onClick={() => void handleConfirmRestoreData()}
                >
                  {restoringData ? "Restoring…" : "Choose backup…"}
                </button>
              </div>
            </div>
          </div>,
          document.body,
        )}
      {enlarged !== null && (
        <div
          className="creation-modal-backdrop"
          onClick={(event) => {
            if (event.target === event.currentTarget) handleCloseEnlarge();
          }}
        >
          <div
            className="creation-lightbox"
            role="dialog"
            aria-modal="true"
            aria-label={`Enlarged capture of ${enlarged.name}`}
            onKeyDown={trapModalFocus}
          >
            <button
              type="button"
              className="creation-lightbox-close"
              aria-label="Close enlarged capture"
              autoFocus
              onClick={handleCloseEnlarge}
            >
              Close
            </button>
            <img
              className="creation-lightbox-image"
              src={enlarged.dataUrl}
              alt={`Captured view of ${enlarged.name}`}
            />
            <p className="creation-lightbox-caption">
              {enlarged.name} — captured{" "}
              {formatCapturedFull(enlarged.capturedAt)}
            </p>
          </div>
        </div>
      )}
      {qrTarget !== null &&
        createPortal(
          <div
            className="creation-modal-backdrop"
            onClick={(event) => {
              if (event.target === event.currentTarget) handleCloseQr();
            }}
          >
            <div
              className="creation-qr-modal"
              role="dialog"
              aria-modal="true"
              aria-labelledby="creation-qr-title"
              onKeyDown={trapModalFocus}
            >
              <h3 id="creation-qr-title" className="creation-modal-title">
                {qrTarget.name} on Tailscale
              </h3>
              <div className="creation-qr-modal-frame">
                <QrCodeSvg
                  value={qrTarget.url}
                  label={`QR code for ${qrTarget.name} Tailscale URL`}
                  size={220}
                />
              </div>
              <code className="creation-qr-modal-url">{qrTarget.url}</code>
              <p className="hint">
                Scan from another device connected to your Tailscale network.
              </p>
              <div className="creation-qr-modal-actions">
                <button
                  type="button"
                  onClick={() => {
                    void navigator.clipboard
                      ?.writeText(qrTarget.url)
                      .then(() => setQrCopied(true))
                      .catch(() => {});
                  }}
                >
                  {qrCopied ? "Copied" : "Copy URL"}
                </button>
                <button
                  type="button"
                  className="button-primary"
                  autoFocus
                  onClick={handleCloseQr}
                >
                  Close
                </button>
              </div>
            </div>
          </div>,
          document.body,
        )}
    </section>
  );
}

function ProjectCard({
  project,
  inWorkspace,
  thumb,
  onEnlargeThumb,
  editedFromName,
  build,
  status,
  usage,
  operation,
  startingOnly,
  operationError,
  busy,
  runtimeReady,
  onBuild,
  onOpen,
  onStart,
  onStop,
  onAskDelete,
  onExportData,
  onAskRestoreData,
  onAskResetData,
  exportingData,
  dataFeedback,
  tailscaleBusy,
  tailscaleError,
  tailscaleReport,
  onCheckTailscale,
  onToggleTailscale,
  onShowTailscaleQr,
  onSelectProject,
  onRestored,
}: {
  project: Project;
  inWorkspace: boolean;
  /**
   * The project's stored card thumbnail, or null when there is nothing
   * honest to show. `capturedAt` is the stored file's mtime and is only
   * ever rendered as the caption's "as of" moment — the card never
   * converts it into a present-tense claim.
   */
  thumb: ThumbInfo | null;
  /**
   * Opens the enlarge overlay for this card's capture. Passed only when a
   * thumb exists — the card already renders no figure otherwise — so the
   * card never has to wonder what clicking would do.
   */
  onEnlargeThumb?: () => void;
  /**
   * Resolved parent name for a draft whose recorded `editedFrom` names an
   * installed record in the current catalog (see the panel's
   * `editedFromName`). The panel passes it only when the persisted lineage
   * resolves here; the card then states it verbatim. Absent means no claim:
   * no caption is rendered, never a raw id and never a guess.
   */
  editedFromName?: string;
  build: ActiveBuild | null;
  status: LoadState<PocketStatus> | undefined;
  usage: PocketUsage | null;
  operation: LifecycleOperation | null;
  startingOnly: boolean;
  operationError: {
    id: string;
    operation: LifecycleOperation;
    message: string;
  } | null;
  busy: boolean;
  runtimeReady: boolean;
  /**
   * Runs the build for this card. `openAfter` is what the card decides, not
   * the panel: true only for a fresh draft (no installed record to disturb),
   * where one click means build *and* start; false for an Update, which applies
   * a new version to a Pocket the user may be using and must not hijack their
   * browser.
   */
  onBuild: (project: Project, openAfter: boolean) => void;
  onOpen: (project: Project) => void;
  onStart: (project: Project) => void;
  onStop: (project: Project) => void;
  onAskDelete?: (project: Project) => void;
  onExportData: (project: Project) => void;
  onAskRestoreData: (project: Project) => void;
  onAskResetData: (project: Project) => void;
  exportingData: boolean;
  dataFeedback: { tone: "ok" | "error"; message: string } | null;
  tailscaleBusy: boolean;
  tailscaleError: string | null;
  tailscaleReport: TailscaleReportState;
  onCheckTailscale: () => void;
  onToggleTailscale: (project: Project, enabled: boolean) => void;
  onShowTailscaleQr: (project: Project, url: string) => void;
  onSelectProject?: (id: string) => void;
  onRestored: () => void;
}) {
  const [expanded, setExpanded] = useState(false);
  // The ⋯ menu (rarely needed and destructive actions live there, so the
  // main action row never wraps) and the address copy confirmation.
  const [menuOpen, setMenuOpen] = useState(false);
  const [copied, setCopied] = useState(false);
  const [tailscaleCopied, setTailscaleCopied] = useState(false);
  const menuRef = useRef<HTMLDivElement | null>(null);
  useEffect(() => {
    if (!menuOpen) return;
    const onPointer = (event: MouseEvent) => {
      if (!menuRef.current?.contains(event.target as Node)) setMenuOpen(false);
    };
    const onKey = (event: KeyboardEvent) => {
      if (event.key === "Escape") setMenuOpen(false);
    };
    document.addEventListener("mousedown", onPointer);
    document.addEventListener("keydown", onKey);
    return () => {
      document.removeEventListener("mousedown", onPointer);
      document.removeEventListener("keydown", onKey);
    };
  }, [menuOpen]);
  useEffect(() => {
    if (!copied) return;
    const timer = window.setTimeout(() => setCopied(false), 1500);
    return () => window.clearTimeout(timer);
  }, [copied]);
  useEffect(() => {
    if (!tailscaleCopied) return;
    const timer = window.setTimeout(() => setTailscaleCopied(false), 1500);
    return () => window.clearTimeout(timer);
  }, [tailscaleCopied]);
  const copyAddress = (url: string) => {
    void navigator.clipboard
      ?.writeText(url)
      .then(() => setCopied(true))
      .catch(() => {
        /* nothing copied, nothing claimed */
      });
  };
  const copyTailscaleUrl = (url: string) => {
    void navigator.clipboard
      ?.writeText(url)
      .then(() => setTailscaleCopied(true))
      .catch(() => {
        /* nothing copied, nothing claimed */
      });
  };
  const id = inWorkspace ? `workspace-${project.id}` : project.id;
  const nameId = `creation-name-${id}`;
  const detailsId = `creation-details-${id}`;
  // One Pocket, two digests: the working source (editable, chat revises it)
  // and the installed pin (what actually runs). The primary action is Build
  // and start for a first install — one click that also runs the new Pocket —
  // and Update when the installed pin trails the working source; an up-to-date
  // build shows neither — the code is already applied.
  const built = isBuilt(project);
  const pending = hasPendingChanges(project);
  const unsynced = project.unsyncedChanges === true;
  const hasChanges = pending || unsynced;
  // In the development workspace, a linked installed Pocket can be updated
  // directly from its live folder (build_project syncs it automatically).
  const canSyncUpdate =
    inWorkspace && built && project.developmentLinked === true;
  const canBuild = !built || hasChanges || canSyncUpdate;
  const buildPrimary = !built || hasChanges;
  const builtImage = installedImageId(project);
  // Legacy built records have no pin yet: assume the installed snapshot is
  // the current source until native stamps `installed`.
  const installedDigest =
    project.installed?.sourceDigest ?? (built ? project.sourceDigest : null);
  const failed =
    build !== null &&
    !build.running &&
    (build.error !== null || build.status?.state === "failed");
  // The build promise only resolves with the persisted installed record, so
  // a settled, non-failed run is a success.
  const succeeded = build !== null && !build.running && !failed;
  const statusValue = status?.status === "ready" ? status.value : null;
  const running = statusValue?.state === "running";
  const stopped = statusValue?.state === "stopped";
  // Open needs a trustworthy status like the Hello Pocket controls; native
  // Start/Open validate readiness themselves.
  const canOpen = !busy && runtimeReady && statusValue !== null;
  const canStart = !busy && runtimeReady && stopped;
  const showStart = stopped || startingOnly;
  // Native Stop validates container ownership itself, so it stays available
  // even when status inspection failed (e.g. an unsafe or missing port
  // mapping) and the container is known to be Pockets-owned.
  const canStop =
    !busy && runtimeReady && (running === true || status?.status === "error");
  // The card's one status word (v2 §8), aligned with PocketSidebar's honest
  // status mapping: Building/Checking/Starting/Unavailable use `warn`,
  // running-and-ready uses `ok`, and Draft/Stopped use `stopped`.
  const header: { word: string; tone: PipTone } = build?.running
    ? { word: "Building", tone: "warn" }
    : !built
      ? { word: "Draft", tone: "stopped" }
      : !runtimeReady
        ? { word: "Runtime not ready", tone: "warn" }
        : statusValue === null
          ? status?.status === "error"
            ? { word: "Status unavailable", tone: "warn" }
            : { word: "Checking…", tone: "warn" }
          : running
            ? statusValue.ready
              ? { word: "Running", tone: "ok" }
              : { word: "Starting", tone: "warn" }
            : { word: "Stopped", tone: "stopped" };
  // Meta row: only fields this record really carries. No port, no usage
  // counts, no storage figure — the catalog reports none of those.
  const meta: string[] = [];
  const builtAt = project.installed?.builtAtSeconds;
  if (built && typeof builtAt === "number") {
    meta.push(`Built ${formatBuiltDate(builtAt)}`);
  }
  if (unsynced) {
    meta.push("Unsynced changes");
  } else if (built && pending) {
    meta.push("Waiting on update");
  }
  // Both names drive the same native flow (`start_project` then
  // `open_project`), which is idempotent: it starts a stopped Pocket and
  // opens the browser for one that is already ready. So the word describes
  // what the click does from here.
  const visitBusy =
    (operation === "starting" && !startingOnly) || operation === "opening";
  const visitWord = running && statusValue?.ready === true ? "Visit" : "Open";
  const usageRunning = running && usage?.state === "running";
  const usageCpu =
    usageRunning &&
    typeof usage.cpuPercent === "number" &&
    Number.isFinite(usage.cpuPercent) &&
    usage.cpuPercent >= 0
      ? usage.cpuPercent
      : null;
  const usageMemory =
    usageRunning &&
    typeof usage.memoryBytes === "number" &&
    Number.isFinite(usage.memoryBytes) &&
    usage.memoryBytes >= 0
      ? usage.memoryBytes
      : null;

  const hasUsage = usageCpu !== null || usageMemory !== null;
  const usageId = `creation-usage-${id}`;
  const tailscale = statusValue?.tailscale ?? null;
  const tailscaleEnabled = tailscale?.enabled === true;
  const tailscaleActive =
    tailscale?.active === true && typeof tailscale.url === "string";
  const tailscaleUrl = tailscaleActive ? tailscale!.url! : null;
  const tailscaleDirectUrl =
    tailscaleActive &&
    typeof tailscale?.directUrl === "string" &&
    tailscale.directUrl !== tailscaleUrl
      ? tailscale.directUrl
      : null;
  const tailscaleNeedsReconnect =
    tailscaleEnabled &&
    !tailscaleActive &&
    running === true &&
    statusValue?.ready === true;
  const tailscaleMeta = tailscaleActive
    ? "Shared"
    : tailscaleNeedsReconnect
      ? "Needs reconnect"
      : tailscaleEnabled
        ? "Paused"
        : "Off";
  const tailscaleReady =
    tailscaleReport.status !== "ready" ||
    tailscaleReport.value.state === "ready";

  return (
    <article className="creation-card" aria-labelledby={nameId}>
      {/* The last real capture of this Pocket's UI, taken natively at an
          honest moment: a preview refresh, a Start that proved the Pocket
          running, a Visit, or the first look at a Pocket already running
          (which the panel asks native to capture, re-checked natively
          first). The caption is the load-bearing part: it names when the
          picture is *from*, so an image that predates the newest change
          says so instead of pretending to be now. No capture yet means no
          figure at all — never a placeholder, never invented art, and the
          image is decoration under the caption that states its age. */}
      {thumb !== null && (
        <figure className="creation-thumb">
          {/* The capture is the card's densest evidence, and at card width
              small UI text is unreadable. The whole image is a real button
              that hands the *same stored bytes* to the enlarge overlay —
              the overlay only displays bigger, it claims nothing new. */}
          <button
            type="button"
            className="creation-thumb-button"
            aria-label={`Enlarge the captured view of ${project.manifest.metadata.name}`}
            onClick={onEnlargeThumb}
          >
            <img className="creation-thumb-image" src={thumb.dataUrl} alt="" />
          </button>
          <figcaption
            className="creation-thumb-caption"
            title={formatCapturedFull(thumb.capturedAt)}
          >
            <time dateTime={new Date(thumb.capturedAt).toISOString()}>
              {formatCapturedAt(thumb.capturedAt)}
            </time>
          </figcaption>
        </figure>
      )}
      <div className="creation-card-top">
        {inWorkspace ? (
          <h3 id={nameId} className="creation-name">
            Installed app
          </h3>
        ) : (
          <div className="creation-card-title-group">
            <PocketCardLogo
              projectId={project.id}
              name={project.manifest.metadata.name}
              cacheKey={`${project.sourceDigest.slice(0, 12)}-${running ? "1" : "0"}`}
            />
            <h3 id={nameId} className="creation-name">
              {project.manifest.metadata.name}
            </h3>
          </div>
        )}
        {/* Usage rides on the status instead of its own line: memory in
            grey while things are calm, CPU in amber once it passes
            HIGH_CPU_PERCENT, and both exact figures in a panel on hover or
            keyboard focus. A stopped Pocket shows no figures at all. */}
        <span
          className="creation-card-status"
          tabIndex={hasUsage ? 0 : undefined}
          aria-describedby={hasUsage ? usageId : undefined}
        >
          {/* Pip, word and figure are direct siblings on one line box, so
              the figure sits on the same baseline as the word. */}
          <span
            className="status-pip"
            data-tone={header.tone}
            aria-hidden="true"
          />
          <span className="creation-card-status-word">{header.word}</span>
          {usageCpu !== null && usageCpu >= HIGH_CPU_PERCENT ? (
            <span className="creation-card-status-figure" data-tone="warn">
              {`· CPU ${formatPercent(usageCpu)}`}
            </span>
          ) : usageMemory !== null ? (
            <span className="creation-card-status-figure">
              {`· ${formatBytes(usageMemory)}`}
            </span>
          ) : null}
          {hasUsage && (
            <dl
              id={usageId}
              role="tooltip"
              className="creation-usage"
              aria-label="Resource usage"
            >
              {usageCpu !== null && (
                <div
                  className="creation-usage-item"
                  title="Container CPU (100% = one core)"
                >
                  <dt>CPU</dt>
                  <dd>{formatPercent(usageCpu)}</dd>
                </div>
              )}
              {usageMemory !== null && (
                <div className="creation-usage-item">
                  <dt>Memory</dt>
                  <dd>{formatBytes(usageMemory)}</dd>
                </div>
              )}
            </dl>
          )}
        </span>
      </div>
      {/* The one visible trace of lineage on this card: the panel resolved
          this draft's persisted `editedFrom` to an installed record in the
          current catalog, so the card states the parent's real manifest
          name. Nothing else about the card changes. */}
      {!inWorkspace && editedFromName !== undefined && (
        <p className="creation-edited-from">Edited from {editedFromName}</p>
      )}
      {!inWorkspace && (
        <p className="creation-description">
          {project.manifest.metadata.description}
        </p>
      )}
      {meta.length > 0 && (
        <p className="creation-card-meta">
          {meta.map((item) => (
            <span key={item} className="creation-card-meta-item">
              {item}
            </span>
          ))}
        </p>
      )}

      {((hasChanges && built) ||
        build !== null ||
        (canBuild && !runtimeReady)) && (
        <section
          className="creation-build-section"
          aria-live="polite"
          aria-busy={build?.running === true}
        >
          {built && unsynced ? (
            <p className="hint creation-update-notice">
              Your linked folder has changes not yet in the installed app.
              Update syncs and installs them while keeping saved data.
            </p>
          ) : built && pending ? (
            <p className="hint creation-update-notice">
              The saved source has changes ready to install. Update replaces the
              installed version and keeps its saved data.
            </p>
          ) : null}

          {build?.running && (
            <p className="hint strong" role="status">
              {build.status?.state === "running"
                ? inWorkspace
                  ? "Building your Pocket — the live output is under Technical details."
                  : "Building your Pocket — the live output is under Manage."
                : "Packaging your Pocket…"}
            </p>
          )}

          {failed && (
            <p className="error">
              The build failed.{" "}
              {/* The current invocation's rejection first: a rejected
                  request (busy guard, runtime unavailable, pre-build
                  failure) leaves a previous run's `failed` status
                  remembered, which would mask the real reason. */}
              {build?.error ??
                build?.status?.error ??
                "No reason was reported."}{" "}
              {inWorkspace
                ? "The full output is under Technical details."
                : "The full output is under Manage."}
            </p>
          )}

          {succeeded && !running && (
            <p className="hint strong">
              Saved in your Pockets. Open it when ready.
            </p>
          )}

          {canBuild && !runtimeReady && (
            <p className="hint">
              Building is available once the container runtime is ready.
            </p>
          )}
        </section>
      )}

      {/* Anchor URL, resource usage, action buttons, and disclosures together
          at the bottom of the card so they stay tightly paired and aligned
          across cards regardless of description length. */}
      <div className="creation-card-footer">
        {built && (
          <section
            className="creation-lifecycle"
            aria-live="polite"
            aria-busy={operation !== null}
          >
            {/* The pip and word in the card header already carry the state, so
                this lane says what to do about it instead of repeating it. */}
            {!runtimeReady ? (
              <p>
                Status and Pocket controls return once the container runtime is
                ready.
              </p>
            ) : statusValue ? (
              <p>
                {/* Friendly copy only: the raw native message can carry the
                    container's internal id and stays under Technical details. */}
                {running ? (
                  statusValue.ready ? (
                    statusValue.url ? (
                      <span className="creation-address">
                        <span
                          className="creation-address-text"
                          title={statusValue.url}
                        >
                          {displayAddress(statusValue.url)}
                        </span>
                        <button
                          type="button"
                          className="creation-address-copy"
                          aria-label="Copy address"
                          title={copied ? "Copied" : "Copy address"}
                          onClick={() => copyAddress(statusValue.url!)}
                        >
                          {copied ? <IconCheckSmall /> : <IconCopy />}
                        </button>
                        {tailscaleUrl && (
                          <button
                            type="button"
                            className="creation-address-tailscale"
                            aria-label={`Show Tailscale QR code for ${project.manifest.metadata.name}`}
                            title="Shared on Tailscale — show QR code"
                            onClick={() =>
                              onShowTailscaleQr(project, tailscaleUrl)
                            }
                          >
                            <span
                              className="status-pip"
                              data-tone="ok"
                              aria-hidden="true"
                            />
                            Tailscale
                          </button>
                        )}
                      </span>
                    ) : (
                      "Your Pocket is ready."
                    )
                  ) : (
                    "Still starting — try Open again shortly."
                  )
                ) : (
                  "Ready to open."
                )}
              </p>
            ) : status?.status === "error" ? (
              <p className="error">
                Could not load the Pocket&apos;s status. {status.message}
              </p>
            ) : (
              <p className="muted">Checking the Pocket status…</p>
            )}

            {operation && (
              <p className="hint strong">
                {LIFECYCLE_LABELS[operation]}
                {operation === "starting" && STARTING_NOTE}
              </p>
            )}

            {operationError && (
              <>
                <p className="error">
                  The operation failed. {operationError.message}
                </p>
                <p className="hint">
                  Status re-checked: Stop is still available if the Pocket is
                  running, and unrelated containers are never changed.
                </p>
              </>
            )}
          </section>
        )}

        <div className="creation-card-actions">
          <div className="creation-card-buttons">
            {canBuild && buildPrimary && (
              <button
                type="button"
                className="creation-build button-primary"
                disabled={busy || !runtimeReady}
                onClick={() => onBuild(project, !built)}
              >
                {build?.running
                  ? build.status?.state === "running"
                    ? "Building…"
                    : "Packaging…"
                  : failed
                    ? built
                      ? "Retry update"
                      : "Retry build"
                    : built
                      ? "Update"
                      : "Build and start"}
              </button>
            )}
            {built && (
              <button
                type="button"
                className={
                  buildPrimary
                    ? "creation-visit"
                    : "creation-visit button-primary"
                }
                onClick={() => onOpen(project)}
                disabled={!canOpen}
              >
                {visitBusy
                  ? operation === "opening"
                    ? LIFECYCLE_LABELS.opening
                    : LIFECYCLE_LABELS.starting
                  : visitWord}
                <IconExternalLink />
              </button>
            )}
            {canBuild && !buildPrimary && (
              <button
                type="button"
                className="creation-build"
                disabled={busy || !runtimeReady || !hasChanges}
                title={!hasChanges ? "No source changes to update" : undefined}
                onClick={() => onBuild(project, false)}
              >
                {build?.running
                  ? build.status?.state === "running"
                    ? "Building…"
                    : "Packaging…"
                  : failed
                    ? "Retry update"
                    : "Update"}
              </button>
            )}
            {onSelectProject && (
              <button
                type="button"
                className="creation-chat"
                onClick={() => onSelectProject(project.id)}
              >
                Manage
              </button>
            )}
            {built &&
              (showStart ? (
                <button
                  type="button"
                  className="creation-start"
                  aria-label={inWorkspace ? "Start installed app" : undefined}
                  onClick={() => onStart(project)}
                  disabled={!canStart}
                >
                  <span className="creation-start-icon" aria-hidden="true" />
                  {startingOnly ? LIFECYCLE_LABELS.starting : "Start"}
                </button>
              ) : (
                <button
                  type="button"
                  className="creation-stop"
                  aria-label={inWorkspace ? "Stop installed app" : undefined}
                  onClick={() => onStop(project)}
                  disabled={!canStop}
                >
                  <span className="creation-stop-icon" aria-hidden="true" />
                  {operation === "stopping"
                    ? LIFECYCLE_LABELS.stopping
                    : "Stop"}
                </button>
              ))}
            {(onAskDelete || (running && statusValue?.url)) && (
              <div className="creation-menu" ref={menuRef}>
                <button
                  type="button"
                  className="creation-menu-button"
                  aria-label="More actions"
                  title="More actions"
                  aria-haspopup="menu"
                  aria-expanded={menuOpen}
                  onClick={() => setMenuOpen((open) => !open)}
                >
                  <IconMore />
                </button>
                {menuOpen && (
                  <div className="creation-menu-list" role="menu">
                    {running && statusValue?.url && (
                      <button
                        type="button"
                        role="menuitem"
                        className="creation-menu-item"
                        onClick={() => {
                          setMenuOpen(false);
                          copyAddress(statusValue.url!);
                        }}
                      >
                        Copy address
                      </button>
                    )}
                    {tailscaleUrl && (
                      <>
                        <button
                          type="button"
                          role="menuitem"
                          className="creation-menu-item"
                          onClick={() => {
                            setMenuOpen(false);
                            copyTailscaleUrl(tailscaleUrl);
                          }}
                        >
                          Copy Tailscale URL
                        </button>
                        <button
                          type="button"
                          role="menuitem"
                          className="creation-menu-item"
                          onClick={() => {
                            setMenuOpen(false);
                            onShowTailscaleQr(project, tailscaleUrl);
                          }}
                        >
                          Show Tailscale QR code…
                        </button>
                      </>
                    )}
                    {onAskDelete && (
                      <>
                        {running && statusValue?.url && (
                          <div
                            className="creation-menu-divider"
                            role="separator"
                          />
                        )}
                        <button
                          type="button"
                          role="menuitem"
                          className="creation-menu-item creation-menu-danger"
                          disabled={busy}
                          onClick={() => {
                            setMenuOpen(false);
                            onAskDelete(project);
                          }}
                        >
                          Delete pocket…
                        </button>
                      </>
                    )}
                  </div>
                )}
              </div>
            )}
          </div>
        </div>

        {/* Secondary controls (Tailscale, Data, Source history, Technical
            details) live only on the Manage view's card. A grid card is a
            glance — status, address, Visit/Manage/Stop — and Manage is the
            one place to change settings or run backups and resets. */}
        {inWorkspace && (
          <div className="creation-card-disclosures">
            {built && (
              <details
                className="creation-tailscale"
                onToggle={(event) => {
                  if (
                    event.currentTarget.open &&
                    tailscaleReport.status === "idle"
                  ) {
                    onCheckTailscale();
                  }
                }}
              >
                <summary className="section-disclosure">
                  Tailscale
                  <span
                    className={
                      tailscaleActive
                        ? "creation-disclosure-meta creation-tailscale-meta-active"
                        : tailscaleNeedsReconnect
                          ? "creation-disclosure-meta creation-tailscale-meta-warn"
                          : "creation-disclosure-meta"
                    }
                  >
                    {tailscaleMeta}
                  </span>
                </summary>
                <div className="creation-tailscale-body">
                  <div className="creation-tailscale-row">
                    <div className="creation-tailscale-label">
                      <span>Share on your tailnet</span>
                      <p className="hint">
                        Private to your Tailscale devices. Pocket stays bound to
                        localhost.
                      </p>
                    </div>
                    <button
                      type="button"
                      role="switch"
                      className="tailscale-switch"
                      aria-checked={tailscaleEnabled}
                      aria-label={`Share ${project.manifest.metadata.name} on Tailscale`}
                      disabled={
                        busy ||
                        !runtimeReady ||
                        statusValue === null ||
                        (!tailscaleEnabled && !tailscaleReady)
                      }
                      onClick={() =>
                        onToggleTailscale(project, !tailscaleEnabled)
                      }
                    >
                      <span
                        className="tailscale-switch-pip"
                        aria-hidden="true"
                      />
                      {tailscaleBusy
                        ? tailscaleEnabled
                          ? "Stopping…"
                          : "Sharing…"
                        : tailscaleEnabled
                          ? "On"
                          : "Off"}
                    </button>
                  </div>

                  {!tailscaleEnabled &&
                    tailscaleReport.status === "ready" &&
                    tailscaleReport.value.state !== "ready" && (
                      <div className="creation-tailscale-qr-copy">
                        <p className="hint">{tailscaleReport.value.message}</p>
                        <div className="creation-tailscale-actions">
                          <button
                            type="button"
                            onClick={onCheckTailscale}
                            disabled={busy}
                          >
                            Re-check Tailscale
                          </button>
                        </div>
                      </div>
                    )}

                  {tailscaleUrl && (
                    <div className="creation-tailscale-active">
                      <div className="creation-tailscale-url-row">
                        <code
                          className="creation-tailscale-url"
                          title={tailscaleUrl}
                        >
                          {displayAddress(tailscaleUrl)}
                        </code>
                        <button
                          type="button"
                          className="creation-address-copy"
                          aria-label="Copy Tailscale URL"
                          title={
                            tailscaleCopied ? "Copied" : "Copy Tailscale URL"
                          }
                          onClick={() => copyTailscaleUrl(tailscaleUrl)}
                        >
                          {tailscaleCopied ? <IconCheckSmall /> : <IconCopy />}
                        </button>
                      </div>
                      <div className="creation-tailscale-qr-block">
                        <button
                          type="button"
                          className="creation-tailscale-qr-button"
                          aria-label={`Enlarge Tailscale QR code for ${project.manifest.metadata.name}`}
                          onClick={() =>
                            onShowTailscaleQr(project, tailscaleUrl)
                          }
                        >
                          <QrCodeSvg
                            value={tailscaleUrl}
                            label={`QR code for ${project.manifest.metadata.name} Tailscale URL`}
                            size={104}
                          />
                        </button>
                        <div className="creation-tailscale-qr-copy">
                          <p className="hint">
                            {tailscaleDirectUrl
                              ? "Named shortcut works while Pockets is open. The direct port URL also stays reachable after quitting Pockets."
                              : "Scan from another device on your Tailscale network. Stays reachable after quitting Pockets while this Mac and Pocket run."}
                          </p>
                          {tailscaleDirectUrl && (
                            <p className="creation-tailscale-direct">
                              Direct:{" "}
                              <code title={tailscaleDirectUrl}>
                                {displayAddress(tailscaleDirectUrl)}
                              </code>
                            </p>
                          )}
                          {tailscale?.message && (
                            <p className="hint">{tailscale.message}</p>
                          )}
                          <div className="creation-tailscale-actions">
                            <button
                              type="button"
                              onClick={() => copyTailscaleUrl(tailscaleUrl)}
                            >
                              {tailscaleCopied ? "Copied" : "Copy URL"}
                            </button>
                            <button
                              type="button"
                              onClick={() =>
                                onShowTailscaleQr(project, tailscaleUrl)
                              }
                            >
                              Enlarge QR
                            </button>
                          </div>
                        </div>
                      </div>
                    </div>
                  )}

                  {tailscaleEnabled && !tailscaleActive && (
                    <div className="creation-tailscale-qr-copy">
                      <p className="hint">
                        {tailscale?.message ??
                          "Paused while stopped — will share automatically when started."}
                      </p>
                      {tailscaleNeedsReconnect && (
                        <div className="creation-tailscale-actions">
                          <button
                            type="button"
                            disabled={busy || !runtimeReady}
                            onClick={() => onToggleTailscale(project, true)}
                          >
                            {tailscaleBusy ? "Reconnecting…" : "Reconnect"}
                          </button>
                        </div>
                      )}
                    </div>
                  )}

                  {tailscaleError && (
                    <p className="error" role="alert">
                      {tailscaleError}
                    </p>
                  )}
                </div>
              </details>
            )}
            <details className="creation-data">
              <summary className="section-disclosure">
                Data
                <span className="creation-disclosure-meta">
                  <span>
                    {built ? "Local Podman volume" : "Volume created on build"}
                  </span>
                  {" · "}
                  <span>
                    {
                      (project.installed?.manifest ?? project.manifest).storage
                        .path
                    }
                  </span>
                </span>
              </summary>
              <div className="creation-data-body">
                <p className="hint creation-data-note">
                  {built
                    ? "Saved inside Podman, separate from your source folder. Removed if you delete this Pocket."
                    : "Podman creates a local volume when built, separate from your source folder and preview data."}
                </p>
                {built && (
                  <>
                    <div className="creation-data-actions">
                      <button
                        type="button"
                        disabled={busy || !runtimeReady}
                        onClick={() => onExportData(project)}
                      >
                        {exportingData ? "Exporting…" : "Export backup…"}
                      </button>
                      <button
                        type="button"
                        disabled={busy || !runtimeReady}
                        onClick={() => onAskRestoreData(project)}
                      >
                        Restore backup…
                      </button>
                    </div>
                    <div className="creation-data-reset">
                      <div className="creation-data-reset-text">
                        <span>Reset data</span>
                        <span className="hint">
                          Erase everything this Pocket has saved.
                        </span>
                      </div>
                      <button
                        type="button"
                        className="creation-delete"
                        aria-label="Reset data"
                        disabled={busy || !runtimeReady}
                        onClick={() => onAskResetData(project)}
                      >
                        Reset…
                      </button>
                    </div>
                  </>
                )}
                {dataFeedback && (
                  <p
                    className={dataFeedback.tone === "error" ? "error" : "hint"}
                    role={dataFeedback.tone === "error" ? "alert" : "status"}
                  >
                    {dataFeedback.message}
                  </p>
                )}
              </div>
            </details>

            {/* Upstream Git updates (for Hub/GitHub-pulled or remote-tracked Pockets). */}
            {project.developmentLinked && (
              <UpstreamUpdates
                projectId={project.id}
                built={built}
                workingDigest={project.sourceDigest}
                disabled={busy}
                onPulled={onRestored}
              />
            )}

            {/* Versions: what was made live, and what changed each time. */}
            {built && (
              <Versions
                projectId={project.id}
                workingDigest={project.sourceDigest}
                installedDigest={installedDigest}
                liveSince={project.installed?.builtAtSeconds ?? null}
                disabled={busy}
                onRestored={onRestored}
              />
            )}

            {/* A linked folder is the source: undo happens there (git or the
                coding tool), so Pockets' snapshot restore is only offered
                when its managed copy is the only source. */}
            {!project.developmentLinked && (
              <RevisionHistory
                projectId={project.id}
                workingDigest={project.sourceDigest}
                installedDigest={installedDigest}
                onRestored={onRestored}
                disabled={busy}
              />
            )}

            <div className="creation-details-disclosure">
              <button
                type="button"
                className="section-disclosure"
                aria-expanded={expanded}
                aria-controls={detailsId}
                onClick={() => setExpanded((open) => !open)}
              >
                Technical details
              </button>

              {expanded && (
                <div id={detailsId} className="creation-review">
                  <ul className="creation-notes">
                    <li>
                      Building runs the app&apos;s generated Containerfile
                      instructions and can download packages from the internet.
                      Build code you trust.
                    </li>
                    <li>
                      Imports are filtered snapshots: local data,{" "}
                      <code>.env</code> files, and dependency caches are not
                      included.
                    </li>
                    <li>
                      The installed Pocket runs from a snapshot. Link a
                      development folder and use Build and start or Update to
                      sync and apply later edits into the same Pocket.
                    </li>
                    <li>
                      Each Pocket keeps its data in its own volume; your
                      original folder is never modified.
                    </li>
                  </ul>

                  <div className="creation-review-section">
                    <h5 className="creation-review-title">Containerfile</h5>
                    {/* Rendered as a text child, so React escapes the untrusted
                      Containerfile; it is never interpreted as markup. */}
                    <pre className="creation-containerfile">
                      {project.containerfile}
                    </pre>
                  </div>

                  <div className="creation-review-section">
                    <h5 className="creation-review-title">
                      Snapshot files{" "}
                      <span className="creation-meta">
                        ({project.files.length} files ·{" "}
                        {formatBytes(project.totalBytes)})
                      </span>
                    </h5>
                    <ul className="creation-files">
                      {project.files.map((file) => (
                        <li key={file}>
                          <FileIcon path={file} />
                          <code>{file}</code>
                        </li>
                      ))}
                    </ul>
                  </div>

                  <p className="creation-digest">
                    Working source digest{" "}
                    <code>{shortDigest(project.sourceDigest)}</code>
                  </p>

                  {project.installed != null && (
                    <p className="creation-digest">
                      Installed source digest{" "}
                      <code>{shortDigest(project.installed.sourceDigest)}</code>{" "}
                      {pending
                        ? "— waiting on Update"
                        : "— matches the working source"}
                    </p>
                  )}

                  {builtImage !== null && (
                    <p className="creation-digest">
                      Built image <code>{shortImageId(builtImage)}</code>
                    </p>
                  )}

                  {statusValue && (
                    <p className="creation-digest">
                      Status message {statusValue.message}
                    </p>
                  )}

                  {build?.status && (
                    <div className="creation-build-log-block">
                      <h5 className="creation-review-title">Build output</h5>
                      {/* Native-owned log, escaped as a text child like the
                        Containerfile. */}
                      {build.status.log !== "" && (
                        <pre className="creation-build-log">
                          {build.status.log}
                        </pre>
                      )}
                      {build.status.truncated && (
                        <p className="hint creation-log-truncated">
                          The log is longer than native keeps — earlier output
                          was trimmed.
                        </p>
                      )}
                    </div>
                  )}
                </div>
              )}
            </div>
          </div>
        )}
      </div>
    </article>
  );
}
