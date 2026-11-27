import { useEffect, useRef, useState } from "react";
import { invoke } from "./api";
import DevelopmentPanel from "./DevelopmentPanel";
import HubSubmissionPanel from "./HubSubmissionPanel";
import RuntimeSettings from "./RuntimeSettings";
import PreviewNetworkSettings from "./PreviewNetworkSettings";
import type { PreviewApiAccess } from "./useProjectPreview";
import type { PreviewApi } from "./CreationPanel";
import { TitleBar } from "./TitleBar";
import { IconButton } from "./Status";
import {
  IconCheck,
  IconClose,
  IconCopy,
  IconPanelLeft,
  IconPanelRight,
} from "./Icons";
import "./ExternalWorkspace.css";

export const PLACEHOLDER_CONTAINERFILE_MARKER =
  "# POCKETS-PLACEHOLDER-CONTAINERFILE";

export interface PocketizeSourceInfo {
  folderPath: string;
  folderName: string;
  detectedStack?: string | null;
  /** Non-null when the new Pocket folder was created inside the source repo (e.g. `"pocket"`). */
  targetSubfolder?: string | null;
}

export function buildCleanRoomPocketizePrompt(
  projectName: string,
  source: PocketizeSourceInfo,
): string {
  const stackClause = source.detectedStack ? ` (${source.detectedStack})` : "";
  if (source.targetSubfolder) {
    const sub = source.targetSubfolder;
    return `Read ${sub}/AGENTS.md and ${sub}/POCKETS.md, then recreate ${projectName}${stackClause} inside ${sub}/ using the existing project in this folder as reference. Keep metadata.id in ${sub}/pocket.yaml unchanged, persist app state under DATA_DIR (/data), declare any required environment variables under runtimeVariables in ${sub}/pocket.yaml, declare any external read-only HTTPS hosts under previewApis, and record a one-line summary in ${sub}/POCKETS_CHANGES.md when done.`;
  }
  return `Read AGENTS.md and POCKETS.md, then recreate the ${projectName} app from ${source.folderPath}${stackClause} inside this Pocket. Keep metadata.id in pocket.yaml unchanged, persist app state under DATA_DIR (/data), declare any required environment variables under runtimeVariables in pocket.yaml, declare any external read-only HTTPS hosts under previewApis, and record a one-line summary in POCKETS_CHANGES.md when done.`;
}

function detectStackFromFiles(files?: string[]): string | null {
  if (!files) return null;
  const set = new Set(files);
  if (set.has("package.json")) return "Node.js";
  if (
    set.has("pyproject.toml") ||
    set.has("requirements.txt") ||
    set.has("Pipfile")
  )
    return "Python";
  if (set.has("go.mod")) return "Go";
  if (set.has("Cargo.toml")) return "Rust";
  if (set.has("Gemfile")) return "Ruby";
  if (set.has("composer.json")) return "PHP";
  if (set.has("Dockerfile")) return "Docker";
  if (set.has("index.html")) return "Static Web";
  return null;
}

export function buildPocketizePrompt(
  projectName: string,
  files?: string[],
): string {
  const stack = detectStackFromFiles(files);
  const stackClause = stack ? ` (${stack})` : "";
  const hasDockerfile = files?.includes("Dockerfile") === true;
  const containerStep = hasDockerfile
    ? "replace the placeholder Containerfile by adapting the existing Dockerfile into a non-root container with a writable /data directory (removing the # POCKETS-PLACEHOLDER-CONTAINERFILE header)"
    : "replace the placeholder Containerfile with a clean non-root container for this app and a writable /data directory (removing the # POCKETS-PLACEHOLDER-CONTAINERFILE header)";
  return `Read POCKETS.md and adapt this existing ${projectName}${stackClause} project to run as a Pocket: ensure the HTTP server binds 0.0.0.0 on PORT (matching web.port in pocket.yaml), serves GET /health returning 200, persists app state under DATA_DIR (/data), declares any required environment variables under runtimeVariables in pocket.yaml, and ${containerStep}. Keep metadata.id in pocket.yaml unchanged and record a one-line summary in POCKETS_CHANGES.md when done.`;
}

