import { useEffect, useRef, useState, type ReactNode } from "react";
import { invoke } from "./api";
import type { PocketCatalog } from "./CreationPanel";
import { formatBytes, formatPercent } from "./formatBytes";
import { groupByLineage } from "./projectLineage";
import type { ThemeMode } from "./theme";
import {
  IconChevronDown,
  IconDraft,
  IconGrid,
  IconMonitor,
  IconMoon,
  IconPanelLeft,
  IconPlus,
  IconPocketsMark,
  IconSun,
} from "./Icons";
import { IconButton, Spinner, StatusPip, type PipTone } from "./Status";
import { TitleBar } from "./TitleBar";
import "./PocketSidebar.css";

/**
 * One persisted project, derived from the catalog that CreationPanel owns so
 * the sidebar never redeclares (and drifts from) the native record shape.
 */
type Project = PocketCatalog["projects"][number];

/** One project's lifecycle read: not yet asked, being asked, failed, or known. */
type StatusEntry = PocketCatalog["statuses"][string];

export interface PocketSidebarProps {
  catalog: PocketCatalog;
  selectedProjectId: string | null;
  onSelectProject: (id: string) => void;
  onNewPocket: () => void;
  /** The routed page; marks the matching section row `aria-current="page"`.
   *  The list heading itself is the static word "Pockets" (v3 §3.3) — the
   *  page title moved to the main column's header. */
  page: "workspace" | "live" | "drafts";
  onShowLive: () => void;
  onShowDrafts: () => void;
  /** Collapse the column: the toggle now lives in this column's title row,
   *  because the AppNav rail is gone (v3 §1.2, §3). */
  onToggleCollapsed: () => void;
  /** The footer's host word (v3 §3.5), App-supplied like the count below. */
  hostLabel: string;
  /** Trustworthy running-pocket count moved here from App unchanged; `null`
   *  when no lifecycle status is known, and the footer shows the host alone. */
  runningCount: number | null;
  /** Optional theme mode control surfaced in the sidebar footer so appearance
   *  is reachable without leaving the workspace. */
  themeMode?: ThemeMode;
  onChangeThemeMode?: (mode: ThemeMode) => void;
  /** Optional listener receiving the latest `resource_snapshot` from this
   *  column's existing poll so other views can reuse per-Pocket metrics
   *  without issuing duplicate native `podman stats` calls. */
  onResourceSnapshot?: (snapshot: ResourceSnapshot) => void;
}

/** Per-Pocket usage record returned by `resource_snapshot`. */
export interface PocketUsage {
  id: string;
  name: string;
  state: "running" | "stopped" | "unavailable";
  cpuPercent: number | null;
  memoryBytes: number | null;
}

/**
 * The exact part of the `resource_snapshot` reply the shell may read:
 * camelCase wire fields per `src-tauri/src/resource_monitor.rs`, where a
 * total is `null` whenever the sum could not be honestly computed.
 */
interface ResourceTotals {
  cpuPercent: number | null;
  memoryBytes: number | null;
}

export interface ResourceSnapshot {
  pockets?: PocketUsage[];
  totals?: ResourceTotals;
}

/** Same cadence as the ResourceMonitor's poll. */
const REFRESH_MS = 5000;

/** Row status: a square pip + capitalized word (v2 §4) plus any honest
    extra detail the status record itself carried. */
interface StatusView {
  tone: PipTone;
  word: string;
  detail: string | null;
}

/**
 * The single honest status mapping (the v1 truth rules, re-worded for v2:
 * pip + capitalized word, never a text glyph). An unresolved or failed read
 * is rendered as such ("Checking", "Status unavailable") — the sidebar
 * never guesses a lifecycle state it did not receive, and the manifest's
 * container port and the status URL are never surfaced as a host address.
 */
function statusView(
  project: Project,
  entry: StatusEntry | undefined,
  building: boolean,
): StatusView {
  if (building) {
    return { tone: "warn", word: "Building", detail: null };
  }
  if (project.phase === "draft") {
    return { tone: "stopped", word: "Draft", detail: null };
  }
  if (entry === undefined || entry.status === "loading") {
    return { tone: "warn", word: "Checking", detail: null };
  }
  if (entry.status === "error") {
    return {
      tone: "warn",
      word: "Status unavailable",
      detail: entry.message,
    };
  }
  if (entry.value.state === "running") {
    return entry.value.ready
      ? { tone: "ok", word: "Running", detail: null }
      : {
          tone: "warn",
          word: "Starting",
          detail: entry.value.message || null,
        };
  }
  return { tone: "stopped", word: "Stopped", detail: null };
}

