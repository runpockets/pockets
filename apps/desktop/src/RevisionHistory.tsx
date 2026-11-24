import { useCallback, useEffect, useRef, useState } from "react";
import { invoke } from "./api";
import { formatBytes } from "./formatBytes";
import "./RevisionHistory.css";

/**
 * One saved source revision returned by the native `list_project_revisions`
 * command, newest first — the native order is displayed as-is and never
 * re-sorted. Every field is rendered as escaped text; nothing here is ever
 * interpreted as markup, and only `id` values are sent back to native.
 */
export interface ProjectRevision {
  id: string;
  /** Real wall-clock seconds (epoch) recorded by native when the snapshot landed. */
  createdAtSeconds: number;
  label: string;
  sourceDigest: string;
  totalBytes: number;
  files: number;
}

export interface RevisionHistoryProps {
  /** Project whose history is listed and restored; passed to native verbatim. */
  projectId: string;
  /** Digest of the current working source. The revision matching it is the
   *  current one: badged `working`, with its Restore button disabled. */
  workingDigest: string;
  /** Digest of the source the installed app was built from, when it exists.
   *  The revision matching it is badged `installed`. */
  installedDigest?: string | null;
  /** Called after a restore resolves successfully, so the owner can refetch
   *  the project (which changes `workingDigest` and refetches this list). */
  onRestored: () => void;
  /** Another operation is busy (chat turn, build, Update…): blocks opening a
   *  confirmation and any native write while true. Reads stay available. */
  disabled?: boolean;
  linked?: boolean;
}

/** Rejection-free result of `restore_project_revision`: a resolve is always a
 *  successful restore; `previewWarning` only qualifies the live preview, it
 *  never means the restore itself failed. */
interface RestoreResult {
  project: unknown;
  previewWarning?: string | null;
}

/** Panel-local view state for the history list. */
type HistoryState =
  | { status: "loading" }
  | { status: "error"; message: string }
  | { status: "ready"; revisions: ProjectRevision[] };

function errorMessage(err: unknown): string {
  if (typeof err === "string") return err;
  if (err instanceof Error) return err.message;
  return "The request failed for an unknown reason.";
}

/** Renders the real native timestamp. Native currently requires an actual
 *  timestamp on every revision, so this is a defensive path only: a missing
 *  one is stated as unknown, never silently shown as "now". */
function formatTimestamp(seconds: number): string {
  if (!Number.isFinite(seconds)) return "unknown time";
  const date = new Date(seconds * 1000);
  if (Number.isNaN(date.getTime())) return "unknown time";
  return date.toLocaleString();
}

/** Digest matching: only two non-empty digests can ever match. */
function digestEquals(a: string | null | undefined, b: string): boolean {
  return typeof a === "string" && a !== "" && b !== "" && a === b;
}

/**
 * Compact per-Pocket source history: every source-changing step (chat edit,
 * import) persists a restorable revision natively, and this panel lists them
 * newest first with their real timestamp, label and snapshot size.
 *
 * Restore semantics (explicit product decision): restoring a revision changes
 * the **working source and preview only**. The installed app and the Pocket's
 * saved data stay untouched until the user runs Update. The panel therefore
 * only ever invokes `list_project_revisions` (read) and
 * `restore_project_revision` (working-source write), passing nothing but the
 * opaque project and revision ids it was given.
 *
 * The list is fetched lazily — only while the `<details>` is open — and
 * refetched when it is open and `workingDigest`/`installedDigest` change, so
 * a chat save or Update lands without remounting and without any polling.
 * `onRestored` is held in a ref so an unstable callback identity can never
 * retrigger the fetch. Late list and restore responses are dropped once their
 * run has been superseded (close, project switch or unmount).
 */
