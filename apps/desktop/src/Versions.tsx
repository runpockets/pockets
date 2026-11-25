import { useEffect, useRef, useState } from "react";
import { invoke } from "./api";
import "./Versions.css";

/** How one file differs from the version before it. */
export interface FileChange {
  path: string;
  kind: "added" | "modified" | "removed";
  /** `null` for files that are not text. */
  addedLines: number | null;
  removedLines: number | null;
}

/** One installed version, as native `list_project_versions` reports it. */
export interface VersionSummary {
  revisionId: string;
  installedAtSeconds: number;
  live: boolean;
  /** The first recorded install: nothing earlier to compare with. */
  first: boolean;
  files: number;
  /** Changed files, without the changes file itself. */
  changes: FileChange[];
  /** Lines the coding agent added to POCKETS_CHANGES.md for this version. */
  notes: string[];
  /** The owner's own name for this version; wins over `notes`. */
  note: string | null;
}

export interface VersionsReport {
  /** Newest first. */
  versions: VersionSummary[];
  /** Synced source that is not live yet, compared with the live version. */
  pending: { changes: FileChange[]; notes: string[] } | null;
  sourceControl: "unlinked" | "untracked" | "git";
}

type ReportState =
  | { status: "loading" }
  | { status: "error"; message: string }
  | { status: "ready"; report: VersionsReport };

function errorMessage(err: unknown): string {
  if (typeof err === "string") return err;
  if (err instanceof Error) return err.message;
  return "The request failed for an unknown reason.";
}

function fileName(path: string): string {
  const slash = path.lastIndexOf("/");
  return slash === -1 ? path : path.slice(slash + 1);
}

function nameList(names: string[]): string {
  if (names.length === 1) return names[0];
  if (names.length === 2) return `${names[0]} and ${names[1]}`;
  return `${names[0]}, ${names[1]} and ${names.length - 2} more`;
}

/**
 * A commit-style one-line summary worked out from the changed files, e.g.
 * "Add settings.js, update index.html". File names are shortened to their
 * last segment unless two changed files share one.
 */
export function summarizeChanges(changes: FileChange[]): string {
  if (changes.length === 0) return "No file changes";
  const counts = new Map<string, number>();
  for (const change of changes) {
    const name = fileName(change.path);
    counts.set(name, (counts.get(name) ?? 0) + 1);
  }
  const label = (change: FileChange) => {
    const name = fileName(change.path);
    return (counts.get(name) ?? 0) > 1 ? change.path : name;
  };
  const phrases: string[] = [];
  for (const [kind, verb] of [
    ["added", "add"],
    ["modified", "update"],
    ["removed", "remove"],
  ] as const) {
    const names = changes.filter((change) => change.kind === kind).map(label);
    if (names.length > 0) phrases.push(`${verb} ${nameList(names)}`);
  }
  const sentence = phrases.join(", ");
  return sentence.charAt(0).toUpperCase() + sentence.slice(1);
}

/** The one-line name a version is listed under: the owner's note, else
 *  the agent's notes, else a summary worked out from the changed files. */
export function versionSubject(version: VersionSummary): string {
  if (version.note) return version.note;
  if (version.notes.length === 1) return version.notes[0];
  if (version.notes.length > 1) {
    const more = version.notes.length - 1;
    return `${version.notes[0]} and ${more} more ${more === 1 ? "change" : "changes"}`;
  }
  if (version.first) {
    return `First version · ${version.files} ${version.files === 1 ? "file" : "files"}`;
  }
  return summarizeChanges(version.changes);
}

/** Longest note the native side accepts. */
const MAX_NOTE_CHARS = 200;

/** Total added and removed lines, or `null` when no changed file is text. */
export function lineTotals(
  changes: FileChange[],
): { added: number; removed: number } | null {
  let counted = false;
  let added = 0;
  let removed = 0;
  for (const change of changes) {
    if (change.addedLines === null || change.removedLines === null) continue;
    counted = true;
    added += change.addedLines;
    removed += change.removedLines;
  }
  return counted ? { added, removed } : null;
}

