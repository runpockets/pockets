import { useEffect, useRef, useState } from "react";
import { invoke } from "./api";
import { IconCheck, IconCopy, IconFolderOpen } from "./Icons";
import { IconButton, StatusPip } from "./Status";
import "./DevelopmentPanel.css";

interface DevelopmentStatus {
  linked: boolean;
  folder: string | null;
  available: boolean;
  error: string | null;
}

/** Persistent source connection and explicit installation-snapshot capture.
 * Automatic watching belongs to the separate project preview service. */
export default function DevelopmentPanel({
  projectId,
  disabled,
  onSourceChanged,
  initiallyExpanded = false,
}: {
  projectId: string;
  disabled: boolean;
  onSourceChanged: () => void;
  initiallyExpanded?: boolean;
}) {
  const [expanded, setExpanded] = useState(initiallyExpanded);
  const [status, setStatus] = useState<DevelopmentStatus | null>(null);
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const [notice, setNotice] = useState<string | null>(null);
  const [copied, setCopied] = useState(false);
  const copiedTimer = useRef<number | null>(null);
  const epoch = useRef(0);
  const lifetime = useRef(0);

  useEffect(() => {
    setBusy(false);
    setCopied(false);
    if (copiedTimer.current !== null) {
      window.clearTimeout(copiedTimer.current);
      copiedTimer.current = null;
    }
    return () => {
      lifetime.current++;
      if (copiedTimer.current !== null) {
        window.clearTimeout(copiedTimer.current);
        copiedTimer.current = null;
      }
    };
  }, [projectId]);

  useEffect(() => {
    const current = ++epoch.current;
    setStatus(null);
    setError(null);
    setNotice(null);
    if (expanded) {
      void invoke<DevelopmentStatus>("development_status", { id: projectId })
        .then((next) => {
          if (epoch.current === current) setStatus(next);
        })
        .catch((failure: unknown) => {
          if (epoch.current === current) setError(String(failure));
        });
    }
    return () => {
      epoch.current++;
    };
  }, [expanded, projectId]);

  async function act(command: string) {
    const current = epoch.current;
    const owner = lifetime.current;
    setBusy(true);
    setError(null);
    setNotice(null);
    try {
      const result = await invoke(command, { id: projectId });
      if (lifetime.current !== owner) return;
      if (
        command === "unlink_development_folder" ||
        ((command === "link_development_folder" ||
          command === "export_development_project") &&
          result !== null)
      )
        onSourceChanged();
      if (epoch.current !== current) return;
      if (
        (command === "link_development_folder" ||
          command === "export_development_project") &&
        result === null
      )
        return;
      if (command === "copy_development_location") {
        setNotice("Folder path copied.");
        setCopied(true);
        if (copiedTimer.current !== null) {
          window.clearTimeout(copiedTimer.current);
        }
        copiedTimer.current = window.setTimeout(() => {
          copiedTimer.current = null;
          setCopied(false);
        }, 2000);
      }
      const next = await invoke<DevelopmentStatus>("development_status", {
        id: projectId,
      });
      if (epoch.current === current) setStatus(next);
    } catch (failure) {
      if (epoch.current === current) {
        setError(String(failure));
      }
    } finally {
      if (lifetime.current === owner) setBusy(false);
    }
  }

  const blocked = disabled || busy;
  return (
    <details
      open={expanded}
      className="pane-details development-panel"
      onToggle={(event) => setExpanded(event.currentTarget.open)}
    >
      <summary>External development</summary>
      {expanded && (
        <div className="development-content" aria-busy={busy}>
          <p className="hint">
            Edit this Pocket in your preferred coding tool.
          </p>
          {status === null && error === null && (
            <p className="hint">Reading folder connection…</p>
          )}
          {status?.linked ? (
            <>
              <div className="development-folder">
                <code>{status.folder}</code>
                <div className="development-folder-actions">
                  <IconButton
                    label="Open folder"
                    disabled={blocked || !status.available}
                    onClick={() => void act("open_development_folder")}
                  >
                    <IconFolderOpen />
                  </IconButton>
                  <IconButton
                    label="Copy folder path"
                    title={copied ? "Folder path copied" : "Copy folder path"}
                    disabled={blocked || !status.available}
                    onClick={() => void act("copy_development_location")}
                  >
                    {copied ? (
                      <IconCheck className="tone-live" />
                    ) : (
                      <IconCopy />
                    )}
                  </IconButton>
                </div>
              </div>
              <p className="development-connection">
                <StatusPip
                  tone={status.available ? "ok" : "warn"}
                  word={
                    status.available ? "Folder connected" : "Folder unavailable"
                  }
                />
              </p>
              {status.error && <p className="error">{status.error}</p>}
              <p className="hint">
                Start preview to watch live edits. Build and start or Update
                packages this folder automatically.
              </p>
              <div className="actions">
                <button
                  type="button"
                  disabled={blocked}
                  onClick={() => void act("link_development_folder")}
                >
                  Relink folder
                </button>
                <button
                  type="button"
                  disabled={blocked}
                  onClick={() => void act("unlink_development_folder")}
                >
                  Unlink folder
                </button>
              </div>
              <p className="hint">
                Unlinking preserves your source folder, installed app, and saved
                data.
              </p>
            </>
          ) : status !== null ? (
            <>
              <p>
                Export this Pocket’s saved source to a new folder, or link an
                existing copy.
              </p>
              <div className="actions">
                <button
                  type="button"
                  className="button-primary"
                  disabled={blocked}
                  onClick={() => void act("export_development_project")}
                >
                  Export saved source…
                </button>
                <button
                  type="button"
                  disabled={blocked}
                  onClick={() => void act("link_development_folder")}
                >
                  Link development folder
                </button>
              </div>
            </>
          ) : null}
          <div aria-live="polite">
            {busy && <p className="hint">Working…</p>}
            {notice && <p className="hint">{notice}</p>}
            {error && <p className="error">{error}</p>}
          </div>
        </div>
      )}
    </details>
  );
}
