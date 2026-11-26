import { useEffect, useId, useRef, useState } from "react";
import { invoke } from "./api";
import type {
  PreviewApi,
  Project,
  ProjectManifest,
  RuntimeVariable,
} from "./CreationPanel";
import { TitleBar } from "./TitleBar";
import { IconButton, StatusPip } from "./Status";
import { IconPanelLeft, IconPanelRight } from "./Icons";
import { formatVersionTag } from "./formatVersion";
import "./NewProject.css";

const FOLDER_RE = /^[a-z0-9]+(-[a-z0-9]+)*$/;

export interface HubPocket {
  repo: string;
  repoUrl: string;
  id?: string | null;
  name: string;
  description: string;
  version?: string | null;
  category?: string | null;
  tags?: string[];
  verified: boolean;
  metadataSource: "repo" | "catalog" | string;
  hostSupported: boolean;
  hostError?: string | null;
  architectures?: string[];
  previewApis?: PreviewApi[];
  runtimeVariables?: RuntimeVariable[];
  existingProjectId?: string | null;
}

export interface HubCatalogResponse {
  source: "remote" | "bundled" | string;
  hubUrl: string;
  pockets: HubPocket[];
}

export interface GitHubPocketPreview {
  owner: string;
  repo: string;
  slug: string;
  repoUrl: string;
  manifest: ProjectManifest;
  hostSupported: boolean;
  hostError?: string | null;
  existingProjectId?: string | null;
}

function capabilitySummary(
  runtimeVariables?: RuntimeVariable[],
  previewApis?: PreviewApi[],
): string | null {
  const parts: string[] = [];
  const varCount = runtimeVariables?.length ?? 0;
  const apiCount = previewApis?.length ?? 0;
  if (varCount > 0) {
    parts.push(`${varCount} runtime ${varCount === 1 ? "variable" : "variables"}`);
  }
  if (apiCount > 0) {
    parts.push(`${apiCount} preview ${apiCount === 1 ? "API" : "APIs"}`);
  }
  return parts.length > 0 ? parts.join(" · ") : null;
}

