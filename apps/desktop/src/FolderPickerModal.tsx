import { useCallback, useEffect, useState } from "react";
import {
  invoke,
  registerFolderPicker,
  type DirectoryListing,
  type FolderPickRequest,
} from "./api";
import "./FolderPickerModal.css";

interface ActivePick {
  request: FolderPickRequest;
  resolve: (path: string | null) => void;
}

export default function FolderPickerModal() {
  const [active, setActive] = useState<ActivePick | null>(null);
  const [listing, setListing] = useState<DirectoryListing | null>(null);
  const [pathInput, setPathInput] = useState("");
  const [showHidden, setShowHidden] = useState(false);
  const [loading, setLoading] = useState(false);
  const [error, setError] = useState<string | null>(null);

  const loadPath = useCallback(async (target?: string, hidden = false) => {
    setLoading(true);
    setError(null);
    try {
      const next = await invoke<DirectoryListing>("browse_directory", {
        path: target ?? null,
        showHidden: hidden,
      });
      setListing(next);
      setPathInput(next.currentPath);
    } catch (failure) {
      setError(String(failure));
    } finally {
      setLoading(false);
    }
  }, []);

  useEffect(() => {
    registerFolderPicker((request) => {
      return new Promise<string | null>((resolve) => {
        setActive({ request, resolve });
      });
    });
    return () => registerFolderPicker(null);
  }, []);

  useEffect(() => {
    if (active !== null) {
      setShowHidden(false);
      void loadPath(undefined, false);
    }
  }, [active, loadPath]);

  useEffect(() => {
    if (active === null) return;
    const onKeyDown = (event: KeyboardEvent) => {
      if (event.key === "Escape") {
        event.preventDefault();
        active.resolve(null);
        setActive(null);
      }
    };
    window.addEventListener("keydown", onKeyDown);
    return () => window.removeEventListener("keydown", onKeyDown);
  }, [active]);

  if (active === null) return null;

  const close = (result: string | null) => {
    const resolver = active.resolve;
    setActive(null);
    resolver(result);
  };

  return (
    <div
      className="folder-picker-backdrop"
      role="presentation"
      onClick={(event) => {
        if (event.target === event.currentTarget) close(null);
      }}
    >
      <div
        className="folder-picker-dialog"
        role="dialog"
        aria-modal="true"
        aria-labelledby="folder-picker-title"
      >
        <div className="folder-picker-header">
          <h2 id="folder-picker-title">{active.request.title}</h2>
          <p className="hint">{active.request.description}</p>
        </div>

        <form
          className="folder-picker-pathbar"
          onSubmit={(event) => {
            event.preventDefault();
            void loadPath(pathInput, showHidden);
          }}
        >
          <input
            type="text"
            value={pathInput}
            aria-label="Folder path"
            onChange={(event) => setPathInput(event.target.value)}
            placeholder="/path/to/folder"
          />
          <button type="submit" disabled={loading}>
            Go
          </button>
          {listing?.parentPath && (
            <button
              type="button"
              disabled={loading}
              onClick={() => void loadPath(listing.parentPath ?? undefined, showHidden)}
            >
              Up
            </button>
          )}
        </form>

        <div className="folder-picker-toolbar">
          <label className="folder-picker-hidden-toggle">
            <input
              type="checkbox"
              checked={showHidden}
              onChange={(event) => {
                const next = event.target.checked;
                setShowHidden(next);
                void loadPath(listing?.currentPath ?? pathInput, next);
              }}
            />
            Show hidden folders
          </label>
          {listing?.hasPocketYaml && (
            <span className="folder-picker-badge">Contains pocket.yaml</span>
          )}
        </div>

        {error && (
          <p role="alert" className="error folder-picker-error">
            {error}
          </p>
        )}

        <div className="folder-picker-list" role="list" aria-busy={loading}>
          {loading && listing === null && (
            <p className="hint folder-picker-empty">Reading folders…</p>
          )}
          {!loading && listing !== null && listing.entries.length === 0 && (
            <p className="hint folder-picker-empty">
              No subfolders here. You can still choose this folder.
            </p>
          )}
          {listing?.entries.map((entry) => (
            <button
              key={entry.path}
              type="button"
              role="listitem"
              className="folder-picker-row"
              onClick={() => void loadPath(entry.path, showHidden)}
            >
              <span className="folder-picker-name">{entry.name}</span>
              {entry.hasPocketYaml && (
                <span className="folder-picker-badge">pocket.yaml</span>
              )}
            </button>
          ))}
        </div>

        <div className="folder-picker-footer">
          <button type="button" onClick={() => close(null)}>
            Cancel
          </button>
          <button
            type="button"
            className="button-primary"
            disabled={loading || pathInput.trim() === ""}
            onClick={() => close(listing?.currentPath ?? pathInput.trim())}
          >
            {active.request.confirmLabel}
          </button>
        </div>
      </div>
    </div>
  );
}
