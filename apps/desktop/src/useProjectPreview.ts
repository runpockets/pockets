import { useCallback, useEffect, useRef, useState } from "react";
import { invoke } from "./api";

/**
 * Preview lifecycle, exactly as native `PreviewState` serializes it
 * (docs/live-preview.md): the proxy SSE and this poll surface share one
 * state machine — `preparing | updating | ready | failed | stopped`.
 */
export type PreviewState =
  "preparing" | "updating" | "ready" | "failed" | "stopped";

/**
 * One recorded transition from the native ring. `message` and `url`/`error`
 * on the status are serde-optional: native omits the key entirely when the
 * transition carries none, so every one of them is optional here too.
 */
interface PreviewEvent {
  revision: number;
  state: PreviewState;
  atSeconds: number;
  message?: string;
}

/** Incremental status returned by the `project_preview_status` poll. */
interface PreviewStatus {
  state: PreviewState;
  /** Monotonic revision: this is the cursor unit polls advance on. */
  revision: number;
  /** Loopback proxy URL, present only while ready. */
  url?: string;
  /** Private Tailscale HTTPS preview URL when accessed from a remote host. */
  remoteUrl?: string;
  /** Transitions with `revision > cursor`, oldest first, ring-bounded. */
  eventsSince: PreviewEvent[];
  /** Revision to pass as the next poll's cursor. */
  nextCursor: number;
  error?: string;
  apiAccess?: ApiAccessReview;
}

interface ApiAccessReview {
  token: string;
  services: { host: string; name: string; purpose: string }[];
}

/** Time between settled preview status requests. */
const POLL_INTERVAL_MS = 700;

/** Native `validate_session_id` requires exactly 32 lowercase hex chars;
 *  the panel mirrors the guard so a malformed prop never reaches native. */
const PROJECT_ID_RE = /^[0-9a-f]{32}$/;

function errorMessage(err: unknown): string {
  if (typeof err === "string") return err;
  if (err instanceof Error) return err.message;
  return "The request failed for an unknown reason.";
}

/** Message of the newest message-bearing transition in one poll page. */
function latestEventMessage(events: PreviewEvent[]): string | null {
  for (let index = events.length - 1; index >= 0; index -= 1) {
    const message = events[index].message;
    if (message) return message;
  }
  return null;
}

