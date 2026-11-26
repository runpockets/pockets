import { useCallback, useEffect, useRef, useState } from "react";
import { invoke } from "./api";
import { formatVersionTag } from "./formatVersion";
import "./UpstreamUpdates.css";

export type UpstreamState =
  "unlinked" | "noRemote" | "upToDate" | "updateAvailable" | "diverged";

export interface UpstreamStatus {
  state: UpstreamState;
  repo?: string | null;
  repoUrl?: string | null;
  branch?: string | null;
  upstreamRef?: string | null;
  localCommit?: string | null;
  upstreamCommit?: string | null;
  ahead: number;
  behind: number;
  localDirty: boolean;
  currentVersion?: string | null;
  upstreamVersion?: string | null;
  notes?: string[];
  fetchError?: string | null;
}

export interface UpstreamPullResult {
  previousCommit: string;
  currentCommit: string;
  backupBranch?: string | null;
  updated: boolean;
}

export interface UpstreamUpdatesProps {
  projectId: string;
  built: boolean;
  workingDigest: string;
  disabled?: boolean;
  onPulled?: () => void;
}

function errorMessage(err: unknown): string {
  if (typeof err === "string") return err;
  if (err instanceof Error) return err.message;
  return "The request failed for an unknown reason.";
}

function normalizeBareVersion(version: string | null | undefined): string {
  if (!version) return "";
  const trimmed = version.trim();
  const bare = trimmed.replace(/^[vV]+/, "");
  return bare || trimmed;
}

export function formatUpstreamDelta(status: UpstreamStatus): string {
  const cur = normalizeBareVersion(status.currentVersion);
  const next = normalizeBareVersion(status.upstreamVersion);
  if (cur && next && cur !== next) {
    return `${formatVersionTag(cur)} → ${formatVersionTag(next)}`;
  }
  const n = status.behind;
  return n === 1 ? "1 new commit" : `${n} new commits`;
}

export function buildAgentMergePrompt(status: UpstreamStatus): string {
  const target = status.upstreamRef ?? "origin/main";
  return `Merge ${target} into the current branch while preserving my local customizations, resolve any conflicts carefully, and run the tests.`;
}

/**
 * Upstream Git update checker and safe puller for Pockets linked to a repo
 * with an `origin` remote (such as Pockets pulled from Pockets Hub).
 * Renders nothing for personal Pockets with no Git remote.
 */
