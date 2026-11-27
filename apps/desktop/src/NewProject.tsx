import { useEffect, useId, useRef, useState } from "react";
import { invoke } from "./api";
import type {
  PreviewApi,
  Project,
  ProjectManifest,
  RuntimeVariable,
} from "./CreationPanel";
import type { PocketizeSourceInfo } from "./ExternalWorkspace";
import { TitleBar } from "./TitleBar";
import { IconButton, StatusPip } from "./Status";
import {
  IconCheck,
  IconFolderOpen,
  IconPanelLeft,
  IconPanelRight,
} from "./Icons";
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

export interface PocketizeInspection {
  folderName: string;
  folderPath: string;
  alreadyPocket: boolean;
  existingName?: string | null;
  existingId?: string | null;
  suggestedName: string;
  suggestedId: string;
  suggestedSubfolder: string;
  detectedStack?: string | null;
  hasServerJs: boolean;
  hasDockerfile: boolean;
  hasContainerfile: boolean;
  filesToAdd: string[];
  filesToUpdate: string[];
  existingFilesKept: string[];
  warnings: string[];
  blockers: string[];
}

function joinPathForDisplay(parent: string, child: string): string {
  const sep = parent.includes("\\") && !parent.includes("/") ? "\\" : "/";
  const trimmed = parent.endsWith(sep) ? parent.slice(0, -1) : parent;
  return `${trimmed}${sep}${child}`;
}

function slugFromName(value: string): string {
  return value
    .normalize("NFKD")
    .replace(/[\u0300-\u036f]/g, "")
    .toLowerCase()
    .replace(/[^a-z0-9]+/g, "-")
    .replace(/^-|-$/g, "")
    .slice(0, 63)
    .replace(/-$/, "");
}

function safeExternalUrl(url?: string | null): string | null {
  if (!url) return null;
  return /^https?:\/\//i.test(url) ? url : null;
}

function isMacHost(): boolean {
  if (typeof navigator !== "undefined") {
    const platform = `${navigator.platform ?? ""} ${navigator.userAgent ?? ""}`;
    if (/mac/i.test(platform)) {
      return true;
    }
  }
  return false;
}

function hostRuntimeNote(): string {
  return isMacHost()
    ? "Runs on this Mac with Podman"
    : "Runs on this computer with Podman";
}

type ExistingTab = "hub" | "github" | "folder";

function capabilitySummary(
  runtimeVariables?: RuntimeVariable[],
  previewApis?: PreviewApi[],
): string | null {
  const parts: string[] = [];
  const varCount = runtimeVariables?.length ?? 0;
  const apiCount = previewApis?.length ?? 0;
  if (varCount > 0) {
    parts.push(`${varCount} ${varCount === 1 ? "setting" : "settings"}`);
  }
  if (apiCount > 0) {
    parts.push(
      `${apiCount} ${apiCount === 1 ? "web service" : "web services"}`,
    );
  }
  return parts.length > 0 ? parts.join(" · ") : null;
}

function HubPocketFavicon({
  repo,
  existingProjectId,
}: {
  repo: string;
  existingProjectId?: string | null;
}) {
  const src = existingProjectId
    ? `/api/projects/${encodeURIComponent(existingProjectId)}/favicon`
    : `/api/hub/favicon?repo=${encodeURIComponent(repo)}`;
  const [loadState, setLoadState] = useState<{
    src: string;
    status: "loaded" | "missing";
  } | null>(null);
  const imgRef = useRef<HTMLImageElement | null>(null);

  const status = loadState?.src === src ? loadState.status : "loading";

  useEffect(() => {
    const img = imgRef.current;
    if (img && img.complete && img.naturalWidth > 0) {
      setLoadState({ src, status: "loaded" });
    }
  }, [src]);

  if (status === "missing") {
    return null;
  }

  return (
    <img
      key={src}
      ref={imgRef}
      className="new-project-hub-favicon"
      src={src}
      alt=""
      aria-hidden="true"
      data-loaded={status === "loaded" ? "true" : undefined}
      onLoad={() => setLoadState({ src, status: "loaded" })}
      onError={() => setLoadState({ src, status: "missing" })}
    />
  );
}