/** "Today, 8:01 pm", "Yesterday, 4:40 pm", "20 Nov, 11:15 am", or with the
 *  year when it is not this year. */
export function formatVersionTime(seconds: number, now = new Date()): string {
  const date = new Date(seconds * 1000);
  if (Number.isNaN(date.getTime())) return "Unknown time";
  const time = date.toLocaleTimeString(undefined, {
    hour: "numeric",
    minute: "2-digit",
  });
  const startOf = (value: Date) =>
    new Date(value.getFullYear(), value.getMonth(), value.getDate()).getTime();
  const days = Math.round((startOf(now) - startOf(date)) / 86_400_000);
  if (days === 0) return `Today, ${time}`;
  if (days === 1) return `Yesterday, ${time}`;
  const day = date.toLocaleDateString(undefined, {
    day: "numeric",
    month: "short",
    ...(date.getFullYear() === now.getFullYear() ? {} : { year: "numeric" }),
  });
  return `${day}, ${time}`;
}

function Lines({ changes }: { changes: FileChange[] }) {
  const totals = lineTotals(changes);
  if (totals === null) return null;
  return (
    <>
      {" · "}
      <span className="versions-added">+{totals.added}</span>{" "}
      <span className="versions-removed">−{totals.removed}</span>
    </>
  );
}

const KIND_MARK: Record<FileChange["kind"], string> = {
  added: "A",
  modified: "M",
  removed: "D",
};

const KIND_WORD: Record<FileChange["kind"], string> = {
  added: "Added",
  modified: "Changed",
  removed: "Removed",
};

function FileList({ changes }: { changes: FileChange[] }) {
  return (
    <ul className="versions-files">
      {changes.map((change) => (
        <li key={change.path}>
          <span className="versions-kind" title={KIND_WORD[change.kind]}>
            <span aria-hidden="true">{KIND_MARK[change.kind]}</span>
            <span className="visually-hidden">{KIND_WORD[change.kind]}</span>
          </span>
          <span className="versions-path">{change.path}</span>
          {change.addedLines !== null && change.removedLines !== null ? (
            <span className="versions-file-lines">
              <span className="versions-added">+{change.addedLines}</span>{" "}
              <span className="versions-removed">−{change.removedLines}</span>
            </span>
          ) : (
            <span className="versions-file-lines">binary</span>
          )}
        </li>
      ))}
    </ul>
  );
}

function Notes({ notes }: { notes: string[] }) {
  return (
    <ul className="versions-notes" aria-label="Notes">
      {notes.map((note, index) => (
        <li key={`${index}-${note}`}>{note}</li>
      ))}
    </ul>
  );
}

export interface VersionsProps {
  projectId: string;
  /** Working source digest; a change refetches the open list. */
  workingDigest: string;
  /** Installed source digest; a change (an Update) refetches the open list. */
  installedDigest: string | null;
  /** When the live version was built, for the collapsed summary line. */
  liveSince?: number | null;
  /** Another operation is busy (build, Update…): no restore may start. */
  disabled?: boolean;
  /** Called after a restore changed the folder, so the owner refetches. */
  onRestored?: () => void;
}

/** Native `restore_project_version` result; the project is refetched by
 *  the owner, so only the commit is read here. */
interface FolderRestore {
  commit: string | null;
}

/** The git commit subject for restoring a version: absolute, never "Today". */
export function restoreSubject(installedAtSeconds: number): string {
  const when = new Date(installedAtSeconds * 1000).toLocaleString(undefined, {
    dateStyle: "medium",
    timeStyle: "short",
  });
  return `Restore the version made live on ${when}`;
}

/**
 * Every version that was made live with Build/Update, newest first, each
 * with a commit-style summary of the files it changed. In a linked folder
 * tracked with git, an earlier version can be restored: native writes its
 * files back into the folder as a commit, and the user makes it live with
 * Update like any other change. The list is fetched only while open.
 */
