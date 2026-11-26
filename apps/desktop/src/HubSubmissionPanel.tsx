import { useCallback, useEffect, useRef, useState } from "react";
import { invoke } from "./api";
import { IconCheck, IconCopy, IconExternalLink } from "./Icons";
import { StatusPip, type PipTone } from "./Status";
import "./HubSubmissionPanel.css";

export interface SubmissionCheck {
  id: string;
  label: string;
  status: "pass" | "warn" | "fail" | "pending" | string;
  detail: string;
}

export interface HubSubmissionReport {
  projectId: string;
  pocketId: string;
  pocketName: string;
  detectedRepo?: string | null;
  resolvedRepo?: string | null;
  alreadyInHub: boolean;
  readyToSubmit: boolean;
  remoteVerified: boolean;
  checks: SubmissionCheck[];
  catalogEntryJson?: string | null;
  prEditUrl: string;
  issueSubmitUrl: string;
}

function checkTone(status: string): PipTone {
  if (status === "pass") return "ok";
  if (status === "pending") return "stopped";
  return "warn";
}

function checkWord(status: string): string {
  if (status === "pass") return "Pass";
  if (status === "warn") return "Note";
  if (status === "fail") return "Required";
  return "Pending";
}

export default function HubSubmissionPanel({
  projectId,
}: {
  projectId: string;
}) {
  const [expanded, setExpanded] = useState(false);
  const [repoInput, setRepoInput] = useState("");
  const [repoDirty, setRepoDirty] = useState(false);
  const [report, setReport] = useState<HubSubmissionReport | null>(null);
  const [loading, setLoading] = useState(false);
  const [verifying, setVerifying] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const [copied, setCopied] = useState(false);
  const copiedTimer = useRef<number | null>(null);
  const epoch = useRef(0);

  useEffect(() => {
    epoch.current++;
    setExpanded(false);
    setRepoInput("");
    setRepoDirty(false);
    setReport(null);
    setLoading(false);
    setVerifying(false);
    setError(null);
    setCopied(false);
    return () => {
      if (copiedTimer.current !== null) {
        window.clearTimeout(copiedTimer.current);
        copiedTimer.current = null;
      }
    };
  }, [projectId]);

  const runCheck = useCallback(
    async (repoOverride: string | undefined, verifyRemote: boolean) => {
      const current = ++epoch.current;
      if (verifyRemote) setVerifying(true);
      else setLoading(true);
      setError(null);
      try {
        const next = await invoke<HubSubmissionReport>("check_hub_submission", {
          id: projectId,
          repo: repoOverride?.trim() || undefined,
          verifyRemote,
        });
        if (epoch.current !== current) return;
        setReport(next);
        if (!repoOverride && next.detectedRepo) {
          setRepoInput((prev) => (prev === "" ? next.detectedRepo! : prev));
        }
      } catch (failure) {
        if (epoch.current === current) setError(String(failure));
      } finally {
        if (epoch.current === current) {
          setLoading(false);
          setVerifying(false);
        }
      }
    },
    [projectId],
  );

  useEffect(() => {
    if (!expanded) return;
    void runCheck(repoDirty ? repoInput : undefined, false);
    // Run local check once on expand; manual verify runs via button.
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [expanded, projectId]);

  async function copyEntry() {
    if (!report?.catalogEntryJson) return;
    try {
      await navigator.clipboard?.writeText(report.catalogEntryJson);
      setCopied(true);
      if (copiedTimer.current !== null) {
        window.clearTimeout(copiedTimer.current);
      }
      copiedTimer.current = window.setTimeout(() => {
        copiedTimer.current = null;
        setCopied(false);
      }, 2000);
    } catch {
      // Ignore clipboard denials in restricted browser contexts.
    }
  }

  function openExternal(url: string) {
    window.open(url, "_blank", "noopener,noreferrer");
  }

  const canOpenPr = Boolean(
    report && report.readyToSubmit && report.remoteVerified && !verifying,
  );

  return (
    <details
      open={expanded}
      className="pane-details hub-submission"
      onToggle={(event) => setExpanded(event.currentTarget.open)}
    >
      <summary>Submit to Pockets Hub</summary>
      {expanded && (
        <div
          className="hub-submission-content"
          aria-busy={loading || verifying}
        >
          <p className="hint">
            Publish this Pocket in your own public GitHub repository, verify it
            against Pockets Hub rules, and open a pull request to{" "}
            <code>runpockets/pockets-hub</code>.
          </p>

          {loading && report === null && (
            <p className="hint">Running submission readiness checks…</p>
          )}

          {error && (
            <p role="alert" className="error">
              {error}
            </p>
          )}

          {report?.alreadyInHub ? (
            <div className="hub-submission-listed">
              <StatusPip tone="ok" word="Listed in Pockets Hub" />
              <p className="hint">
                This Pocket is already in the Pockets Hub catalog. Push updates
                to your repository to update its description and version
                automatically.
              </p>
            </div>
          ) : report ? (
            <>
              <form
                className="hub-submission-repo-form"
                onSubmit={(event) => {
                  event.preventDefault();
                  void runCheck(repoInput, true);
                }}
              >
                <label className="hub-submission-label">
                  Public GitHub repository
                  <div className="hub-submission-repo-row">
                    <input
                      type="text"
                      value={repoInput}
                      disabled={loading || verifying}
                      placeholder="owner/repo"
                      onChange={(event) => {
                        setRepoDirty(true);
                        setRepoInput(event.target.value);
                      }}
                    />
                    <button
                      type="submit"
                      disabled={
                        loading || verifying || repoInput.trim() === ""
                      }
                    >
                      {verifying ? "Verifying…" : "Verify GitHub repo"}
                    </button>
                  </div>
                </label>
              </form>

              <ul
                className="hub-submission-checks"
                aria-label="Hub submission checks"
              >
                {report.checks.map((check) => (
                  <li
                    key={check.id}
                    className="hub-submission-check"
                    data-status={check.status}
                  >
                    <div className="hub-submission-check-head">
                      <span className="hub-submission-check-label">
                        {check.label}
                      </span>
                      <StatusPip
                        tone={checkTone(check.status)}
                        word={checkWord(check.status)}
                      />
                    </div>
                    <p className="hub-submission-check-detail">
                      {check.detail}
                    </p>
                  </li>
                ))}
              </ul>

              {report.catalogEntryJson && (
                <div className="hub-submission-snippet">
                  <div className="hub-submission-snippet-head">
                    <span className="hub-submission-snippet-title">
                      Entry for <code>catalog.json</code>
                    </span>
                    <button
                      type="button"
                      className="hub-submission-copy"
                      onClick={() => void copyEntry()}
                    >
                      {copied ? (
                        <>
                          <IconCheck size={12} className="tone-live" /> Copied
                        </>
                      ) : (
                        <>
                          <IconCopy size={12} /> Copy entry
                        </>
                      )}
                    </button>
                  </div>
                  <pre className="hub-submission-code">
                    {report.catalogEntryJson}
                  </pre>
                </div>
              )}

              <div className="hub-submission-actions">
                <button
                  type="button"
                  className="button-primary"
                  disabled={!canOpenPr}
                  onClick={() => openExternal(report.prEditUrl)}
                >
                  Open PR on GitHub <IconExternalLink size={12} />
                </button>
                <button
                  type="button"
                  disabled={!canOpenPr}
                  onClick={() => openExternal(report.issueSubmitUrl)}
                >
                  Submit via Issue form <IconExternalLink size={12} />
                </button>
              </div>
              {!canOpenPr && (
                <p className="hint">
                  Pass every required check above and click{" "}
                  <strong>Verify GitHub repo</strong> to enable submission.
                </p>
              )}
            </>
          ) : null}
        </div>
      )}
    </details>
  );
}