export default function NewProject({
  sidebarCollapsed,
  previewCollapsed,
  onToggleSidebar,
  onTogglePreview,
  onCreated,
  onSelectProject,
}: {
  sidebarCollapsed: boolean;
  previewCollapsed: boolean;
  onToggleSidebar: () => void;
  onTogglePreview: () => void;
  onCreated: (project: Project) => void;
  onSelectProject?: (id: string) => void;
}) {
  const [name, setName] = useState("");
  const [folder, setFolder] = useState("");
  const [customFolder, setCustomFolder] = useState(false);

  const [hubPockets, setHubPockets] = useState<HubPocket[]>([]);
  const [hubLoading, setHubLoading] = useState(true);
  const [hubError, setHubError] = useState<string | null>(null);

  const [githubSource, setGithubSource] = useState("");
  const [inspecting, setInspecting] = useState(false);
  const [repoPreview, setRepoPreview] = useState<GitHubPocketPreview | null>(
    null,
  );
  const [inspectError, setInspectError] = useState<string | null>(null);

  const [busy, setBusy] = useState(false);
  const [pullingRepo, setPullingRepo] = useState<string | null>(null);
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

  useEffect(() => {
    let cancelled = false;
    setHubLoading(true);
    setHubError(null);
    invoke<HubCatalogResponse | undefined>("list_hub_pockets", {})
      .then((response) => {
        if (!cancelled && response && Array.isArray(response.pockets)) {
          setHubPockets(response.pockets);
        }
      })
      .catch((failure) => {
        if (!cancelled) setHubError(String(failure));
      })
      .finally(() => {
        if (!cancelled) setHubLoading(false);
      });
    return () => {
      cancelled = true;
    };
  }, []);

  async function submitCreateOrLink(link = false) {
    if (inFlight.current) return;
    inFlight.current = true;
    setBusy(true);
    setError(null);
    try {
      const project = await invoke<Project | null>(
        link ? "link_development_folder" : "create_development_project",
        link ? {} : { name: name.trim(), folderName: folder },
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

  async function inspectGitHubRepo() {
    const trimmed = githubSource.trim();
    if (inFlight.current || inspecting || trimmed === "") return;
    setInspecting(true);
    setInspectError(null);
    setRepoPreview(null);
    setError(null);
    try {
      const preview = await invoke<GitHubPocketPreview>(
        "inspect_github_pocket",
        { source: trimmed },
      );
      if (mounted.current) setRepoPreview(preview);
    } catch (failure) {
      if (mounted.current) setInspectError(String(failure));
    } finally {
      if (mounted.current) setInspecting(false);
    }
  }

  async function pullGitHubRepo(source: string) {
    const trimmed = source.trim();
    if (inFlight.current || trimmed === "") return;
    inFlight.current = true;
    setBusy(true);
    setPullingRepo(trimmed);
    setError(null);
    try {
      const project = await invoke<Project>("add_github_pocket", {
        source: trimmed,
      });
      onCreated({ ...project, developmentLinked: true });
    } catch (failure) {
      if (mounted.current) setError(String(failure));
    } finally {
      inFlight.current = false;
      if (mounted.current) {
        setBusy(false);
        setPullingRepo(null);
      }
    }
  }

  const folderValid = FOLDER_RE.test(folder) && folder.length <= 63;
  const showFolderError =
    !folderValid && (folder.length > 0 || customFolder || name.trim() !== "");
  const createValid = name.trim() !== "" && name.length <= 80 && folderValid;

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
          {error && (
            <p role="alert" className="error">
              {error}
            </p>
          )}

          <section className="new-project-section" aria-label="Pockets Hub">
            <div className="new-project-intro">
              <h2>Pockets Hub</h2>
              <p className="hint">
                Pull ready-to-run Pockets directly from their repositories.
                Pulling clones the source without running code — you choose
                Preview or Build &amp; start.
              </p>
            </div>

            {hubError && (
              <p role="alert" className="error">
                {hubError}
              </p>
            )}

            {hubLoading && hubPockets.length === 0 ? (
              <p className="hint">Loading featured Pockets…</p>
            ) : hubPockets.length > 0 ? (
              <ul className="new-project-hub-grid">
                {hubPockets.map((pocket) => {
                  const caps = capabilitySummary(
                    pocket.runtimeVariables,
                    pocket.previewApis,
                  );
                  const isPulling = pullingRepo === pocket.repo;
                  const alreadyAdded = Boolean(pocket.existingProjectId);
                  return (
                    <li key={pocket.repo} className="new-project-hub-card">
                      <div className="new-project-hub-card-head">
                        <div className="new-project-hub-title-row">
                          <h3 className="new-project-hub-name">
                            {pocket.name}
                          </h3>
                          {pocket.version && (
                            <span className="new-project-tag">
                              {formatVersionTag(pocket.version)}
                            </span>
                          )}
                        </div>
                        {pocket.verified && (
                          <div className="new-project-hub-badges">
                            <span className="new-project-tag new-project-tag-verified">
                              Verified
                            </span>
                          </div>
                        )}
                      </div>
                      {pocket.description && (
                        <p className="new-project-hub-description">
                          {pocket.description}
                        </p>
                      )}
                      <p className="new-project-hub-meta">
                        <span>{pocket.repo}</span>
                        {caps && <span> · {caps}</span>}
                      </p>
                      {!pocket.hostSupported && pocket.hostError && (
                        <p className="error">{pocket.hostError}</p>
                      )}
                      <div className="new-project-hub-actions">
                        {alreadyAdded &&
                        pocket.existingProjectId &&
                        onSelectProject ? (
                          <>
                            <span className="new-project-hub-added">
                              Already in Pockets
                            </span>
                            <button
                              type="button"
                              className="new-project-action-right"
                              disabled={busy}
                              onClick={() =>
                                onSelectProject(pocket.existingProjectId!)
                              }
                            >
                              Open Pocket
                            </button>
                          </>
                        ) : (
                          <button
                            type="button"
                            className="button-primary new-project-action-right"
                            disabled={busy || !pocket.hostSupported}
                            onClick={() => void pullGitHubRepo(pocket.repo)}
                          >
                            {isPulling
                              ? "Pulling…"
                              : alreadyAdded
                                ? "Open Pocket"
                                : "Pull Pocket"}
                          </button>
                        )}
                      </div>
                    </li>
                  );
                })}
              </ul>
            ) : null}
          </section>

          <section
            className="new-project-section"
            aria-label="Pull from GitHub"
          >
            <div className="new-project-intro">
              <h2>Pull from GitHub</h2>
              <p className="hint">
                Enter any public GitHub Pocket repository. Pockets checks its{" "}
                <code>pocket.yaml</code> and <code>Containerfile</code> before
                cloning.
              </p>
            </div>
            <form
              className="new-project-github-form"
              onSubmit={(event) => {
                event.preventDefault();
                void inspectGitHubRepo();
              }}
              aria-busy={inspecting || busy}
            >
              <div className="new-project-github">
                <input
                  type="text"
                  value={githubSource}
                  disabled={busy || inspecting}
                  aria-label="GitHub repository"
                  onChange={(event) => {
                    setGithubSource(event.target.value);
                    setRepoPreview(null);
                    setInspectError(null);
                  }}
                  placeholder="owner/repo or https://github.com/owner/repo"
                />
                <button
                  type="submit"
                  disabled={busy || inspecting || githubSource.trim() === ""}
                >
                  {inspecting ? "Checking…" : "Check repo"}
                </button>
              </div>
            </form>

            {inspectError && (
              <p role="alert" className="error">
                {inspectError}
              </p>
            )}

            {repoPreview && (
              <div
                className="new-project-repo-preview"
                role="region"
                aria-label="Validated repository preview"
              >
                <div className="new-project-repo-status">
                  <StatusPip
                    tone={repoPreview.hostSupported ? "ok" : "warn"}
                    word={
                      repoPreview.hostSupported
                        ? "Valid Pocket repository"
                        : "Incompatible host architecture"
                    }
                  />
                  <span className="new-project-repo-slug">
                    {repoPreview.slug}
                  </span>
                </div>
                <div className="new-project-hub-title-row">
                  <h3 className="new-project-hub-name">
                    {repoPreview.manifest.metadata.name}
                  </h3>
                  <span className="new-project-tag">
                    {formatVersionTag(repoPreview.manifest.metadata.version)}
                  </span>
                </div>
                <p className="new-project-hub-description">
                  {repoPreview.manifest.metadata.description}
                </p>
                {(() => {
                  const caps = capabilitySummary(
                    repoPreview.manifest.runtimeVariables,
                    repoPreview.manifest.previewApis,
                  );
                  const archs =
                    repoPreview.manifest.runtime.architectures.join(", ");
                  return (
                    <p className="new-project-hub-meta">
                      <span>Arch: {archs}</span>
                      {caps && <span> · {caps}</span>}
                    </p>
                  );
                })()}
                {!repoPreview.hostSupported && repoPreview.hostError && (
                  <p className="error">{repoPreview.hostError}</p>
                )}
                <div className="new-project-hub-actions">
                  {repoPreview.existingProjectId && onSelectProject ? (
                    <>
                      <span className="new-project-hub-added">
                        Already in Pockets
                      </span>
                      <button
                        type="button"
                        className="new-project-action-right"
                        disabled={busy}
                        onClick={() =>
                          onSelectProject(repoPreview.existingProjectId!)
                        }
                      >
                        Open Pocket
                      </button>
                    </>
                  ) : (
                    <button
                      type="button"
                      className="button-primary new-project-action-right"
                      disabled={busy || !repoPreview.hostSupported}
                      onClick={() => void pullGitHubRepo(repoPreview.slug)}
                    >
                      {pullingRepo === repoPreview.slug
                        ? "Pulling…"
                        : "Pull Pocket"}
                    </button>
                  )}
                </div>
              </div>
            )}
          </section>

          <section
            className="new-project-section"
            aria-label="Create a project"
          >
            <div className="new-project-intro">
              <h2>Create a project</h2>
              <p className="hint">
                Start from a runnable Node template, build in your preferred
                coding tool, and watch saved edits refresh in Pockets.
              </p>
            </div>
            <form
              className="new-project-create-form"
              onSubmit={(event) => {
                event.preventDefault();
                if (createValid) void submitCreateOrLink();
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
                  Use lowercase letters, numbers, and single hyphens (for
                  example, my-portfolio).
                </p>
              )}
              <p className="hint">
                Choose a parent folder next. Pockets creates a new subfolder
                named {folder || "after your project"} and never overwrites an
                existing folder.
              </p>
              <button
                type="submit"
                className="button-primary"
                disabled={busy || !createValid}
              >
                {busy && pullingRepo === null
                  ? "Working…"
                  : "Choose location & create"}
              </button>
            </form>
            <p className="hint">
              The starter includes a runnable Node app, tests and instructions
              for your coding tool. Creating it runs no code.
            </p>
          </section>

          <div className="new-project-existing">
            <p className="hint">Already have a Pocket source folder?</p>
            <button
              type="button"
              disabled={busy}
              onClick={() => void submitCreateOrLink(true)}
            >
              Link existing project
            </button>
          </div>
        </div>
      </div>
    </div>
  );
}
