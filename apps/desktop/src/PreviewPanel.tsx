import { useCallback, useEffect, useRef, useState } from "react";
import type { PreviewState, ProjectPreview } from "./useProjectPreview";
import { IconCheck, IconCopy, IconExternalLink, IconPanelRight } from "./Icons";
import { IconButton, type PipTone } from "./Status";
import { TitleBar } from "./TitleBar";
import "./PreviewPanel.css";

/**
 * The rect the panel reserves for the native preview pane, in CSS logical
 * pixels as reported by the viewport element's own bounding box (never
 * device pixels). T3's native child webview consumes this through
 * `project_preview_set_bounds`, which maps it to window coordinates and
 * repositions the pane — the spike measured that plumbing at median 1 ms,
 * so reporting on every resize is cheap and always current.
 */
export interface PreviewBounds {
  x: number;
  y: number;
  width: number;
  height: number;
}

/**
 * The URL-strip state word plus the pip tone that travels with it (v2 §7).
 * These are the user's words, not the wire enum: `ready` reads "Live" and
 * `updating` reads "Rebuilding…". The pip is never alone — the word sits at
 * the right end of the same strip.
 */
const PREVIEW_STATE_VIEWS: Record<
  PreviewState,
  { word: string; tone: PipTone }
> = {
  preparing: { word: "Preparing…", tone: "warn" },
  updating: { word: "Rebuilding…", tone: "warn" },
  ready: { word: "Ready", tone: "ok" },
  failed: { word: "Preview error", tone: "warn" },
  stopped: { word: "Stopped", tone: "stopped" },
};

/** States with something running natively: Stop can act on them. */
const ACTIVE_STATES: PreviewState[] = [
  "preparing",
  "updating",
  "ready",
  "failed",
];

/**
 * Display form of the loopback proxy URL (v2 §7): the `http://` scheme and
 * the trailing slash are noise next to the state word. It stays plain text —
 * the frontend never anchors or navigates to a status URL.
 */