export default function UpstreamUpdates({
  projectId,
  built,
  workingDigest,
  disabled = false,
  onPulled,
}: UpstreamUpdatesProps) {
  const [status, setStatus] = useState<UpstreamStatus | null>(null);
  const [open, setOpen] = useState(false);
  const [checking, setChecking] = useState(false);
  const [pulling, setPulling] = useState(false);
  const [confirmReplace, setConfirmReplace] = useState(false);
  const [copiedPrompt, setCopiedPrompt] = useState(false);
  const [outcome, setOutcome] = useState<{
    tone: "ok" | "error";
    message: string;
  } | null>(null);

  const seqRef = useRef(0);
  const autoOpenedProject = useRef<string | null>(null);
  const copiedTimer = useRef<number | null>(null);
  const onPulledRef = useRef(onPulled);
  useEffect(() => {
    onPulledRef.current = onPulled;
  });

  useEffect(() => {
    setStatus(null);
    setOpen(false);
    setChecking(false);
    setPulling(false);
    setConfirmReplace(false);
    setCopiedPrompt(false);
    setOutcome(null);
    autoOpenedProject.current = null;
    if (copiedTimer.current !== null) {
      window.clearTimeout(copiedTimer.current);
      copiedTimer.current = null;
    }
  }, [projectId]);

  useEffect(
    () => () => {
      seqRef.current += 1;
      if (copiedTimer.current !== null) {
        window.clearTimeout(copiedTimer.current);
      }
    },
    [],
  );

  const checkUpstream = useCallback(
    async (force: boolean) => {
      const seq = ++seqRef.current;
      if (force) {
        setChecking(true);
        setOutcome(null);
      }
      try {
        const next = await invoke<UpstreamStatus>("check_project_upstream", {
          id: projectId,
          force,
        });
        if (seq !== seqRef.current) return;
        setStatus(next);
        if (
          (next.state === "updateAvailable" || next.state === "diverged") &&
          autoOpenedProject.current !== projectId
        ) {
          autoOpenedProject.current = projectId;
          setOpen(true);
        }
      } catch (err) {
        if (seq !== seqRef.current) return;
        if (force) {
          setOutcome({ tone: "error", message: errorMessage(err) });
        }
      } finally {
        if (seq === seqRef.current && force) {
          setChecking(false);
        }
      }
    },
    [projectId],
  );

  useEffect(() => {
    void checkUpstream(false);
  }, [checkUpstream, workingDigest]);

  async function pull(mode: "fastForward" | "replaceWithUpstream") {
    if (disabled || pulling || checking) return;
    const seq = ++seqRef.current;
    setPulling(true);
    setOutcome(null);
    try {
      const result = await invoke<UpstreamPullResult>("pull_project_upstream", {
        id: projectId,
        mode,
      });
      if (seq !== seqRef.current) return;
      setConfirmReplace(false);
      const nextStep = built
        ? "Choose Update above to make it live."
        : "Choose Build and start above to install it.";
      if (!result.updated) {
        setOutcome({ tone: "ok", message: "Already up to date." });
      } else if (result.backupBranch) {
        setOutcome({
          tone: "ok",
          message: `Replaced with upstream (${result.currentCommit}). Your local commits were saved on branch ${result.backupBranch}. ${nextStep}`,
        });
      } else {
        setOutcome({
          tone: "ok",
          message: `Pulled latest changes (${result.currentCommit}). ${nextStep}`,
        });
      }
      onPulledRef.current?.();
      const refreshed = await invoke<UpstreamStatus>("check_project_upstream", {
        id: projectId,
        force: false,
      }).catch(() => null);
      if (seq === seqRef.current && refreshed) {
        setStatus(refreshed);
      }
    } catch (err) {
      if (seq !== seqRef.current) return;
      setConfirmReplace(false);
      setOutcome({ tone: "error", message: errorMessage(err) });
    } finally {
      if (seq === seqRef.current) setPulling(false);
    }
  }

  async function copyMergePrompt() {
    if (!status) return;
    try {
      await navigator.clipboard.writeText(buildAgentMergePrompt(status));
      setCopiedPrompt(true);
      if (copiedTimer.current !== null) {
        window.clearTimeout(copiedTimer.current);
      }
      copiedTimer.current = window.setTimeout(() => {
        copiedTimer.current = null;
        setCopiedPrompt(false);
      }, 2000);
    } catch {
      setOutcome({
        tone: "error",
        message: "Could not copy to clipboard.",
      });
    }
  }

  if (!status || status.state === "unlinked" || status.state === "noRemote") {
    return null;
  }

  const hasUpdate =
    status.state === "updateAvailable" || status.state === "diverged";
  const sourceLabel = status.repo ?? status.upstreamRef ?? "origin";
  const notes = status.notes ?? [];
  const busy = disabled || pulling || checking;

  let summaryMeta: string;
  if (status.state === "updateAvailable") {
    summaryMeta = formatUpstreamDelta(status);
  } else if (status.state === "diverged") {
    summaryMeta = `${formatUpstreamDelta(status)} · local changes`;
  } else if (status.ahead > 0) {
    summaryMeta = `Up to date · ${status.ahead} local`;
  } else {
    summaryMeta = `${sourceLabel} · Up to date`;
  }

  let divergedSummary = "";
  if (status.state === "diverged") {
    const commitWord = status.ahead === 1 ? "commit" : "commits";
    if (status.localDirty && status.ahead > 0) {
      divergedSummary = `Upstream has updates (${formatUpstreamDelta(status)}), and your folder has uncommitted edits and ${status.ahead} local ${commitWord}.`;
    } else if (status.localDirty) {
      divergedSummary = `Upstream has updates (${formatUpstreamDelta(status)}), and your folder has uncommitted edits.`;
    } else {
      divergedSummary = `Upstream has updates (${formatUpstreamDelta(status)}), and you have ${status.ahead} local ${commitWord} of your own.`;
    }
  }

  return (
    <details
      className="upstream"
      open={open}
      onToggle={(event) => setOpen(event.currentTarget.open)}
    >
      <summary className="section-disclosure">
        Updates
        <span className="creation-disclosure-meta upstream-disclosure-meta">
          {hasUpdate && <span className="upstream-pip" aria-hidden="true" />}
          {summaryMeta}
        </span>
      </summary>
      {open && (
        <div className="upstream-body" aria-busy={pulling || checking}>
          <div className="upstream-source">
            {status.repoUrl ? (
              <a
                href={status.repoUrl}
                target="_blank"
                rel="noopener noreferrer"
                className="upstream-repo-link"
              >
                {sourceLabel}
              </a>
            ) : (
              <span>{sourceLabel}</span>
            )}
            {status.localCommit && (
              <>
                {" · "}
                <code>{status.localCommit}</code>
              </>
            )}
            {status.currentVersion && (
              <>
                {" · "}
                <span>{formatVersionTag(status.currentVersion)}</span>
              </>
            )}
          </div>

          {status.fetchError && (
            <p className="hint">
              Could not reach remote ({status.fetchError}). Showing last fetched
              state.
            </p>
          )}

          {outcome && (
            <p
              className={outcome.tone === "error" ? "error" : "hint strong"}
              role={outcome.tone === "error" ? "alert" : "status"}
            >
              {outcome.message}
            </p>
          )}

          {status.state === "updateAvailable" && (
            <>
              <p className="upstream-summary">
                New upstream changes available ({formatUpstreamDelta(status)}).
                Pulling updates your folder without restarting the live app.
              </p>
              {notes.length > 0 && (
                <ul className="upstream-notes" aria-label="Incoming changes">
                  {notes.map((note, index) => (
                    <li key={`${index}-${note}`}>{note}</li>
                  ))}
                </ul>
              )}
              <div className="upstream-actions">
                <button
                  type="button"
                  className="button-primary"
                  disabled={busy}
                  onClick={() => void pull("fastForward")}
                >
                  {pulling ? "Pulling…" : "Pull update"}
                </button>
                <button
                  type="button"
                  disabled={busy}
                  onClick={() => void checkUpstream(true)}
                >
                  {checking ? "Checking…" : "Check again"}
                </button>
              </div>
            </>
          )}

          {status.state === "diverged" && (
            <>
              <p className="upstream-summary">{divergedSummary}</p>
              {notes.length > 0 && (
                <ul className="upstream-notes" aria-label="Incoming changes">
                  {notes.map((note, index) => (
                    <li key={`${index}-${note}`}>{note}</li>
                  ))}
                </ul>
              )}
              <p className="hint">
                Pockets won&apos;t overwrite your customizations automatically.
                Open this folder in your coding tool to merge{" "}
                <code>{status.upstreamRef ?? "origin/main"}</code> while keeping
                your edits.
              </p>
              <div className="upstream-actions">
                <button
                  type="button"
                  disabled={busy}
                  onClick={() => void copyMergePrompt()}
                >
                  {copiedPrompt ? "Prompt copied" : "Copy merge prompt"}
                </button>
                {!status.localDirty && (
                  <button
                    type="button"
                    disabled={busy}
                    onClick={() => {
                      setOutcome(null);
                      setConfirmReplace(true);
                    }}
                  >
                    Replace with upstream…
                  </button>
                )}
                <button
                  type="button"
                  disabled={busy}
                  onClick={() => void checkUpstream(true)}
                >
                  {checking ? "Checking…" : "Check again"}
                </button>
              </div>
              {status.localDirty && (
                <p className="hint">
                  Commit or stash your uncommitted edits first if you want to
                  replace your folder with upstream.
                </p>
              )}
              {confirmReplace && !status.localDirty && (
                <div
                  className="upstream-confirm"
                  role="group"
                  aria-label="Confirm replace with upstream"
                >
                  <p className="hint">
                    Pockets will save your local commits to a backup git branch
                    (<code>pockets-backup-{status.localCommit ?? "local"}</code>
                    ) and reset the folder to{" "}
                    <code>{status.upstreamRef ?? "origin/main"}</code>.{" "}
                    {built
                      ? "Choose Update afterwards to make it live."
                      : "Choose Build and start afterwards to install it."}
                  </p>
                  <div className="upstream-confirm-actions">
                    <button
                      type="button"
                      disabled={pulling}
                      onClick={() => setConfirmReplace(false)}
                    >
                      Cancel
                    </button>
                    <button
                      type="button"
                      className="button-primary"
                      disabled={busy}
                      onClick={() => void pull("replaceWithUpstream")}
                    >
                      {pulling ? "Replacing…" : "Replace with upstream"}
                    </button>
                  </div>
                </div>
              )}
            </>
          )}

          {status.state === "upToDate" && (
            <>
              <p className="hint">
                {status.ahead > 0
                  ? `Up to date with ${status.upstreamRef ?? "origin"} (${status.ahead} local ${status.ahead === 1 ? "commit" : "commits"} ahead).`
                  : `Up to date with ${status.upstreamRef ?? "origin"}.`}
              </p>
              <div className="upstream-actions">
                <button
                  type="button"
                  disabled={busy}
                  onClick={() => void checkUpstream(true)}
                >
                  {checking ? "Checking…" : "Check for updates"}
                </button>
              </div>
            </>
          )}
        </div>
      )}
    </details>
  );
}