/** A number the backend actually measured; anything else renders nothing. */
function usableValue(value: number | null | undefined): number | null {
  return typeof value === "number" && Number.isFinite(value) && value >= 0
    ? value
    : null;
}

/** The CPU bar's fill: the real percentage, clamped to the bar's geometry
    — a multi-core total prints its number, the track only holds 100%. */
function barWidth(value: number): number {
  return Math.min(100, Math.max(0, Math.round(value * 10) / 10));
}

const THEME_CYCLE: Record<ThemeMode, ThemeMode> = {
  system: "light",
  light: "dark",
  dark: "system",
};

const THEME_LABELS: Record<ThemeMode, string> = {
  system: "System",
  light: "Light",
  dark: "Dark",
};

/**
 * The 248px pockets column of the v3 shell (pockets-design-update-v3.md §3):
 * a shared `TitleBar` row (sidebar toggle leading, New pocket trailing — the
 * 40px body button is gone), the two management section rows that replace
 * the deleted AppNav rail, the static "Pockets" label over one two-line
 * button per pocket, and a footer carrying the host label plus the
 * trustworthy running count and, inside Tauri with a real sample, the
 * honest resource meters. All management (build, start, stop, delete,
 * settings) stays in CreationPanel. The column never collapses onto the
 * chat: the title row's toggle hides it outright.
 */
