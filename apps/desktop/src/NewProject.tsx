import { useEffect, useId, useRef, useState } from "react";
import { invoke } from "./api";
import type { Project } from "./CreationPanel";
import { TitleBar } from "./TitleBar";
import { IconButton } from "./Status";
import { IconPanelLeft, IconPanelRight } from "./Icons";
import "./NewProject.css";

const FOLDER_RE = /^[a-z0-9]+(-[a-z0-9]+)*$/;

export default function NewProject({
  sidebarCollapsed,
  previewCollapsed,
  onToggleSidebar,
  onTogglePreview,
  onCreated,
}: {
  sidebarCollapsed: boolean;
  previewCollapsed: boolean;
  onToggleSidebar: () => void;
  onTogglePreview: () => void;
  onCreated: (project: Project) => void;
}) {
  const [name, setName] = useState("");
  const [folder, setFolder] = useState("");
  const [customFolder, setCustomFolder] = useState(false);
  const [brief, setBrief] = useState("");
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const folderErrorId = useId();
  const inFlight = useRef(false);
  const mounted = useRef(true);
  useEffect(() => {
    mounted.current = true;
    return () => {
      mounted.current = false;
    };
  }, []);

  async function submit(link = false) {
    if (inFlight.current) return;
    inFlight.current = true;
    setBusy(true);
    setError(null);
    try {
      const project = await invoke<Project | null>(
        link ? "link_development_folder" : "create_development_project",
        link ? {} : { name: name.trim(), folderName: folder, brief },
      );
      // Registration still deserves a catalog refresh if the user navigated
      // away during the native picker; App decides whether to navigate.
      if (project !== null) onCreated({ ...project, developmentLinked: true });
    } catch (failure) {
      if (mounted.current) setError(String(failure));
    } finally {
      inFlight.current = false;
      if (mounted.current) setBusy(false);
    }
  }

  const briefTooLong = new TextEncoder().encode(brief).length > 8_000;
  const folderValid = FOLDER_RE.test(folder) && folder.length <= 63;
  const showFolderError =
    !folderValid && (folder.length > 0 || customFolder || name.trim() !== "");
  const valid =
    name.trim() !== "" && name.length <= 80 && folderValid && !briefTooLong;
  return (
    <div className="new-project">
      <TitleBar
        ariaLabel="New project"
        title={<span>New pocket</span>}
        reserveTrafficLights={sidebarCollapsed}
        leading={
          sidebarCollapsed ? (
            <IconButton
              label="Toggle sidebar"
              title="Toggle sidebar ⌘B"
              onClick={onToggleSidebar}
            >
              <IconPanelLeft />
            </IconButton>
          ) : undefined
        }
        trailing={
          previewCollapsed ? (
            <IconButton label="Toggle preview" onClick={onTogglePreview}>
              <IconPanelRight />
            </IconButton>
          ) : undefined
        }
      />
      <div className="new-project-content">
        <div className="new-project-inner">
          <div className="new-project-intro">
            <h2>Create a project</h2>
            <p className="hint">
              Start here, build in your preferred coding tool, and watch saved
              edits refresh in Pockets.
            </p>
          </div>
          <form
            onSubmit={(event) => {
              event.preventDefault();
              if (valid) void submit();
            }}
            aria-busy={busy}
          >
            <label>
              Project name
              <input
                value={name}
                maxLength={80}
                required
                disabled={busy}
                onChange={(event) => {
                  const value = event.target.value;
                  setName(value);
                  if (!customFolder)
                    setFolder(
                      value
                        .normalize("NFKD")
                        .replace(/[\u0300-\u036f]/g, "")
                        .toLowerCase()
                        .replace(/[^a-z0-9]+/g, "-")
                        .replace(/^-|-$/g, "")
                        .slice(0, 63)
                        .replace(/-$/, ""),
                    );
                }}
                placeholder="My portfolio"
              />
            </label>
            <label>
              Folder name
              <input
                value={folder}
                maxLength={63}
                pattern="[a-z0-9]+(-[a-z0-9]+)*"
                required
                disabled={busy}
                aria-invalid={showFolderError || undefined}
                aria-describedby={showFolderError ? folderErrorId : undefined}
                onChange={(event) => {
                  setCustomFolder(true);
                  setFolder(event.target.value);
                }}
                placeholder="my-portfolio"
              />
            </label>
            {showFolderError && (
              <p id={folderErrorId} className="error">
                Use lowercase letters, numbers, and single hyphens (for example,
                my-portfolio).
              </p>
            )}
            <p className="hint">
              Choose a parent folder next. Pockets creates a new subfolder named{" "}
              {folder || "after your project"} and never overwrites an existing
              folder.
            </p>
            <label>
              Project brief (optional)
              <textarea
                value={brief}
                disabled={busy}
                rows={5}
                maxLength={8_000}
                onChange={(event) => setBrief(event.target.value)}
                placeholder="What should the app do, and how should it look?"
              />
            </label>
            {briefTooLong && (
              <p className="error">
                The brief is too long. Shorten it to continue.
              </p>
            )}
            <button
              type="submit"
              className="button-primary"
              disabled={busy || !valid}
            >
              {busy ? "Working…" : "Choose location & create"}
            </button>
          </form>
          <p className="hint">
            The starter includes a runnable Node app, tests and instructions for
            your coding tool. Creating it runs no code. You choose Start for
            preview and Build/Update for installation.
          </p>
          <div className="new-project-existing">
            <p className="hint">Already have a Pocket source folder?</p>
            <button
              type="button"
              disabled={busy}
              onClick={() => void submit(true)}
            >
              Link existing project
            </button>
          </div>
          {error && (
            <p role="alert" className="error">
              {error}
            </p>
          )}
        </div>
      </div>
    </div>
  );
}