export default function RevisionHistory({
  projectId,
  workingDigest,
  installedDigest = null,
  onRestored,
  disabled = false,
  linked = false,
}: RevisionHistoryProps) {
  // The <details> is controlled so the list body — and its fetch — only
  // exist while open.
  const [open, setOpen] = useState(false);
  const [history, setHistory] = useState<HistoryState>({
    status: "loading",
  });
  const [retryToken, setRetryToken] = useState(0);
  // Inline confirmation (never window.confirm): at most one revision is
  // awaiting confirmation, and one restore may be in flight at a time.
  const [confirmingId, setConfirmingId] = useState<string | null>(null);
  const [restoringId, setRestoringId] = useState<string | null>(null);
  const [restoreError, setRestoreError] = useState<string | null>(null);
  // Settled restore outcome, kept until the next attempt or a project switch
  // so the "Update to apply" handoff stays readable while the list refetches.
  const [restoredNotice, setRestoredNotice] = useState<{
    warning: string | null;
  } | null>(null);

  // Monotonic run tokens: superseding a run (reopen, project switch, unmount,
  // explicit retry) invalidates its in-flight response so it can never land
  // on a closed panel, a different project, or after unmount.
  const listSeqRef = useRef(0);
  const restoreSeqRef = useRef(0);
  const projectIdRef = useRef(projectId);
  // Callbacks live outside the fetch dependency set (CreationPanel pattern):
  // an unstable identity must not cause a refetch loop.
  const onRestoredRef = useRef(onRestored);
  useEffect(() => {
    onRestoredRef.current = onRestored;
  });

  useEffect(() => {
    if (projectIdRef.current !== projectId) {
      // Project switch: nothing in flight for the old project may land, and
      // its outcome notices are not this project's story.
      projectIdRef.current = projectId;
      restoreSeqRef.current += 1;
      setConfirmingId(null);
      setRestoringId(null);
      setRestoreError(null);
      setRestoredNotice(null);
    }
    const seq = ++listSeqRef.current;
    if (!open) return;
    setHistory({ status: "loading" });
    invoke<ProjectRevision[]>("list_project_revisions", { id: projectId })
      .then((revisions) => {
        if (seq === listSeqRef.current)
          setHistory({ status: "ready", revisions });
      })
      .catch((err: unknown) => {
        if (seq === listSeqRef.current)
          setHistory({ status: "error", message: errorMessage(err) });
      });
    return () => {
      // Superseded (closed, switched, deps changed) or unmounted: the next
      // run's token is already ahead, so this run's response lands nowhere.
      if (seq === listSeqRef.current) listSeqRef.current += 1;
    };
  }, [open, projectId, workingDigest, installedDigest, retryToken]);

  // Final unmount guard, including the runs that returned early above.
  useEffect(
    () => () => {
      listSeqRef.current += 1;
      restoreSeqRef.current += 1;
    },
    [],
  );

  const revisions = history.status === "ready" ? history.revisions : [];

  /** Opens the inline confirmation. Invokes nothing; the current working
   *  revision and a busy panel never even get here. */
  const handleAskRestore = useCallback(
    (revision: ProjectRevision) => {
      if (disabled || linked || restoringId !== null) return;
      if (digestEquals(revision.sourceDigest, workingDigest)) return;
      setRestoreError(null);
      setRestoredNotice(null);
      setConfirmingId(revision.id);
    },
    [disabled, linked, restoringId, workingDigest],
  );

  /** Closes the confirmation — but never mid-restore, when native has
   *  already changed the working source and the outcome must be seen. */
  const handleCancelRestore = useCallback(() => {
    if (restoringId !== null) return;
    setConfirmingId(null);
  }, [restoringId]);

  /**
   * Performs the confirmed restore. Only the opaque ids cross to native; a
   * resolve is always a successful restore of the working source. The
   * optional `previewWarning` qualifies the live preview, never the restore,
   * so it is shown alongside the success line — never as a failure.
   */
  const handleConfirmRestore = useCallback(async () => {
    if (disabled || linked || restoringId !== null) return;
    const revision = revisions.find((entry) => entry.id === confirmingId);
    // No revision, or it became the current working source meanwhile: never
    // invoke.
    if (!revision || digestEquals(revision.sourceDigest, workingDigest)) {
      setConfirmingId(null);
      return;
    }
    const seq = ++restoreSeqRef.current;
    setRestoringId(revision.id);
    setRestoreError(null);
    try {
      const result = await invoke<RestoreResult>("restore_project_revision", {
        id: projectId,
        revisionId: revision.id,
      });
      if (seq !== restoreSeqRef.current) return;
      setConfirmingId(null);
      setRestoredNotice({ warning: result?.previewWarning ?? null });
      onRestoredRef.current();
    } catch (err) {
      if (seq !== restoreSeqRef.current) return;
      setConfirmingId(null);
      setRestoreError(errorMessage(err));
    } finally {
      if (seq === restoreSeqRef.current) setRestoringId(null);
    }
  }, [
    confirmingId,
    disabled,
    linked,
    projectId,
    restoringId,
    revisions,
    workingDigest,
  ]);

  return (
    <details
      className="revision-history"
      open={open}
      onToggle={(event) => setOpen(event.currentTarget.open)}
    >
      <summary className="section-disclosure">Source history</summary>
      {open && (
        <div className="revision-history-body">
          {linked && (
            <p className="hint">
              Restore source in your coding tool, then choose Update. Unlink the
              folder to restore a saved Pockets snapshot.
            </p>
          )}
          {restoredNotice && (
            <div className="revision-notice" role="status">
              <p className="revision-notice-line">
                Source restored. Update to apply it to the installed app.
              </p>
              {restoredNotice.warning !== null &&
                restoredNotice.warning !== "" && (
                  <p className="revision-notice-warning">
                    Preview note: {restoredNotice.warning}
                  </p>
                )}
            </div>
          )}
          {restoreError && (
            <p className="error">The restore failed. {restoreError}</p>
          )}

          <div className="revision-history-status" aria-live="polite">
            {history.status === "loading" && (
              <p className="muted">Loading source history…</p>
            )}
            {history.status === "error" && (
              <p className="error">
                Could not load source history. {history.message}{" "}
                <button
                  type="button"
                  onClick={() => setRetryToken((token) => token + 1)}
                >
                  Retry
                </button>
              </p>
            )}
          </div>

          {history.status === "ready" && revisions.length === 0 && (
            <p className="muted">
              No source history yet. Synced and built snapshots appear here.
            </p>
          )}

          {history.status === "ready" && revisions.length > 0 && (
            <ul className="revision-list">
              {revisions.map((revision) => {
                const isCurrent = digestEquals(
                  revision.sourceDigest,
                  workingDigest,
                );
                const isInstalled = digestEquals(
                  revision.sourceDigest,
                  installedDigest ?? "",
                );
                const rowBusy = restoringId !== null;
                return (
                  <li key={revision.id} className="revision-item">
                    <div className="revision-head">
                      {/* Native-owned label: text child, never markup. */}
                      <span className="revision-label">{revision.label}</span>
                      {isCurrent && (
                        <span className="badge revision-badge revision-badge-working">
                          working
                        </span>
                      )}
                      {isInstalled && (
                        <span className="badge revision-badge revision-badge-installed">
                          installed
                        </span>
                      )}
                      {!linked && (
                        <button
                          type="button"
                          className="revision-restore"
                          disabled={isCurrent || disabled || rowBusy}
                          onClick={() => handleAskRestore(revision)}
                        >
                          {restoringId === revision.id
                            ? "Restoring…"
                            : "Restore"}
                        </button>
                      )}
                    </div>
                    <p className="revision-meta">
                      {formatTimestamp(revision.createdAtSeconds)} ·{" "}
                      {revision.files} files ·{" "}
                      {formatBytes(revision.totalBytes)}
                    </p>

                    {!linked && confirmingId === revision.id && !isCurrent && (
                      <div
                        className="revision-confirm"
                        role="group"
                        aria-label="Confirm restore"
                      >
                        <p className="revision-confirm-text">
                          Restoring changes the working source and preview. The
                          installed app and saved data stay unchanged until
                          Update.
                        </p>
                        <div className="revision-confirm-actions">
                          <button
                            type="button"
                            onClick={handleCancelRestore}
                            disabled={rowBusy}
                          >
                            Cancel
                          </button>
                          <button
                            type="button"
                            onClick={() => void handleConfirmRestore()}
                            disabled={disabled || rowBusy}
                          >
                            {restoringId === revision.id
                              ? "Restoring…"
                              : "Confirm restore"}
                          </button>
                        </div>
                      </div>
                    )}
                  </li>
                );
              })}
            </ul>
          )}
        </div>
      )}
    </details>
  );
}