function ProjectTitleFavicon({
  projectId,
  cacheKey,
}: {
  projectId: string;
  cacheKey?: string;
}) {
  const src = cacheKey
    ? `/api/projects/${encodeURIComponent(projectId)}/favicon?v=${encodeURIComponent(cacheKey)}`
    : `/api/projects/${encodeURIComponent(projectId)}/favicon`;
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
      className="external-workspace-favicon"
      src={src}
      alt=""
      aria-hidden="true"
      data-loaded={status === "loaded" ? "true" : undefined}
      onLoad={() => setLoadState({ src, status: "loaded" })}
      onError={() => setLoadState({ src, status: "missing" })}
    />
  );
}

export default function ExternalWorkspace({
  projectId,
  projectName,
  faviconCacheKey,
  linked,
  containerfile,
  projectFiles,
  pocketizeSource,
  onDismissPocketizeSource,
  previewApiAccess,
  previewApis,
  building,
  sidebarCollapsed,
  previewCollapsed,
  onToggleSidebar,
  onTogglePreview,
  onSourceChanged,
  controlsRef,
}: {
  projectId: string;
  projectName: string;
  faviconCacheKey?: string;
  linked: boolean;
  containerfile?: string;
  projectFiles?: string[];
  pocketizeSource?: PocketizeSourceInfo;
  onDismissPocketizeSource?: () => void;
  previewApiAccess: PreviewApiAccess;
  previewApis?: PreviewApi[];
  building: boolean;
  sidebarCollapsed: boolean;
  previewCollapsed: boolean;
  onToggleSidebar: () => void;
  onTogglePreview: () => void;
  onSourceChanged: () => void;
  controlsRef: (element: HTMLDivElement | null) => void;
}) {
  const [logs, setLogs] = useState<string | null>(null);
  const [logError, setLogError] = useState<string | null>(null);
  const [loadingLogs, setLoadingLogs] = useState(false);
  const [promptCopied, setPromptCopied] = useState(false);
  const [promptCopyError, setPromptCopyError] = useState<string | null>(null);
  const promptCopyTimer = useRef<number | null>(null);

  useEffect(() => {
    setPromptCopied(false);
    setPromptCopyError(null);
    return () => {
      if (promptCopyTimer.current !== null) {
        window.clearTimeout(promptCopyTimer.current);
        promptCopyTimer.current = null;
      }
    };
  }, [projectId]);

  const isCleanRoomPocketize = Boolean(pocketizeSource);
  const isInPlacePlaceholder =
    !isCleanRoomPocketize &&
    linked &&
    typeof containerfile === "string" &&
    containerfile.includes(PLACEHOLDER_CONTAINERFILE_MARKER);
  const pocketizePrompt = pocketizeSource
    ? buildCleanRoomPocketizePrompt(projectName, pocketizeSource)
    : isInPlacePlaceholder
      ? buildPocketizePrompt(projectName, projectFiles)
      : null;

  async function copyPocketizePrompt() {
    if (!pocketizePrompt) return;
    setPromptCopyError(null);
    try {
      await navigator.clipboard.writeText(pocketizePrompt);
      setPromptCopied(true);
      if (promptCopyTimer.current !== null) {
        window.clearTimeout(promptCopyTimer.current);
      }
      promptCopyTimer.current = window.setTimeout(() => {
        promptCopyTimer.current = null;
        setPromptCopied(false);
      }, 2000);
    } catch {
      setPromptCopyError(
        "Could not access clipboard automatically — select and copy the prompt above.",
      );
    }
  }
  async function readLogs() {
    setLoadingLogs(true);
    setLogError(null);
    try {
      setLogs(await invoke<string>("project_preview_logs", { projectId }));
    } catch (error) {
      setLogError(String(error));
    } finally {
      setLoadingLogs(false);
    }
  }

  return (
    <div className="external-workspace">
      <TitleBar
        ariaLabel="Development"
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
        title={
          <span className="external-workspace-title">
            <ProjectTitleFavicon
              projectId={projectId}
              cacheKey={faviconCacheKey}
            />
            <span className="external-workspace-title-text">{projectName}</span>
          </span>
        }
        trailing={
          previewCollapsed ? (
            <IconButton label="Toggle preview" onClick={onTogglePreview}>
              <IconPanelRight />
            </IconButton>
          ) : undefined
        }
      />
      <div className="external-workspace-content">
        {pocketizePrompt && (
          <section
            className="external-workspace-pocketize"
            aria-label="Finish pocketizing"
          >
            <div className="external-workspace-pocketize-header">
              <h2 className="external-workspace-pocketize-title">
                Finish pocketizing in your coding tool
              </h2>
              {isCleanRoomPocketize && onDismissPocketizeSource && (
                <IconButton
                  label="Dismiss pocketize prompt"
                  className="external-workspace-pocketize-dismiss"
                  onClick={onDismissPocketizeSource}
                >
                  <IconClose size={14} />
                </IconButton>
              )}
            </div>
            {pocketizeSource ? (
              pocketizeSource.targetSubfolder ? (
                <p className="hint">
                  Keep <code>{pocketizeSource.folderName}</code> open in your
                  coding tool and paste this prompt to build the Pocket inside{" "}
                  <code>{pocketizeSource.targetSubfolder}/</code> without
                  touching your existing files.
                </p>
              ) : (
                <p className="hint">
                  Open this new Pocket folder in your coding tool and paste this
                  prompt so it recreates{" "}
                  <code>{pocketizeSource.folderName}</code> here without
                  touching the original project.
                </p>
              )
            ) : (
              <p className="hint">
                Paste this prompt into your coding tool so it wires up{" "}
                <code>/health</code>, <code>/data</code>, and{" "}
                <code>Containerfile</code> for your stack.
              </p>
            )}
            <pre className="external-workspace-pocketize-prompt">
              {pocketizePrompt}
            </pre>
            <div className="external-workspace-pocketize-actions">
              <button
                type="button"
                className="button-primary external-workspace-pocketize-copy"
                onClick={() => void copyPocketizePrompt()}
              >
                {promptCopied ? (
                  <>
                    <IconCheck size={14} />
                    Copied prompt
                  </>
                ) : (
                  <>
                    <IconCopy size={14} />
                    Copy AI prompt
                  </>
                )}
              </button>
            </div>
            {promptCopyError && (
              <p role="alert" className="error">
                {promptCopyError}
              </p>
            )}
            {pocketizeSource ? (
              <p className="hint">
                Start <strong>Preview</strong> to watch live edits as your
                coding tool builds it, then click{" "}
                <strong>Build &amp; start</strong> when ready.
              </p>
            ) : (
              <p className="hint">
                Once finished, click <strong>Build &amp; start</strong> below.
                Live Preview is for dependency-free root <code>server.js</code>{" "}
                apps.
              </p>
            )}
          </section>
        )}
        <div className="external-workspace-section">
          <h2>Develop in your coding tool</h2>
          <DevelopmentPanel
            projectId={projectId}
            disabled={building}
            onSourceChanged={onSourceChanged}
            initiallyExpanded
          />
        </div>
        <div
          ref={controlsRef}
          className="external-workspace-install"
          role="region"
          aria-label="Installation and source history"
        />
        <div className="external-workspace-section">
          <h3 className="external-workspace-subheading">
            Preview &amp; runtime configuration
          </h3>
          {linked && (
            <>
              <p className="hint">
                Preview runs dependency-free Node apps with isolated test data.
                Stop discards preview data.
              </p>
              <PreviewNetworkSettings
                projectId={projectId}
                access={previewApiAccess}
                previewApis={previewApis}
              />
              <details className="pane-details">
                <summary>Preview logs</summary>
                <div className="preview-log-content" aria-busy={loadingLogs}>
                  <button
                    type="button"
                    disabled={loadingLogs}
                    onClick={() => void readLogs()}
                  >
                    {loadingLogs
                      ? "Reading…"
                      : logs === null
                        ? "Read logs"
                        : "Refresh logs"}
                  </button>
                  {logError && (
                    <p role="alert" className="error">
                      {logError}
                    </p>
                  )}
                  {logs !== null &&
                    (logs.length === 0 ? (
                      <p className="hint">No preview output is available.</p>
                    ) : (
                      <pre className="creation-build-log">{logs}</pre>
                    ))}
                </div>
              </details>
            </>
          )}
          <RuntimeSettings
            projectId={projectId}
            disabled={building}
            defaultEnvironment={linked ? "preview" : "installed"}
          />
          <HubSubmissionPanel projectId={projectId} />
        </div>
      </div>
    </div>
  );
}
