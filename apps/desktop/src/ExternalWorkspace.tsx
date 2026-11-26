import { useState } from "react";
import { invoke } from "./api";
import DevelopmentPanel from "./DevelopmentPanel";
import HubSubmissionPanel from "./HubSubmissionPanel";
import RuntimeSettings from "./RuntimeSettings";
import PreviewNetworkSettings from "./PreviewNetworkSettings";
import type { PreviewApiAccess } from "./useProjectPreview";
import type { PreviewApi } from "./CreationPanel";
import { TitleBar } from "./TitleBar";
import { IconButton } from "./Status";
import { IconPanelLeft, IconPanelRight } from "./Icons";
import "./ExternalWorkspace.css";

export default function ExternalWorkspace({
  projectId,
  projectName,
  linked,
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
  linked: boolean;
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
        title={<span>{projectName}</span>}
        trailing={
          previewCollapsed ? (
            <IconButton label="Toggle preview" onClick={onTogglePreview}>
              <IconPanelRight />
            </IconButton>
          ) : undefined
        }
      />
      <div className="external-workspace-content">
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