export default function PocketSidebar({
  catalog,
  selectedProjectId,
  onSelectProject,
  onNewPocket,
  page,
  onShowLive,
  onShowDrafts,
  onToggleCollapsed,
  hostLabel,
  runningCount,
  themeMode,
  onChangeThemeMode,
  onResourceSnapshot,
}: PocketSidebarProps) {
  const [totals, setTotals] = useState<ResourceTotals | null>(null);
  const onSnapshotRef = useRef(onResourceSnapshot);
  useEffect(() => {
    onSnapshotRef.current = onResourceSnapshot;
  });
  const projects = catalog.projects;
  const total = projects.length;

  // The same Tauri test App uses: outside the native webview there is no
  // `resource_snapshot` command, so no request is made and the meters do not
  // render at all — no zeros, no spinner that can never finish.
  const inTauri = true;

  useEffect(() => {
    if (!inTauri) return;
    // Self-rescheduling poll with one request in flight, same as the
    // ResourceMonitor: the next sample is armed in the previous reply's
    // finally, polling pauses while the page is hidden and stops for good
    // on unmount. A failed read stays invisible — with no trustworthy
    // sample the meters simply stay absent; a previous real sample stays.
    let active = true;
    let timer: number | null = null;
    let inFlight = false;

    const schedule = () => {
      if (timer !== null) {
        window.clearTimeout(timer);
        timer = null;
      }
      if (document.visibilityState === "hidden") return;
      timer = window.setTimeout(() => void sample(), REFRESH_MS);
    };

    const sample = async () => {
      if (!active || inFlight || document.visibilityState === "hidden") {
        return;
      }
      inFlight = true;
      try {
        const value = await invoke<ResourceSnapshot>("resource_snapshot");
        if (active) {
          setTotals(value.totals ?? null);
          onSnapshotRef.current?.(value);
        }
      } catch {
        // Deliberately silent: the meters show nothing rather than a value
        // that was not measured.
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
  }, [inTauri]);

  const cpu = usableValue(totals?.cpuPercent);
  const memory = usableValue(totals?.memoryBytes);

  // Section-row counts (v3 §3.1): a plain tally of the persisted `phase`
  // over the same catalog the list renders — two honest numbers from the
  // records themselves, no second data source to drift from.
  const liveCount = projects.filter((p) => p.phase === "installed").length;
  const draftCount = projects.filter((p) => p.phase === "draft").length;

  // Lineage grouping from persisted `editedFrom` only (see
  // projectLineage.ts): an installed parent carries its imported drafts as
  // children; everything else renders exactly as a flat row.
  const groups = groupByLineage(projects);

  // Which parents have their drafts shown. Unset means "follow the
  // selection": a group opens by itself while one of its drafts is the
  // selected pocket, and stays folded otherwise.
  const [openGroups, setOpenGroups] = useState<Record<string, boolean>>({});

  const displayName = (project: Project) => {
    const name = project.manifest.metadata.name.trim();
    return name === "" ? project.id : name;
  };

  // The two-line row button for one pocket, shared by top-level rows and
  // lineage children so both carry identical markup, names and
  // `aria-current` behavior — the only difference is where the row sits.
  const rowButton = (
    project: Project,
    options: { label?: string; draftCount?: number } = {},
  ) => {
    const selected = project.id === selectedProjectId;
    const view = statusView(
      project,
      catalog.statuses[project.id],
      catalog.buildingProjectId === project.id,
    );
    const name = project.manifest.metadata.name.trim();
    return (
      <button
        type="button"
        className="pocket-sidebar-item"
        aria-current={selected ? "true" : undefined}
        onClick={() => onSelectProject(project.id)}
      >
        <span className="pocket-sidebar-name">
          {options.label ?? (name === "" ? project.id : name)}
        </span>
        <span className="pocket-sidebar-state" title={view.detail ?? undefined}>
          <StatusPip tone={view.tone} word={view.word} />
          {view.detail !== null && (
            <span className="pocket-sidebar-detail">{`· ${view.detail}`}</span>
          )}
          {options.draftCount !== undefined && (
            <span className="pocket-sidebar-detail">
              {`· ${options.draftCount} ${options.draftCount === 1 ? "draft" : "drafts"}`}
            </span>
          )}
        </span>
      </button>
    );
  };

  // One management page row (v3 §3.1): icon, label and count on a single
  // line, styled exactly like a pocket row so page rows and pocket rows read
  // as the same row type. `aria-current="page"` marks the open page.
  const sectionRow = (
    label: string,
    icon: ReactNode,
    count: number,
    current: boolean,
    onClick: () => void,
    shortcutTitle?: string,
  ) => (
    <button
      type="button"
      className="pocket-sidebar-section"
      aria-current={current ? "page" : undefined}
      title={shortcutTitle}
      onClick={onClick}
    >
      {icon}
      <span className="pocket-sidebar-section-label">{label}</span>
      <span className="pocket-sidebar-section-count">{count}</span>
    </button>
  );

  return (
    <nav
      className="pocket-sidebar"
      aria-label="Pockets"
      data-testid="pocket-sidebar"
      data-loading={String(catalog.loading)}
    >
      {/* This column is only mounted while the sidebar is open, so it is
          then the window's leftmost column and its row reserves the
          traffic lights (v3 §1.5). The §3 Search control is deliberately
          omitted: no search feature exists, and an icon that opens nothing
          is a lie. */}
      <TitleBar
        ariaLabel="Sidebar"
        className="pocket-sidebar-titlebar"
        reserveTrafficLights
        leading={
          <span className="pocket-sidebar-brand">
            <IconPocketsMark size={18} />
            <span className="pocket-sidebar-brand-name">Pockets</span>
          </span>
        }
        trailing={
          <>
            <IconButton
              label="New pocket"
              title="New pocket ⌘N"
              onClick={onNewPocket}
            >
              <IconPlus />
            </IconButton>
            <IconButton
              label="Toggle sidebar"
              title="Toggle sidebar ⌘B"
              pressed
              onClick={onToggleCollapsed}
            >
              <IconPanelLeft />
            </IconButton>
          </>
        }
      />

      <div className="pocket-sidebar-body">
        {sectionRow(
          "Live pockets",
          <IconGrid />,
          liveCount,
          page === "live",
          onShowLive,
          "Live pockets ⌘2",
        )}
        {sectionRow(
          "Drafts",
          <IconDraft />,
          draftCount,
          page === "drafts",
          onShowDrafts,
          "Drafts ⌘3",
        )}

        <div className="pocket-sidebar-divider" aria-hidden="true" />

        <ul className="pocket-sidebar-list">
          {catalog.error !== null && (
            <li className="pocket-sidebar-note pocket-sidebar-tone-warn">
              {`Could not load pockets: ${catalog.error}`}
            </li>
          )}
          {catalog.error === null && total === 0 && (
            <li className="pocket-sidebar-note">
              {catalog.loading ? (
                <>
                  <Spinner />
                  {"Loading pockets…"}
                </>
              ) : (
                "None yet — create or link a pocket"
              )}
            </li>
          )}
          {groups.map(({ project, drafts }) =>
            drafts.length === 0 ? (
              <li key={project.id}>{rowButton(project)}</li>
            ) : (
              // A parent with lineage drafts: the wrapper `<li>` is the
              // nesting marker (and the CSS indent hook), the nested `<ul>`
              // holds only the recorded children. No invented status word
              // marks the relationship — placement is the whole claim, and
              // it exists only because native persisted `editedFrom`.
              //
              // The drafts fold under the parent: the parent row says how
              // many there are, and a chevron beside it shows them. Each
              // child keeps its own name when it was renamed; one that
              // still carries the parent's name reads "Edited copy" (and a
              // number when there are several) instead of repeating it.
              <GroupRows
                key={project.id}
                parent={project}
                drafts={drafts}
                open={
                  openGroups[project.id] ??
                  drafts.some((draft) => draft.id === selectedProjectId)
                }
                onToggle={(open) =>
                  setOpenGroups((current) => ({
                    ...current,
                    [project.id]: open,
                  }))
                }
                parentName={displayName(project)}
                childLabel={(draft, index) =>
                  displayName(draft) === displayName(project)
                    ? drafts.length === 1
                      ? "Edited copy"
                      : `Edited copy ${index + 1}`
                    : displayName(draft)
                }
                rowButton={rowButton}
              />
            ),
          )}
        </ul>
      </div>

      {/* The host row (§3.5) is static, App-supplied text plus the one
          trustworthy count moved from App unchanged, so it renders whenever
          the column does. What stays honest-only is the meters block below:
          the CPU and Memory meters can show nothing but `resource_snapshot`
          totals — no storage meter and no host disk percentage (the backend
          deliberately samples no host data, resource_monitor.rs); no memory
          percentage or bar — the snapshot carries absolute bytes and no
          honest denominator; and no ":port" text anywhere — manifest.rs
          documents that a manifest never supplies a host port. Without a
          trustworthy sample (or outside Tauri) the meters stay absent. */}
      <div className="pocket-sidebar-footer">
        <div className="pocket-sidebar-host">
          <span className="pocket-sidebar-host-label">{hostLabel}</span>
          {runningCount !== null && (
            <StatusPip
              tone={runningCount > 0 ? "ok" : "stopped"}
              word={`${runningCount} running`}
            />
          )}
        </div>

        {inTauri && (cpu !== null || memory !== null) && (
          <div className="pocket-sidebar-meters">
            {cpu !== null && (
              <div className="pocket-sidebar-meter">
                <div className="pocket-sidebar-meter-row">
                  <span className="pocket-sidebar-meter-label">CPU</span>
                  <span className="pocket-sidebar-meter-value">
                    {formatPercent(cpu)}
                  </span>
                </div>
                <div className="pocket-sidebar-meter-bar" aria-hidden="true">
                  <span
                    className="pocket-sidebar-meter-fill"
                    style={{ width: `${barWidth(cpu)}%` }}
                  />
                </div>
              </div>
            )}
            {memory !== null && (
              // Absolute amount only: a memory bar would need a total the
              // snapshot does not carry.
              <div className="pocket-sidebar-meter">
                <div className="pocket-sidebar-meter-row">
                  <span className="pocket-sidebar-meter-label">Memory</span>
                  <span className="pocket-sidebar-meter-value">
                    {formatBytes(memory)}
                  </span>
                </div>
              </div>
            )}
          </div>
        )}

        {themeMode !== undefined && onChangeThemeMode !== undefined && (
          <button
            type="button"
            className="pocket-sidebar-theme"
            aria-label={`Theme: ${THEME_LABELS[themeMode]}. Switch to ${THEME_LABELS[THEME_CYCLE[themeMode]]}`}
            title={`Switch theme to ${THEME_LABELS[THEME_CYCLE[themeMode]]}`}
            onClick={() => onChangeThemeMode(THEME_CYCLE[themeMode])}
          >
            {themeMode === "light" ? (
              <IconSun size={14} />
            ) : themeMode === "dark" ? (
              <IconMoon size={14} />
            ) : (
              <IconMonitor size={14} />
            )}
            <span className="pocket-sidebar-theme-label">Theme</span>
            <span className="pocket-sidebar-theme-value">
              {THEME_LABELS[themeMode]}
            </span>
          </button>
        )}
      </div>
    </nav>
  );
}

/** One installed parent with its lineage drafts folded beneath it. */
function GroupRows({
  parent,
  drafts,
  open,
  onToggle,
  parentName,
  childLabel,
  rowButton,
}: {
  parent: Project;
  drafts: Project[];
  open: boolean;
  onToggle: (open: boolean) => void;
  parentName: string;
  childLabel: (draft: Project, index: number) => string;
  rowButton: (
    project: Project,
    options?: { label?: string; draftCount?: number },
  ) => ReactNode;
}) {
  const listId = `pocket-sidebar-drafts-${parent.id}`;
  return (
    <li className="pocket-sidebar-group">
      <div className="pocket-sidebar-row">
        {rowButton(parent, { draftCount: drafts.length })}
        <button
          type="button"
          className="pocket-sidebar-disclosure"
          aria-expanded={open}
          aria-controls={listId}
          aria-label={`${open ? "Hide" : "Show"} drafts of ${parentName}`}
          title={open ? "Hide drafts" : "Show drafts"}
          onClick={() => onToggle(!open)}
        >
          <IconChevronDown size={14} />
        </button>
      </div>
      {open && (
        <ul id={listId} className="pocket-sidebar-children">
          {drafts.map((draft, index) => (
            <li key={draft.id}>
              {rowButton(draft, { label: childLabel(draft, index) })}
            </li>
          ))}
        </ul>
      )}
    </li>
  );
}