export default function Versions({
  projectId,
  workingDigest,
  installedDigest,
  liveSince = null,
  disabled = false,
  onRestored,
}: VersionsProps) {
  const [open, setOpen] = useState(false);
  const [state, setState] = useState<ReportState>({ status: "loading" });
  const [retryToken, setRetryToken] = useState(0);
  const [expanded, setExpanded] = useState<string | null>(null);
  const [confirming, setConfirming] = useState<string | null>(null);
  const [restoring, setRestoring] = useState<string | null>(null);
  const [editing, setEditing] = useState<string | null>(null);
  const [draft, setDraft] = useState("");
  const [savingNote, setSavingNote] = useState(false);
  const [noteError, setNoteError] = useState<string | null>(null);
  const [outcome, setOutcome] = useState<{
    tone: "ok" | "error";
    message: string;
  } | null>(null);
  const seqRef = useRef(0);
  const restoreSeqRef = useRef(0);
  const onRestoredRef = useRef(onRestored);
  useEffect(() => {
    onRestoredRef.current = onRestored;
  });

  useEffect(() => {
    restoreSeqRef.current += 1;
    setExpanded(null);
    setEditing(null);
    setConfirming(null);
    setRestoring(null);
    setOutcome(null);
  }, [projectId]);

  useEffect(() => {
    const seq = ++seqRef.current;
    if (!open) return;
    // A refetch after an Update or restore keeps the list on screen.
    setState((current) =>
      current.status === "ready" ? current : { status: "loading" },
    );
    invoke<VersionsReport>("list_project_versions", { id: projectId })
      .then((report) => {
        if (seq === seqRef.current) setState({ status: "ready", report });
      })
      .catch((err: unknown) => {
        if (seq === seqRef.current)
          setState({ status: "error", message: errorMessage(err) });
      });
    return () => {
      if (seq === seqRef.current) seqRef.current += 1;
    };
  }, [open, projectId, workingDigest, installedDigest, retryToken]);

  useEffect(
    () => () => {
      seqRef.current += 1;
      restoreSeqRef.current += 1;
    },
    [],
  );

  function startEditing(version: VersionSummary) {
    setEditing(version.revisionId);
    setDraft(version.note ?? "");
    setNoteError(null);
  }

  async function saveNote(version: VersionSummary) {
    if (savingNote) return;
    setSavingNote(true);
    setNoteError(null);
    try {
      const note = draft.trim();
      await invoke("set_project_version_note", {
        id: projectId,
        revisionId: version.revisionId,
        note: note === "" ? null : note,
      });
      setEditing(null);
      setRetryToken((token) => token + 1);
    } catch (err) {
      setNoteError(errorMessage(err));
    } finally {
      setSavingNote(false);
    }
  }

  async function restore(version: VersionSummary) {
    if (disabled || restoring !== null) return;
    const seq = ++restoreSeqRef.current;
    setRestoring(version.revisionId);
    setOutcome(null);
    try {
      const result = await invoke<FolderRestore>("restore_project_version", {
        id: projectId,
        revisionId: version.revisionId,
        subject: restoreSubject(version.installedAtSeconds),
      });
      if (seq !== restoreSeqRef.current) return;
      setConfirming(null);
      setOutcome({
        tone: "ok",
        message:
          result.commit === null
            ? "Your folder already matches this version."
            : `Restored into your folder and committed as ${result.commit}. Choose Update to make it live.`,
      });
      onRestoredRef.current?.();
    } catch (err) {
      if (seq !== restoreSeqRef.current) return;
      setConfirming(null);
      setOutcome({ tone: "error", message: errorMessage(err) });
    } finally {
      if (seq === restoreSeqRef.current) setRestoring(null);
    }
  }

  const report = state.status === "ready" ? state.report : null;
  const pending = report?.pending ?? null;
  const hasPending =
    pending !== null &&
    (pending.changes.length > 0 || pending.notes.length > 0);
  const canRestore = report?.sourceControl === "git";

  return (
    <details
      className="versions"
      open={open}
      onToggle={(event) => setOpen(event.currentTarget.open)}
    >
      <summary className="section-disclosure">
        Versions
        {typeof liveSince === "number" && (
          <span className="creation-disclosure-meta">
            Live since {formatVersionTime(liveSince)}
          </span>
        )}
      </summary>
      {open && (
        <div className="versions-body">
          <div className="versions-status" aria-live="polite">
            {state.status === "loading" && (
              <p className="muted">Loading versions…</p>
            )}
            {state.status === "error" && (
              <p className="error">
                Could not load versions. {state.message}{" "}
                <button
                  type="button"
                  onClick={() => setRetryToken((token) => token + 1)}
                >
                  Retry
                </button>
              </p>
            )}
            {outcome && (
              <p
                className={outcome.tone === "error" ? "error" : "hint strong"}
                role={outcome.tone === "error" ? "alert" : undefined}
              >
                {outcome.message}
              </p>
            )}
          </div>

          {pending && hasPending && (
            <div className="versions-pending">
              <span className="versions-pending-pip" aria-hidden="true" />
              <div className="versions-pending-text">
                <span>Not live yet</span>
                <span className="versions-meta">
                  {pending.changes.length === 1
                    ? "1 file changed"
                    : `${pending.changes.length} files changed`}
                  <Lines changes={pending.changes} />
                  {" · Update to make it live"}
                </span>
                {pending.notes.length > 0 && <Notes notes={pending.notes} />}
              </div>
            </div>
          )}

          {report && report.versions.length === 0 && (
            <p className="muted">
              No versions yet. Each Update you make live appears here.
            </p>
          )}

          {report && report.versions.length > 0 && (
            <ul className="versions-list" aria-label="Versions">
              {report.versions.map((version) => {
                const isOpen = expanded === version.revisionId;
                const listId = `versions-files-${version.revisionId}`;
                const subject = versionSubject(version);
                // The agent's notes are listed in full when the subject
                // shows only one of them, or when the owner renamed it.
                const listNotes =
                  version.notes.length > 1 ||
                  (version.note !== null && version.notes.length > 0);
                const isEditing = editing === version.revisionId;
                const row = (
                  <>
                    <span className="versions-chevron" aria-hidden="true" />
                    <span className="versions-row-text">
                      <span className="versions-subject">{subject}</span>
                      <span className="versions-meta">
                        {version.live && (
                          <>
                            <span className="versions-live">
                              <span
                                className="versions-live-pip"
                                aria-hidden="true"
                              />
                              Live
                            </span>
                            {" · "}
                          </>
                        )}
                        {formatVersionTime(version.installedAtSeconds)}
                        {version.first ? (
                          (version.note !== null || version.notes.length > 0) &&
                          version.files > 0 && (
                            <>
                              {" · "}
                              {version.files === 1
                                ? "1 file"
                                : `${version.files} files`}
                            </>
                          )
                        ) : (
                          <>
                            {version.changes.length > 0 && (
                              <>
                                {" · "}
                                {version.changes.length === 1
                                  ? "1 file"
                                  : `${version.changes.length} files`}
                              </>
                            )}
                            <Lines changes={version.changes} />
                          </>
                        )}
                      </span>
                    </span>
                  </>
                );
                // The live version is only worth restoring once the folder
                // has moved on from it.
                const restorable = canRestore && (!version.live || hasPending);
                const isConfirming = confirming === version.revisionId;
                return (
                  <li key={version.revisionId} className="versions-item">
                    <div className="versions-item-head">
                      <button
                        type="button"
                        className="versions-row"
                        aria-expanded={isOpen}
                        aria-controls={isOpen ? listId : undefined}
                        onClick={() => {
                          setExpanded(isOpen ? null : version.revisionId);
                          setEditing(null);
                          setConfirming(null);
                        }}
                      >
                        {row}
                      </button>
                    </div>
                    {isOpen && (
                      <div id={listId} className="versions-detail">
                        {listNotes && <Notes notes={version.notes} />}
                        {!version.first && version.changes.length > 0 && (
                          <FileList changes={version.changes} />
                        )}
                        {isEditing ? (
                          <form
                            className="versions-note-form"
                            onSubmit={(event) => {
                              event.preventDefault();
                              void saveNote(version);
                            }}
                          >
                            <input
                              type="text"
                              aria-label="Version note"
                              placeholder="Name this version"
                              value={draft}
                              maxLength={MAX_NOTE_CHARS}
                              disabled={savingNote}
                              autoFocus
                              onChange={(event) => setDraft(event.target.value)}
                              onKeyDown={(event) => {
                                if (event.key === "Escape") {
                                  event.preventDefault();
                                  setEditing(null);
                                }
                              }}
                            />
                            <button
                              type="button"
                              disabled={savingNote}
                              onClick={() => setEditing(null)}
                            >
                              Cancel
                            </button>
                            <button
                              type="submit"
                              className="button-primary"
                              disabled={savingNote}
                            >
                              {savingNote ? "Saving…" : "Save"}
                            </button>
                          </form>
                        ) : (
                          <div className="versions-detail-actions">
                            <button
                              type="button"
                              className="versions-note-edit"
                              onClick={() => startEditing(version)}
                            >
                              {version.note ? "Edit note" : "Add a note"}
                            </button>
                            {restorable && (
                              <button
                                type="button"
                                className="versions-restore"
                                aria-label={`Restore ${subject}`}
                                disabled={disabled || restoring !== null}
                                onClick={() => {
                                  setOutcome(null);
                                  setConfirming(version.revisionId);
                                }}
                              >
                                {restoring === version.revisionId
                                  ? "Restoring…"
                                  : "Restore"}
                              </button>
                            )}
                          </div>
                        )}
                        {restorable && isConfirming && !isEditing && (
                          <div
                            className="versions-confirm"
                            role="group"
                            aria-label="Confirm restore"
                          >
                            <p className="hint">
                              Pockets puts this version&apos;s files back in
                              your folder and commits that as a new change, so
                              your git history keeps everything. Choose Update
                              afterwards to make it live.
                            </p>
                            <div className="versions-confirm-actions">
                              <button
                                type="button"
                                disabled={restoring !== null}
                                onClick={() => setConfirming(null)}
                              >
                                Cancel
                              </button>
                              <button
                                type="button"
                                className="button-primary"
                                disabled={disabled || restoring !== null}
                                onClick={() => void restore(version)}
                              >
                                {restoring === version.revisionId
                                  ? "Restoring…"
                                  : "Restore"}
                              </button>
                            </div>
                          </div>
                        )}
                        {isEditing && noteError && (
                          <p className="error" role="alert">
                            {noteError}
                          </p>
                        )}
                      </div>
                    )}
                  </li>
                );
              })}
            </ul>
          )}

          {report?.sourceControl === "untracked" && (
            <div className="versions-git">
              <svg
                className="versions-git-icon"
                width="16"
                height="16"
                viewBox="0 0 24 24"
                fill="none"
                stroke="currentColor"
                strokeWidth="1.8"
                aria-hidden="true"
              >
                <circle cx="6" cy="6" r="2.2" />
                <circle cx="6" cy="18" r="2.2" />
                <circle cx="18" cy="9" r="2.2" />
                <path d="M6 8.2v7.6M18 11.2c0 3-3 4.2-9.8 5.6" />
              </svg>
              <p className="hint">
                Run <code>git init</code> in this folder and commit your work to
                restore earlier versions here.
              </p>
            </div>
          )}
        </div>
      )}
    </details>
  );
}