export function useProjectPreview(projectId: string | null) {
  const id =
    projectId !== null && PROJECT_ID_RE.test(projectId) ? projectId : null;
  const request = useCallback(
    <T>(
      operation: "status" | "start" | "stop" | "open" | "review_api_access",
      id: string,
      extra: Record<string, unknown> = {},
    ) => invoke<T>(`project_preview_${operation}`, { projectId: id, ...extra }),
    [],
  );
  const [status, setStatus] = useState<PreviewStatus | null>(null);
  const [statusProjectId, setStatusProjectId] = useState(id);
  // Newest transition message seen across polls — the failed-state detail
  // when the status itself carries no error text.
  const [lastEventMessage, setLastEventMessage] = useState<string | null>(null);
  const [stale, setStale] = useState<string | null>(null);
  const [starting, setStarting] = useState(false);
  const [startError, setStartError] = useState<string | null>(null);
  const [reviewing, setReviewing] = useState(false);
  const [reviewError, setReviewError] = useState<string | null>(null);
  const [stopping, setStopping] = useState(false);
  const [stopError, setStopError] = useState<string | null>(null);
  const [openBusy, setOpenBusy] = useState(false);
  const [openNotice, setOpenNotice] = useState<string | null>(null);

  // Poll replies belong to this project until the selection changes.
  const activeIdRef = useRef<string | null>(null);
  // Monotonic effect token so a superseded (or unmounted) poll loop can
  // never apply a late reply — StrictMode's double effect relies on this.
  const sessionRef = useRef(0);
  // The revision cursor across polls, reset whenever the session changes.
  const cursorRef = useRef(0);
  const appliedRevisionRef = useRef(-1);
  // An API decision can change the pending request at the same preview revision.
  const actionVersionRef = useRef(0);

  /** Authoritative status write, ordered by revision so a reply that was
   *  already in flight cannot regress the displayed state. */
  const applyStatus = useCallback((next: PreviewStatus) => {
    if (next.revision < appliedRevisionRef.current) return;
    appliedRevisionRef.current = next.revision;
    setStatus((previous) =>
      previous !== null &&
      previous.revision === next.revision &&
      previous.state === next.state &&
      previous.url === next.url &&
      previous.remoteUrl === next.remoteUrl &&
      previous.error === next.error &&
      previous.apiAccess?.token === next.apiAccess?.token
        ? previous
        : next,
    );
    const message = latestEventMessage(next.eventsSince);
    if (message !== null) setLastEventMessage(message);
  }, []);

  const applyActionStatus = useCallback(
    (next: PreviewStatus) => {
      actionVersionRef.current += 1;
      applyStatus(next);
      setStale(null);
    },
    [applyStatus],
  );

  useEffect(() => {
    const session = ++sessionRef.current;
    activeIdRef.current = id;
    cursorRef.current = 0;
    appliedRevisionRef.current = -1;
    setStatus(null);
    setStatusProjectId(id);
    setLastEventMessage(null);
    setStale(null);
    setStartError(null);
    setReviewError(null);
    setStarting(false);
    setReviewing(false);
    setStopping(false);
    setStopError(null);
    setOpenBusy(false);
    setOpenNotice(null);
    if (id === null) return;

    let timer: number | null = null;
    const alive = () => sessionRef.current === session;
    const clear = () => {
      if (timer !== null) {
        window.clearTimeout(timer);
        timer = null;
      }
    };

    /** Self-rescheduling poll: one read in flight, next tick 700 ms after
     *  this one settles, paused (not merely skipped) while the page is
     *  hidden — the visibilitychange handler resumes with a fresh read. */
    const tick = async () => {
      if (!alive() || activeIdRef.current !== id) return;
      if (document.visibilityState === "hidden") return;
      const actionVersion = actionVersionRef.current;
      const stillCurrent = () =>
        alive() && actionVersionRef.current === actionVersion;
      try {
        const next = await request<PreviewStatus>("status", id, {
          cursor: cursorRef.current,
        });
        if (!stillCurrent()) return;
        cursorRef.current = Math.max(cursorRef.current, next.nextCursor);
        applyStatus(next);
        setStale(null);
      } catch (err) {
        // A status-read failure is not a preview failure: keep showing the
        // last known state clearly marked stale and retry next tick, like
        // ResourceMonitor does with its samples.
        if (!stillCurrent()) return;
        setStale(errorMessage(err));
      } finally {
        if (alive() && activeIdRef.current === id) {
          clear();
          timer = window.setTimeout(() => void tick(), POLL_INTERVAL_MS);
        }
      }
    };

    const onVisibility = () => {
      clear();
      if (document.visibilityState !== "hidden") void tick();
    };
    document.addEventListener("visibilitychange", onVisibility);
    void tick();

    return () => {
      if (sessionRef.current === session) {
        sessionRef.current += 1;
        if (activeIdRef.current === id) activeIdRef.current = null;
      }
      clear();
      document.removeEventListener("visibilitychange", onVisibility);
    };
  }, [id, applyStatus, request]);

  const handleStart = useCallback(async () => {
    if (id === null || activeIdRef.current !== id || starting || stopping)
      return;
    const session = sessionRef.current;
    setStarting(true);
    setStartError(null);
    setReviewError(null);
    try {
      const next = await request<PreviewStatus>("start", id);
      if (sessionRef.current === session) applyActionStatus(next);
    } catch (err) {
      if (sessionRef.current === session) {
        setStartError(`Starting the preview failed. ${errorMessage(err)}`);
      }
    } finally {
      if (sessionRef.current === session) setStarting(false);
    }
  }, [id, starting, stopping, applyActionStatus, request]);

  const review = statusProjectId === id ? (status?.apiAccess ?? null) : null;
  const handleApiDecision = useCallback(
    async (allow: boolean) => {
      if (
        id === null ||
        activeIdRef.current !== id ||
        !review ||
        reviewing ||
        starting ||
        stopping
      )
        return;
      const session = sessionRef.current;
      setReviewing(true);
      setReviewError(null);
      try {
        const next = await request<PreviewStatus>("review_api_access", id, {
          decision: { token: review.token, allow },
        });
        if (sessionRef.current === session) applyActionStatus(next);
      } catch (err) {
        if (sessionRef.current === session) setReviewError(errorMessage(err));
      } finally {
        if (sessionRef.current === session) setReviewing(false);
      }
    },
    [id, review, reviewing, starting, stopping, applyActionStatus, request],
  );

  const handleStop = useCallback(async () => {
    if (id === null || activeIdRef.current !== id || stopping) return;
    const session = sessionRef.current;
    setStopping(true);
    setStopError(null);
    try {
      const stopped = await request<boolean>("stop", id);
      if (sessionRef.current !== session) return;
      if (!stopped) {
        // Native found nothing to stop: the next poll shows the real state,
        // so this is only a quiet explanation, never a failure claim.
        setStopError("There was no active preview to stop.");
      }
    } catch (err) {
      if (sessionRef.current !== session) return;
      setStopError(`Stopping the preview failed. ${errorMessage(err)}`);
    } finally {
      if (sessionRef.current === session) setStopping(false);
    }
  }, [id, stopping, request]);

  /**
   * Escape hatch to the system browser on the same proxy URL the pane shows.
   * Native refuses unless the session's preview is Ready with a loopback URL,
   * so the rejection reason is surfaced verbatim — opening is advisory and
   * never leaves the panel in a claimed-open state.
   */
  const handleOpen = useCallback(async () => {
    if (id === null || activeIdRef.current !== id || openBusy) return;
    const session = sessionRef.current;
    setOpenBusy(true);
    setOpenNotice(null);
    try {
      await request<void>("open", id);
    } catch (err) {
      if (sessionRef.current === session) {
        setOpenNotice(`Could not open the preview. ${errorMessage(err)}`);
      }
    } finally {
      if (sessionRef.current === session) setOpenBusy(false);
    }
  }, [id, openBusy, request]);

  const current = statusProjectId === id;
  return {
    projectId: id,
    status: current ? status : null,
    stale: current ? stale : null,
    starting: current && starting,
    startError: current ? startError : null,
    stopping: current && stopping,
    stopError: current ? stopError : null,
    openBusy: current && openBusy,
    openNotice: current ? openNotice : null,
    lastEventMessage: current ? lastEventMessage : null,
    start: handleStart,
    stop: handleStop,
    open: handleOpen,
    apiAccess: {
      review,
      busy: current && (starting || reviewing || stopping),
      canEditHosts:
        current && status?.state === "stopped" && !starting && !stopping,
      error: review ? reviewError : null,
      decide: handleApiDecision,
    },
  };
}

export type ProjectPreview = ReturnType<typeof useProjectPreview>;
export type PreviewApiAccess = ProjectPreview["apiAccess"];
