import { useEffect, useState } from "react";
import { invoke } from "./api";
import {
  IconCheck,
  IconClose,
  IconCopy,
  IconExternalLink,
  IconPocketsMark,
} from "./Icons";
import { Spinner } from "./Status";
import "./AppUpdateModal.css";

export interface AppUpdateStatus {
  currentVersion: string;
  latestVersion: string | null;
  updateAvailable: boolean;
  releaseUrl: string | null;
  notes: string[];
  publishedAt: string | null;
  installMethod: "homebrew" | "binary" | "development" | string;
  canAutoUpdate: boolean;
  manualCommand: string;
  error: string | null;
}

interface ApplyUpdateResult {
  updatedTo: string;
  restarting: boolean;
  message: string;
}

interface AppUpdateModalProps {
  open: boolean;
  status: AppUpdateStatus | null;
  checking: boolean;
  onClose: () => void;
  onRecheck: () => void;
}

type Phase = "idle" | "updating" | "restarting" | "restart-manual";

export default function AppUpdateModal({
  open,
  status,
  checking,
  onClose,
  onRecheck,
}: AppUpdateModalProps) {
  const [phase, setPhase] = useState<Phase>("idle");
  const [updatedTo, setUpdatedTo] = useState<string | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [copied, setCopied] = useState(false);

  const busy = phase === "updating" || phase === "restarting";
  const installedPendingReload = phase === "restart-manual";

  useEffect(() => {
    if (!open) {
      setPhase("idle");
      setUpdatedTo(null);
      setError(null);
      setCopied(false);
    }
  }, [open]);

  useEffect(() => {
    if (!open) return;
    const onKeyDown = (event: KeyboardEvent) => {
      if (event.key === "Escape" && !busy) {
        onClose();
      }
    };
    window.addEventListener("keydown", onKeyDown);
    return () => window.removeEventListener("keydown", onKeyDown);
  }, [open, busy, onClose]);

  useEffect(() => {
    const targetVersion = (updatedTo ?? status?.latestVersion ?? "").replace(
      /^v/,
      "",
    );
    if (
      (phase !== "restarting" && phase !== "restart-manual") ||
      !targetVersion
    ) {
      return;
    }
    let active = true;
    let attempts = 0;
    let timerId: number | null = null;
    const maxFastAttempts = 15;
    const intervalMs = phase === "restarting" ? 1000 : 3000;

    const poll = async () => {
      if (!active) return;
      attempts += 1;
      try {
        const res = await fetch("/api/health", { cache: "no-store" });
        if (res.ok) {
          const data = (await res.json()) as { version?: string };
          if (data.version?.replace(/^v/, "") === targetVersion) {
            window.location.reload();
            return;
          }
        }
      } catch {
        // Expected while the daemon restarts.
      }
      if (!active) return;
      if (phase === "restarting" && attempts >= maxFastAttempts) {
        setPhase("restart-manual");
        return;
      }
      timerId = window.setTimeout(() => void poll(), intervalMs);
    };

    timerId = window.setTimeout(() => void poll(), intervalMs);
    return () => {
      active = false;
      if (timerId !== null) window.clearTimeout(timerId);
    };
  }, [phase, updatedTo, status?.latestVersion]);

  if (!open || status === null) return null;

  async function handleUpdateNow() {
    if (busy || installedPendingReload) return;
    setPhase("updating");
    setError(null);
    try {
      const result = await invoke<ApplyUpdateResult>("apply_app_update");
      setUpdatedTo(result.updatedTo);
      if (result.restarting) {
        setPhase("restarting");
      } else {
        window.location.reload();
      }
    } catch (failure) {
      setPhase("idle");
      setError(failure instanceof Error ? failure.message : String(failure));
    }
  }

  async function handleCopyCommand() {
    if (!status?.manualCommand) return;
    try {
      await navigator.clipboard.writeText(status.manualCommand);
      setCopied(true);
      window.setTimeout(() => setCopied(false), 2000);
    } catch {
      // Clipboard may be unavailable in restricted contexts; command stays selectable.
    }
  }

  const hasUpdate = status.updateAvailable && status.latestVersion !== null;
  const targetDisplay = updatedTo ?? status.latestVersion ?? status.currentVersion;
  const heading = installedPendingReload
    ? "Update installed"
    : hasUpdate
      ? "Pockets update available"
      : "Pockets is up to date";
  const subtitle = installedPendingReload
    ? `Version v${targetDisplay} has been installed.`
    : hasUpdate
      ? `Version v${status.latestVersion} is ready to install.`
      : `You're running v${status.currentVersion}.`;

  return (
    <div
      className="app-update-backdrop"
      role="presentation"
      onClick={(event) => {
        if (event.target === event.currentTarget && !busy) {
          onClose();
        }
      }}
    >
      <div
        className="app-update-dialog"
        role="dialog"
        aria-modal="true"
        aria-labelledby="app-update-title"
      >
        <button
          type="button"
          className="app-update-close"
          aria-label="Close update dialog"
          disabled={busy}
          onClick={onClose}
        >
          <IconClose size={16} />
        </button>

        <div className="app-update-hero">
          <div className="app-update-badge" aria-hidden="true">
            <IconPocketsMark size={32} />
          </div>
          <h2 id="app-update-title" className="app-update-title">
            {heading}
          </h2>
          <p className="app-update-subtitle">{subtitle}</p>
          {hasUpdate && !installedPendingReload && (
            <p className="app-update-current">
              Installed: <code>{`v${status.currentVersion}`}</code>
            </p>
          )}
        </div>

        {hasUpdate && status.notes.length > 0 && (
          <div className="app-update-notes">
            <div className="app-update-notes-heading">In this update</div>
            <ul className="app-update-notes-list">
              {status.notes.map((note, index) => (
                <li key={index}>{note}</li>
              ))}
            </ul>
            {status.releaseUrl && (
              <a
                className="app-update-release-link"
                href={status.releaseUrl}
                target="_blank"
                rel="noopener noreferrer"
              >
                <IconExternalLink size={13} />
                <span>Release notes</span>
              </a>
            )}
          </div>
        )}

        {status.error && (
          <p role="alert" className="app-update-error">
            {status.error}
          </p>
        )}

        {error && (
          <p role="alert" className="app-update-error">
            {error}
          </p>
        )}

        {phase === "restart-manual" && (
          <p className="app-update-info">
            Update installed! Restart <code>pockets</code> if this page does not
            reconnect automatically, then reload.
          </p>
        )}

        {hasUpdate && (!status.canAutoUpdate || error !== null) && (
          <div className="app-update-command-box">
            <div className="app-update-command-label">
              {status.installMethod === "development"
                ? "Running a local development build:"
                : "Or update manually in your terminal:"}
            </div>
            <div className="app-update-command-row">
              <code className="app-update-command-code">
                {status.manualCommand}
              </code>
              <button
                type="button"
                className="app-update-copy"
                aria-label="Copy update command"
                onClick={() => void handleCopyCommand()}
              >
                {copied ? <IconCheck size={14} /> : <IconCopy size={14} />}
                <span>{copied ? "Copied" : "Copy"}</span>
              </button>
            </div>
          </div>
        )}

        <div className="app-update-actions">
          {installedPendingReload ? (
            <>
              <button
                type="button"
                className="app-update-primary"
                onClick={() => window.location.reload()}
              >
                Reload page
              </button>
              <button
                type="button"
                className="app-update-secondary"
                onClick={onClose}
              >
                Close
              </button>
            </>
          ) : hasUpdate ? (
            <>
              {status.canAutoUpdate && (
                <button
                  type="button"
                  className="app-update-primary"
                  disabled={busy}
                  onClick={() => void handleUpdateNow()}
                >
                  {phase === "updating" ? (
                    <>
                      <Spinner />
                      <span>{`Updating to v${status.latestVersion}…`}</span>
                    </>
                  ) : phase === "restarting" ? (
                    <>
                      <Spinner />
                      <span>Restarting Pockets…</span>
                    </>
                  ) : (
                    "Update now"
                  )}
                </button>
              )}
              <button
                type="button"
                className="app-update-secondary"
                disabled={busy}
                onClick={onClose}
              >
                Maybe later
              </button>
            </>
          ) : (
            <>
              <button
                type="button"
                className="app-update-primary"
                disabled={checking}
                onClick={onRecheck}
              >
                {checking ? (
                  <>
                    <Spinner />
                    <span>Checking…</span>
                  </>
                ) : (
                  "Check again"
                )}
              </button>
              {status.releaseUrl && (
                <a
                  className="app-update-release-link app-update-release-link-centered"
                  href={status.releaseUrl}
                  target="_blank"
                  rel="noopener noreferrer"
                >
                  <IconExternalLink size={13} />
                  <span>View releases on GitHub</span>
                </a>
              )}
              <button
                type="button"
                className="app-update-secondary"
                onClick={onClose}
              >
                Close
              </button>
            </>
          )}
        </div>
      </div>
    </div>
  );
}