function HubPocketCard({
  pocket,
  busy,
  isPulling,
  onPullRepo,
  onSelectProject,
}: {
  pocket: HubPocket;
  busy: boolean;
  isPulling: boolean;
  onPullRepo: (repo: string) => void;
  onSelectProject?: (id: string) => void;
}) {
  const alreadyAdded = Boolean(pocket.existingProjectId);

  return (
    <li className="new-project-hub-card">
      <div className="new-project-hub-body">
        <div className="new-project-hub-title-row">
          <div className="new-project-hub-identity">
            <HubPocketFavicon
              repo={pocket.repo}
              existingProjectId={pocket.existingProjectId}
            />
            <h3 className="new-project-hub-name">{pocket.name}</h3>
          </div>
          {pocket.version && (
            <span className="new-project-hub-version">
              {formatVersionTag(pocket.version)}
            </span>
          )}
          {pocket.verified && (
            <span className="new-project-hub-verified">
              <IconCheck size={12} />
              Verified
            </span>
          )}
        </div>
        {pocket.description && (
          <p className="new-project-hub-description">{pocket.description}</p>
        )}
        <p className="new-project-hub-facts">
          {safeExternalUrl(pocket.repoUrl) ? (
            <a
              href={pocket.repoUrl}
              target="_blank"
              rel="noopener noreferrer"
              className="new-project-hub-source-link"
            >
              <span>{pocket.repo}</span>
              <span className="new-project-hub-source-arrow" aria-hidden="true">
                ↗
              </span>
            </a>
          ) : (
            <span className="new-project-hub-source-text">{pocket.repo}</span>
          )}
        </p>
      </div>

      <div className="new-project-hub-footer">
        {alreadyAdded && pocket.existingProjectId && onSelectProject ? (
          <>
            <span className="new-project-hub-added">
              <StatusPip tone="ok" word="In your Pockets" />
            </span>
            <button
              type="button"
              className="new-project-hub-action"
              disabled={busy}
              onClick={() => onSelectProject(pocket.existingProjectId!)}
            >
              Manage
            </button>
          </>
        ) : (
          <>
            {!pocket.hostSupported && pocket.hostError ? (
              <span className="new-project-hub-foot-error">
                {pocket.hostError}
              </span>
            ) : (
              <span className="new-project-hub-foot-hint">
                {hostRuntimeNote()}
              </span>
            )}
            <button
              type="button"
              className="button-primary new-project-hub-action"
              disabled={busy || !pocket.hostSupported}
              onClick={() => onPullRepo(pocket.repo)}
            >
              {isPulling ? "Adding…" : "Add to Pockets"}
            </button>
          </>
        )}
      </div>
    </li>
  );
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
  onCreated: (project: Project, pocketizeSource?: PocketizeSourceInfo) => void;
  onSelectProject?: (id: string) => void;
}) {
  const [name, setName] = useState("");
  const [folder, setFolder] = useState("");
  const [customFolder, setCustomFolder] = useState(false);
  const [editingFolder, setEditingFolder] = useState(false);
  const [existingTab, setExistingTab] = useState<ExistingTab>("hub");

  const [hubPockets, setHubPockets] = useState<HubPocket[]>([]);
  const [hubLoading, setHubLoading] = useState(true);
  const [hubError, setHubError] = useState<string | null>(null);

  const [githubSource, setGithubSource] = useState("");
  const [inspecting, setInspecting] = useState(false);
  const [repoPreview, setRepoPreview] = useState<GitHubPocketPreview | null>(
    null,
  );
  const [inspectError, setInspectError] = useState<string | null>(null);

  const [pocketizePlan, setPocketizePlan] =
    useState<PocketizeInspection | null>(null);
  const [pocketizeName, setPocketizeName] = useState("");
  const [pocketizeId, setPocketizeId] = useState("");
  const [pocketizeCustomId, setPocketizeCustomId] = useState(false);
  const [pocketizeCustomParent, setPocketizeCustomParent] = useState<
    string | null
  >(null);
  const [pocketizeError, setPocketizeError] = useState<string | null>(null);

  const [busy, setBusy] = useState(false);
  const [pullingRepo, setPullingRepo] = useState<string | null>(null);
  const [error, setError] = useState<string | null>(null);

  const folderErrorId = useId();
  const pocketizeIdErrorId = useId();
  const folderInputRef = useRef<HTMLInputElement | null>(null);
  const inFlight = useRef(false);
  const mounted = useRef(true);
  useEffect(() => {
    mounted.current = true;
    return () => {
      mounted.current = false;
    };
  }, []);

  useEffect(() => {
    if (editingFolder) {
      folderInputRef.current?.focus();
    }
  }, [editingFolder]);

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

  async function chooseLocalFolder() {
    if (inFlight.current) return;
    inFlight.current = true;
    setBusy(true);
    setError(null);
    setPocketizeError(null);
    try {
      const inspection = await invoke<PocketizeInspection | null>(
        "inspect_pocketize_folder",
        {},
      );
      if (inspection === null) return;
      if (mounted.current) {
        setPocketizePlan(inspection);
        setPocketizeName(inspection.suggestedName);
        setPocketizeId(inspection.suggestedId);
        setPocketizeCustomId(false);
        setPocketizeCustomParent(null);
      }
    } catch (failure) {
      if (mounted.current) setPocketizeError(String(failure));
    } finally {
      inFlight.current = false;
      if (mounted.current) setBusy(false);
    }
  }

  async function submitLinkFolder() {
    if (inFlight.current || pocketizePlan === null) return;
    inFlight.current = true;
    setBusy(true);
    setError(null);
    setPocketizeError(null);
    try {
      const project = await invoke<Project | null>("link_development_folder", {
        folderPath: pocketizePlan.folderPath,
      });
      if (project !== null) {
        onCreated({ ...project, developmentLinked: true });
      }
    } catch (failure) {
      if (mounted.current) setPocketizeError(String(failure));
    } finally {
      inFlight.current = false;
      if (mounted.current) setBusy(false);
    }
  }

  async function choosePocketizeLocation() {
    if (inFlight.current || pocketizePlan === null) return;
    inFlight.current = true;
    setBusy(true);
    setPocketizeError(null);
    try {
      const picked = await invoke<string | null>("pick_parent_folder", {
        folderName: pocketizeId || pocketizePlan.suggestedId,
      });
      if (picked !== null && mounted.current) {
        setPocketizeCustomParent(
          picked === pocketizePlan.folderPath ? null : picked,
        );
      }
    } catch (failure) {
      if (mounted.current) setPocketizeError(String(failure));
    } finally {
      inFlight.current = false;
      if (mounted.current) setBusy(false);
    }
  }

  async function submitPocketize() {
    if (inFlight.current || pocketizePlan === null) return;
    inFlight.current = true;
    setBusy(true);
    setError(null);
    setPocketizeError(null);
    const insideSource = pocketizeCustomParent === null;
    const parentPath = pocketizeCustomParent ?? pocketizePlan.folderPath;
    const targetFolderName = insideSource
      ? pocketizePlan.suggestedSubfolder || "pocket"
      : pocketizeId;
    try {
      const created = await invoke<Project | null>(
        "create_development_project",
        {
          parentPath,
          name: pocketizeName.trim(),
          id: pocketizeId,
          folderName: targetFolderName,
        },
      );
      if (created !== null) {
        onCreated(
          { ...created, developmentLinked: true },
          {
            folderPath: pocketizePlan.folderPath,
            folderName: pocketizePlan.folderName,
            detectedStack: pocketizePlan.detectedStack ?? null,
            targetSubfolder: insideSource ? targetFolderName : null,
          },
        );
      }
    } catch (failure) {
      if (mounted.current) setPocketizeError(String(failure));
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
  const showFolderInput = editingFolder || showFolderError;
  const createValid = name.trim() !== "" && name.length <= 80 && folderValid;

  const pocketizeIdValid =
    FOLDER_RE.test(pocketizeId) && pocketizeId.length <= 63;
  const showPocketizeIdError =
    pocketizePlan !== null &&
    !pocketizeIdValid &&
    (pocketizeId.length > 0 ||
      pocketizeCustomId ||
      pocketizeName.trim() !== "");
  const pocketizeValid =
    pocketizePlan !== null &&
    pocketizePlan.blockers.length === 0 &&
    pocketizeName.trim() !== "" &&
    pocketizeName.length <= 80 &&
    pocketizeIdValid;

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

          <section
            className="new-project-section new-project-create-section"
            aria-label="Create a project"
          >
            <div className="new-project-intro">
              <h2 className="new-project-heading">Create a project</h2>
              <p className="hint">
                Start from a runnable Node template, build it in your coding
                tool, and watch saved edits refresh here.
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
              <div className="new-project-field-group">
                <label className="new-project-field">
                  <span className="new-project-field-label">Project name</span>
                  <input
                    className="new-project-name-input"
                    value={name}
                    maxLength={80}
                    required
                    disabled={busy}
                    onChange={(event) => {
                      const value = event.target.value;
                      setName(value);
                      if (!customFolder) setFolder(slugFromName(value));
                    }}
                    placeholder="Habit tracker"
                  />
                </label>

                {showFolderInput ? (
                  <div className="new-project-folder-editor">
                    <div className="new-project-folder-editor-row">
                      <label className="new-project-folder-label">
                        <span className="new-project-folder-prefix">
                          Folder name
                        </span>
                        <input
                          ref={folderInputRef}
                          value={folder}
                          maxLength={63}
                          pattern="[a-z0-9]+(-[a-z0-9]+)*"
                          required
                          disabled={busy}
                          aria-invalid={showFolderError || undefined}
                          aria-describedby={
                            showFolderError ? folderErrorId : undefined
                          }
                          onChange={(event) => {
                            setCustomFolder(true);
                            setFolder(event.target.value);
                          }}
                          placeholder="habit-tracker"
                        />
                      </label>
                      {folderValid && (
                        <button
                          type="button"
                          className="new-project-link-button"
                          disabled={busy}
                          onClick={() => setEditingFolder(false)}
                        >
                          Done
                        </button>
                      )}
                    </div>
                    {showFolderError && (
                      <p id={folderErrorId} className="error">
                        Use lowercase letters, numbers, and single hyphens (for
                        example, habit-tracker).
                      </p>
                    )}
                  </div>
                ) : (
                  <div className="new-project-folder-inline">
                    <span className="new-project-folder-prefix">Folder</span>
                    <span
                      className={
                        folder
                          ? "new-project-folder-value"
                          : "new-project-folder-placeholder"
                      }
                    >
                      {folder || "habit-tracker"}
                    </span>
                    <span className="new-project-folder-sep" aria-hidden="true">
                      ·
                    </span>
                    <button
                      type="button"
                      className="new-project-link-button"
                      disabled={busy}
                      onClick={() => setEditingFolder(true)}
                    >
                      Change
                    </button>
                  </div>
                )}
              </div>

              <div className="new-project-submit-row">
                <button
                  type="submit"
                  className="button-primary new-project-submit-button"
                  disabled={busy || !createValid}
                >
                  {busy && pullingRepo === null
                    ? "Working…"
                    : "Create project…"}
                </button>
                <p className="hint new-project-submit-hint">
                  Pockets scaffolds a new folder with the starter app, manifest,
                  tests, and agent instructions.
                </p>
              </div>
            </form>
          </section>

          <section
            className="new-project-section new-project-existing-section"
            aria-label="Start from an existing Pocket"
          >
            <h2 className="new-project-subheading">
              Or start from an existing Pocket
            </h2>

            <div
              className="new-project-tabs"
              role="tablist"
              aria-label="Start from an existing Pocket"
            >
              <button
                type="button"
                role="tab"
                id="new-project-tab-hub"
                aria-selected={existingTab === "hub"}
                aria-controls="new-project-panel-hub"
                className="new-project-tab"
                onClick={() => setExistingTab("hub")}
              >
                Pockets Hub
              </button>
              <button
                type="button"
                role="tab"
                id="new-project-tab-github"
                aria-selected={existingTab === "github"}
                aria-controls="new-project-panel-github"
                className="new-project-tab"
                onClick={() => setExistingTab("github")}
              >
                GitHub
              </button>
              <button
                type="button"
                role="tab"
                id="new-project-tab-folder"
                aria-selected={existingTab === "folder"}
                aria-controls="new-project-panel-folder"
                className="new-project-tab"
                onClick={() => setExistingTab("folder")}
              >
                Local folder
              </button>
            </div>

            {existingTab === "hub" && (
              <div
                role="tabpanel"
                id="new-project-panel-hub"
                aria-labelledby="new-project-tab-hub"
                className="new-project-tabpanel"
              >
                <p className="hint">
                  Ready-to-run Pockets. Pulling copies the source without
                  running it.
                </p>

                {hubError && (
                  <p role="alert" className="error">
                    {hubError}
                  </p>
                )}

                {hubLoading && hubPockets.length === 0 ? (
                  <p className="hint">Loading featured Pockets…</p>
                ) : hubPockets.length > 0 ? (
                  <ul className="new-project-hub-grid">
                    {hubPockets.map((pocket) => (
                      <HubPocketCard
                        key={pocket.repo}
                        pocket={pocket}
                        busy={busy}
                        isPulling={pullingRepo === pocket.repo}
                        onPullRepo={(repo) => void pullGitHubRepo(repo)}
                        onSelectProject={onSelectProject}
                      />
                    ))}
                  </ul>
                ) : null}
              </div>
            )}

            {existingTab === "github" && (
              <div
                role="tabpanel"
                id="new-project-panel-github"
                aria-labelledby="new-project-tab-github"
                className="new-project-tabpanel"
              >
                <p className="hint">
                  Enter any public GitHub Pocket repository. Pockets checks its{" "}
                  <code>pocket.yaml</code> and <code>Containerfile</code> before
                  cloning.
                </p>
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
                      disabled={
                        busy || inspecting || githubSource.trim() === ""
                      }
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
                      {safeExternalUrl(repoPreview.repoUrl) ? (
                        <a
                          href={repoPreview.repoUrl}
                          target="_blank"
                          rel="noopener noreferrer"
                          className="new-project-repo-slug new-project-repo-link"
                        >
                          {repoPreview.slug}
                        </a>
                      ) : (
                        <span className="new-project-repo-slug">
                          {repoPreview.slug}
                        </span>
                      )}
                    </div>
                    <div className="new-project-hub-title-row">
                      <h3 className="new-project-hub-name">
                        {repoPreview.manifest.metadata.name}
                      </h3>
                      <span className="new-project-tag">
                        {formatVersionTag(
                          repoPreview.manifest.metadata.version,
                        )}
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
                    <div className="new-project-repo-actions">
                      {repoPreview.existingProjectId && onSelectProject ? (
                        <>
                          <span className="new-project-hub-added">
                            <StatusPip tone="ok" word="In your Pockets" />
                          </span>
                          <button
                            type="button"
                            className="new-project-hub-action"
                            disabled={busy}
                            onClick={() =>
                              onSelectProject(repoPreview.existingProjectId!)
                            }
                          >
                            Manage
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
              </div>
            )}

            {existingTab === "folder" && (
              <div
                role="tabpanel"
                id="new-project-panel-folder"
                aria-labelledby="new-project-tab-folder"
                className="new-project-tabpanel"
              >
                {pocketizeError && (
                  <p role="alert" className="error">
                    {pocketizeError}
                  </p>
                )}
                {pocketizePlan === null ? (
                  <>
                    <p className="hint">
                      Link an existing Pocket folder, or choose any non-Pocket
                      project to <strong>pocketize</strong> (convert) it. When
                      pocketizing, Pockets creates a separate{" "}
                      <code>pocket/</code> folder and never changes your
                      existing files.
                    </p>
                    <div className="new-project-folder-actions">
                      <button
                        type="button"
                        disabled={busy}
                        onClick={() => void chooseLocalFolder()}
                      >
                        {busy ? "Checking folder…" : "Choose local folder…"}
                      </button>
                    </div>
                  </>
                ) : (
                  <div className="new-project-local-flow">
                    <div className="new-project-folder-picked">
                      <IconFolderOpen
                        size={15}
                        className="new-project-folder-picked-icon"
                      />
                      <div className="new-project-folder-picked-meta">
                        <div className="new-project-folder-picked-title">
                          <strong className="new-project-folder-picked-name">
                            {pocketizePlan.folderName}
                          </strong>
                          {pocketizePlan.detectedStack && (
                            <span className="new-project-folder-picked-stack">
                              {pocketizePlan.detectedStack}
                            </span>
                          )}
                        </div>
                        <span className="new-project-folder-picked-path">
                          {pocketizePlan.folderPath}
                        </span>
                      </div>
                      <button
                        type="button"
                        className="new-project-link-button"
                        disabled={busy}
                        onClick={() => void chooseLocalFolder()}
                      >
                        Change folder
                      </button>
                    </div>

                    {pocketizePlan.alreadyPocket ? (
                      <div
                        className="new-project-pocketize-body"
                        role="region"
                        aria-label="Existing Pocket folder"
                      >
                        <div className="new-project-already-pocket-status">
                          <StatusPip
                            tone={
                              pocketizePlan.blockers.length === 0
                                ? "ok"
                                : "warn"
                            }
                            word="Already a Pocket"
                          />
                          {pocketizePlan.existingName && (
                            <>
                              <span
                                className="new-project-folder-sep"
                                aria-hidden="true"
                              >
                                ·
                              </span>
                              <span>{pocketizePlan.existingName}</span>
                            </>
                          )}
                          {pocketizePlan.existingId && (
                            <>
                              <span
                                className="new-project-folder-sep"
                                aria-hidden="true"
                              >
                                ·
                              </span>
                              <code>{pocketizePlan.existingId}</code>
                            </>
                          )}
                        </div>
                        <p className="new-project-already-pocket-note">
                          Pockets links the folder where it is. Your coding tool
                          keeps working in it, and nothing is added or run.
                        </p>
                        {pocketizePlan.blockers.map((blocker) => (
                          <p key={blocker} role="alert" className="error">
                            {blocker}
                          </p>
                        ))}
                        <div className="new-project-pocketize-footer">
                          <button
                            type="button"
                            disabled={busy}
                            onClick={() => {
                              setPocketizePlan(null);
                              setPocketizeError(null);
                            }}
                          >
                            Cancel
                          </button>
                          <button
                            type="button"
                            className="button-primary"
                            disabled={busy || pocketizePlan.blockers.length > 0}
                            onClick={() => void submitLinkFolder()}
                          >
                            {busy ? "Linking…" : "Link folder"}
                          </button>
                        </div>
                      </div>
                    ) : (
                      <form
                        className="new-project-pocketize-body"
                        aria-label="Pocketize folder review"
                        onSubmit={(event) => {
                          event.preventDefault();
                          if (pocketizeValid) void submitPocketize();
                        }}
                      >
                        <div className="new-project-pocketize-fields">
                          <label className="new-project-field">
                            <span className="new-project-field-label">
                              Project name
                            </span>
                            <input
                              type="text"
                              value={pocketizeName}
                              maxLength={80}
                              disabled={busy}
                              onChange={(event) => {
                                const nextName = event.target.value;
                                setPocketizeName(nextName);
                                if (!pocketizeCustomId) {
                                  setPocketizeId(slugFromName(nextName));
                                }
                              }}
                            />
                          </label>
                          <div className="new-project-field-group">
                            <label className="new-project-field">
                              <span className="new-project-field-label">
                                Pocket ID
                              </span>
                              <input
                                type="text"
                                value={pocketizeId}
                                maxLength={63}
                                disabled={busy}
                                aria-invalid={showPocketizeIdError}
                                aria-describedby={
                                  showPocketizeIdError
                                    ? pocketizeIdErrorId
                                    : undefined
                                }
                                onChange={(event) => {
                                  setPocketizeCustomId(true);
                                  setPocketizeId(event.target.value);
                                }}
                              />
                            </label>
                            {showPocketizeIdError ? (
                              <p id={pocketizeIdErrorId} className="error">
                                Use lowercase letters, numbers, and single
                                hyphens (for example, my-project).
                              </p>
                            ) : (
                              <span className="new-project-field-note">
                                Used in its address, so pick one you’ll keep.
                              </span>
                            )}
                          </div>
                        </div>

                        {(() => {
                          const insideSource = pocketizeCustomParent === null;
                          const targetSubfolder =
                            pocketizePlan.suggestedSubfolder || "pocket";
                          const targetPath = joinPathForDisplay(
                            pocketizeCustomParent ?? pocketizePlan.folderPath,
                            insideSource
                              ? targetSubfolder
                              : pocketizeId || pocketizePlan.suggestedId,
                          );
                          return (
                            <div className="new-project-pocketize-destination">
                              <div className="new-project-pocketize-destination-row">
                                <span className="new-project-folder-prefix">
                                  Creates
                                </span>
                                <code className="new-project-folder-value">
                                  {targetPath}
                                </code>
                                <span
                                  className="new-project-folder-sep"
                                  aria-hidden="true"
                                >
                                  ·
                                </span>
                                <button
                                  type="button"
                                  className="new-project-link-button"
                                  disabled={busy}
                                  onClick={() => void choosePocketizeLocation()}
                                >
                                  Change location
                                </button>
                                {!insideSource && (
                                  <>
                                    <span
                                      className="new-project-folder-sep"
                                      aria-hidden="true"
                                    >
                                      ·
                                    </span>
                                    <button
                                      type="button"
                                      className="new-project-link-button"
                                      disabled={busy}
                                      onClick={() =>
                                        setPocketizeCustomParent(null)
                                      }
                                    >
                                      Inside {pocketizePlan.folderName}
                                    </button>
                                  </>
                                )}
                              </div>
                              <ul className="new-project-pocketize-steps">
                                <li>
                                  {insideSource ? (
                                    <>
                                      Creates a fresh starter Pocket in{" "}
                                      <code>{targetSubfolder}/</code> without
                                      modifying your existing{" "}
                                      <code>{pocketizePlan.folderName}</code>{" "}
                                      files.
                                    </>
                                  ) : (
                                    <>
                                      Creates a fresh starter Pocket at{" "}
                                      <code>{targetPath}</code> without
                                      modifying your original{" "}
                                      <code>{pocketizePlan.folderName}</code>{" "}
                                      folder.
                                    </>
                                  )}
                                </li>
                                <li>
                                  Does not auto-convert or run existing code.
                                  Next, you’ll get a tailored prompt for your
                                  coding tool to recreate the app here with Live
                                  Preview.
                                </li>
                              </ul>
                            </div>
                          );
                        })()}

                        {pocketizePlan.blockers.map((blocker) => (
                          <p key={blocker} role="alert" className="error">
                            {blocker}
                          </p>
                        ))}

                        <div className="new-project-pocketize-footer">
                          <button
                            type="button"
                            disabled={busy}
                            onClick={() => {
                              setPocketizePlan(null);
                              setPocketizeCustomParent(null);
                              setPocketizeError(null);
                            }}
                          >
                            Cancel
                          </button>
                          <button
                            type="submit"
                            className="button-primary"
                            disabled={busy || !pocketizeValid}
                          >
                            {busy ? "Pocketizing…" : "Pocketize"}
                          </button>
                        </div>
                      </form>
                    )}
                  </div>
                )}
              </div>
            )}
          </section>
        </div>
      </div>
    </div>
  );
}