function displayPreviewUrl(url: string): string {
  return url.replace(/^https?:\/\//, "").replace(/\/$/, "");
}

/**
 * The dedupe key for coalesced bounds reports (v3 §6.5): the rounded rect,
 * so sub-pixel jitter inside one drag never doubles a placement report.
 */
function boundsKey(bounds: PreviewBounds): string {
  return [
    Math.round(bounds.x),
    Math.round(bounds.y),
    Math.round(bounds.width),
    Math.round(bounds.height),
  ].join("|");
}

/** Project preview status and the measured viewport for the native pane. */
export default function PreviewPanel({
  preview,
  newProject = false,
  onBounds,
  onTogglePreview,
  overlayOpen = false,
  pocketName = null,
  snapshotUrl = null,
}: {
  preview: ProjectPreview;
  newProject?: boolean;
  onBounds?: (bounds: PreviewBounds) => void;
  /** Collapse this column (v3 §6.1): the title row's `IconPanelRight`
   *  toggle. It reads `aria-pressed="true"` because the column is open
   *  exactly while this panel renders. */
  onTogglePreview?: () => void;
  /** An in-app overlay currently covers the preview area and App has
   *  hidden the native pane (v3 §9.2). The viewport shows the stored
   *  snapshot, or the pocket name, in place of the live affordances — and
   *  stays mounted and reporting bounds, so the restore lands the pane at
   *  exactly this rect when the overlay closes. */
  overlayOpen?: boolean;
  /** Pocket name for the overlay fallback, used when there is no snapshot. */
  pocketName?: string | null;
  /** Stored snapshot data URL for the overlay fallback, or null. */
  snapshotUrl?: string | null;
}) {
  const {
    projectId,
    status,
    stale,
    starting,
    startError,
    stopping,
    stopError,
    openBusy,
    openNotice,
    lastEventMessage,
  } = preview;

  // Latest callback without re-arming the observer when App re-renders.
  const onBoundsRef = useRef(onBounds);
  useEffect(() => {
    onBoundsRef.current = onBounds;
  });

  // The viewport region is the reserved rect, not the card: the native pane
  // covers exactly this box, leaving the title row and the strips above it
  // clickable. The region renders with or without a session and with or
  // without an overlay (v3 §9.2) — under an overlay only what it shows
  // changes, never whether it measures — so the placement target is stable
  // through every chrome state.
  const viewportRef = useRef<HTMLDivElement | null>(null);
  // Coalescing state (v3 §6.5): at most one queued frame, plus the rounded
  // rect last handed to App so a frame whose rect has not moved sends
  // nothing. The immediate path records the key too, so the observer's
  // first delivery after a placement report never duplicates it.
  const frameRef = useRef<number | null>(null);
  const lastBoundsKeyRef = useRef<string | null>(null);

  /**
   * Read the viewport rect and report it now. Hoisted so the session/state
   * effect and the overlay restore below can re-report on demand, not only
   * on mount. Placement triggers use this path — mount, session arrival,
   * state/URL transitions (the native create gate lives on these reports,
   * docs/live-preview.md), overlay restore — so they never wait on a frame
   * and never dedupe: App must see them even when the rect did not move.
   */
  const reportBounds = useCallback(() => {
    const callback = onBoundsRef.current;
    const element = viewportRef.current;
    if (callback === undefined || element === null) return;
    const rect = element.getBoundingClientRect();
    const bounds = {
      x: rect.left,
      y: rect.top,
      width: rect.width,
      height: rect.height,
    };
    lastBoundsKeyRef.current = boundsKey(bounds);
    callback(bounds);
  }, []);

  /**
   * The continuous signals — ResizeObserver, scroll, window resize — land
   * here instead of reporting directly (v3 §6.5): everything raised within
   * one frame produces ONE report of the freshest rect, skipped whole when
   * the rounded rect has not moved since the last send. With no other
   * throttling the pane still tracks every frame of a window resize or a
   * divider drag; the coalescing only removes same-frame redundancy.
   */
  const scheduleBounds = useCallback(() => {
    if (frameRef.current !== null) return;
    frameRef.current = requestAnimationFrame(() => {
      frameRef.current = null;
      const callback = onBoundsRef.current;
      const element = viewportRef.current;
      if (callback === undefined || element === null) return;
      const rect = element.getBoundingClientRect();
      const bounds = {
        x: rect.left,
        y: rect.top,
        width: rect.width,
        height: rect.height,
      };
      const key = boundsKey(bounds);
      if (key === lastBoundsKeyRef.current) return;
      lastBoundsKeyRef.current = key;
      callback(bounds);
    });
  }, []);

  useEffect(() => {
    const element = viewportRef.current;
    if (element === null) return;
    // Report once on mount so placement does not wait for the observer's
    // first delivery; ResizeObserver covers every later resize.
    reportBounds();
    const observer = new ResizeObserver(() => scheduleBounds());
    observer.observe(element);
    // The column can move without changing size: with a pinned preview
    // width, collapsing or opening the sidebar shifts the rect sideways
    // only. The main column does resize then, so watching it catches the
    // move and the pane follows.
    const mainColumn = element.closest(".shell-main");
    if (mainColumn !== null) observer.observe(mainColumn);
    // Scrolling is not resizing: the shell scrolls inside panes (and the
    // document below ~720px), which moves the reserved rect without
    // changing its size — invisible to ResizeObserver, and the pane would
    // drift over the controls. Scroll events do not bubble, so the window
    // listener must capture them from every nested scroller; resize covers
    // window chrome the observer might miss. Each delivered report is still
    // the full rect, advisory, revalidated natively — negatives included —
    // but the reports themselves ride the coalesced frame (v3 §6.5).
    const report = () => scheduleBounds();
    window.addEventListener("scroll", report, true);
    window.addEventListener("resize", report);
    return () => {
      observer.disconnect();
      window.removeEventListener("scroll", report, true);
      window.removeEventListener("resize", report);
      if (frameRef.current !== null) {
        cancelAnimationFrame(frameRef.current);
        frameRef.current = null;
      }
    };
  }, [reportBounds, scheduleBounds]);

  // Re-report when the pane's subject or content identity changes: the
  // mount-time report ran with no session (App's placement callback
  // ignores those), and a proxy restart re-allocates the URL the native
  // pane follows. Native `plan_pane` decides what a report means — create
  // on Ready, skip while it is not time, retarget on a new URL — so these
  // extra reports are idempotent by contract. Without this, a preview
  // started after mount could only appear on a window resize.
  useEffect(() => {
    if (projectId === null) return;
    reportBounds();
  }, [projectId, status?.state, status?.url, reportBounds]);

  // Overlay restore (v3 §9.3): when the last popup closes and App brings
  // the pane back, one fresh report lands it at the rect as it stands now,
  // even if nothing resized while the overlay was up. Mount is not a
  // restore: the ref starts at the mount value, so the mount-time report
  // above stays the only one.
  const overlayOpenRef = useRef(overlayOpen);
  useEffect(() => {
    const wasOpen = overlayOpenRef.current;
    overlayOpenRef.current = overlayOpen;
    if (wasOpen && !overlayOpen) reportBounds();
  }, [overlayOpen, reportBounds]);

  const bound = projectId !== null;
  const state = status?.state ?? null;
  const failedMessage = status?.error ?? lastEventMessage;
  const readyUrl = state === "ready" ? status?.url : undefined;
  const canOpen = readyUrl !== undefined && !openBusy;
  const stateView = state === null ? null : PREVIEW_STATE_VIEWS[state];

  const [urlCopied, setUrlCopied] = useState(false);
  const copyTimerRef = useRef<number | null>(null);
  useEffect(() => {
    setUrlCopied(false);
    return () => {
      if (copyTimerRef.current !== null) {
        window.clearTimeout(copyTimerRef.current);
        copyTimerRef.current = null;
      }
    };
  }, [projectId, readyUrl]);

  const copyPreviewUrl = useCallback(() => {
    if (!readyUrl) return;
    void navigator.clipboard
      ?.writeText(readyUrl)
      .then(() => {
        setUrlCopied(true);
        if (copyTimerRef.current !== null) {
          window.clearTimeout(copyTimerRef.current);
        }
        copyTimerRef.current = window.setTimeout(() => {
          copyTimerRef.current = null;
          setUrlCopied(false);
        }, 2000);
      })
      .catch(() => {
        // Clipboard permission is optional; the URL stays selectable text.
      });
  }, [readyUrl]);

  const hasStatusLine =
    status === null ||
    status.state === "preparing" ||
    status.state === "updating" ||
    status.state === "failed";

  return (
    <section
      className="card preview-panel"
      aria-labelledby="preview-heading"
      aria-busy={starting || stopping}
    >
      {/* Title row (v3 §6.1): the shared 52px `TitleBar` — the old
          `.preview-toolbar` is gone and nothing here wraps (§10). There is
          deliberately no Code/Logs segment and no reload button: this pane
          renders a native child webview, and no command exists to reload or
          read source from it — a control that did nothing would be worse
          than no control. The title names what the pane actually serves:
          `project_preview_open` only ever shows the *workspace source* through
          the preview proxy, never the installed app, so calling it "Live"
          read as the running version and contradicted the sidebar's
          "Running" pip. The collapse toggle is the last control, and it is
          pressed exactly because the column is open while this renders. */}
      <TitleBar
        ariaLabel="Preview"
        title={
          <h2 id="preview-heading" className="preview-title">
            Workspace preview
          </h2>
        }
        trailing={
          <>
            {bound && (
              <>
                <IconButton
                  label="Open in browser"
                  onClick={() => void preview.open()}
                  disabled={!canOpen}
                >
                  <IconExternalLink />
                </IconButton>
                {/* Start covers stopped and failed alike: native `start` is
                    idempotent (a failed entry re-opens `preparing` and re-
                    ensures the owned objects), so a failed preview gets one
                    honest click back. The failure line below stays visible
                    until a new status replaces it — nothing is hidden. */}
                {(state === null ||
                  state === "stopped" ||
                  state === "failed") && (
                  <button
                    type="button"
                    className="preview-power"
                    onClick={() => void preview.start()}
                    disabled={starting || stopping}
                  >
                    {starting ? "Starting…" : "Start"}
                  </button>
                )}
                {state !== null && ACTIVE_STATES.includes(state) && (
                  <button
                    type="button"
                    className="preview-power"
                    onClick={() => void preview.stop()}
                    disabled={starting || stopping}
                  >
                    {stopping ? "Stopping…" : "Stop"}
                  </button>
                )}
              </>
            )}
            <IconButton
              label="Toggle preview"
              pressed
              onClick={onTogglePreview}
            >
              <IconPanelRight />
            </IconButton>
          </>
        }
      />

      {/* URL strip (v3 §6.2): a full-width row directly under the title —
          pip + address on the left, the state word on the right. Before the
          first read there is no state to name, so the strip says so plainly
          instead of guessing. */}
      {bound && (
        <div className="preview-url-bar">
          {stateView !== null && (
            <span
              className="status-pip"
              data-tone={stateView.tone}
              aria-hidden="true"
            />
          )}
          {readyUrl !== undefined ? (
            /* Text only on purpose: this is the loopback proxy URL, and the
               frontend must never navigate to or link it — same rule as every
               other status URL in this app. The native pane covers the
               viewport below when live; this is the copyable fallback. */
            <>
              <span className="preview-url">{displayPreviewUrl(readyUrl)}</span>
              <IconButton
                label="Copy preview URL"
                title={urlCopied ? "URL copied" : "Copy preview URL"}
                className="preview-url-copy"
                onClick={copyPreviewUrl}
              >
                {urlCopied ? (
                  <IconCheck size={14} className="tone-live" />
                ) : (
                  <IconCopy size={14} />
                )}
              </IconButton>
            </>
          ) : (
            <span className="preview-url-empty">
              {status === null
                ? "No address yet"
                : "No address while the preview is not live"}
            </span>
          )}
          <span className="preview-state">
            {stateView?.word ?? "Checking…"}
          </span>
        </div>
      )}

      {/* Status lane (v3 §6.3): renders only when there is transitional
          progress, an error, a stale refresh warning, or an open notice —
          avoiding a permanent banner duplicating Ready/Stopped from the URL
          strip above so the live preview gets maximum vertical height. */}
      {bound && (
        <div className="preview-status" aria-live="polite">
          {status === null && (
            <p className="hint">{stale ?? "Checking the preview…"}</p>
          )}
          {hasStatusLine && status !== null && (
            <p className={status.state === "failed" ? "error" : undefined}>
              {status.state === "preparing" &&
                "Getting the preview container ready — no page is live yet."}
              {status.state === "updating" &&
                "Hot-reloading your edits — the page updates on its own once the app answers again."}
              {status.state === "failed" &&
                `The preview failed. ${failedMessage ?? "No reason was reported."}`}
            </p>
          )}
          {status !== null && stale !== null && (
            <p className="hint">
              Could not refresh the preview status. {stale} The state above is
              the last one reported.
            </p>
          )}
          {startError !== null && <p className="error">{startError}</p>}
          {stopError !== null && <p className="error">{stopError}</p>}
          {openNotice !== null && <p className="hint">{openNotice}</p>}
        </div>
      )}

      {/* The reserved pane region: the rect reported to
          `project_preview_set_bounds`, flush with the card edges (v3 §6.4).
          While nothing is live it reads as an intentional harness area —
          the panel's guidance renders as a centred child, never as padding
          on the region itself — and once Ready lands the native pane covers
          it. Under an open overlay (v3 §9.2) App has hidden the pane: the
          same element keeps measuring and reporting while the stored
          snapshot, or the pocket name, stands in for the live affordances. */}
      <div className="preview-viewport" ref={viewportRef}>
        {overlayOpen ? (
          snapshotUrl !== null ? (
            <img
              src={snapshotUrl}
              alt=""
              className="preview-overlay-snapshot"
            />
          ) : pocketName !== null ? (
            <p className="preview-overlay-name">{pocketName}</p>
          ) : null
        ) : (
          <>
            {!bound &&
              (newProject ? (
                <div className="preview-empty-guide">
                  <p className="preview-empty-title">Live workspace preview</p>
                  <p className="muted preview-empty">
                    Create or link a project to begin. In its development
                    workspace, choose Start to preview saved edits from your
                    coding tool.
                  </p>
                  <ol className="preview-empty-steps">
                    <li>Create a starter folder or link an existing Pocket</li>
                    <li>Edit source files in your preferred coding tool</li>
                    <li>Watch live reloads here, then Sync &amp; Build</li>
                  </ol>
                </div>
              ) : (
                <p className="muted preview-empty">
                  Link or export this Pocket’s saved source to enable live
                  preview.
                </p>
              ))}
            {bound && state === "ready" && readyUrl !== undefined && (
              <iframe
                src={readyUrl}
                title="Workspace preview"
                className="preview-iframe"
                sandbox="allow-scripts allow-forms allow-same-origin allow-popups"
              />
            )}
            {bound && state !== "ready" && (
              <div className="preview-viewport-prompt">
                <p className="hint">
                  Saved edits refresh automatically while the preview runs.
                </p>
                {(state === "stopped" || state === "failed") && (
                  <button
                    type="button"
                    className="button-primary"
                    onClick={() => void preview.start()}
                    disabled={starting || stopping}
                  >
                    {starting
                      ? "Starting…"
                      : state === "failed"
                        ? "Retry preview"
                        : "Start preview"}
                  </button>
                )}
              </div>
            )}
          </>
        )}
      </div>
    </section>
  );
}
