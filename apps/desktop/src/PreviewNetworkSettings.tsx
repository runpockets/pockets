import { useEffect, useId, useRef, useState } from "react";
import { invoke } from "./api";
import type { PreviewApi } from "./CreationPanel";
import type { PreviewApiAccess } from "./useProjectPreview";

interface PreviewNetwork {
  hosts: string[];
  previewApis?: PreviewApi[];
}

export default function PreviewNetworkSettings({
  projectId,
  access,
  previewApis,
}: {
  projectId: string;
  access?: PreviewApiAccess;
  previewApis?: PreviewApi[];
}) {
  const [open, setOpen] = useState(false);
  // How many hostnames are saved, once the list has been read; shown on the
  // right of the header so the count is visible with the section closed.
  const [allowedCount, setAllowedCount] = useState<number | null>(null);
  const section = useRef<HTMLDetailsElement>(null);
  const summary = useRef<HTMLElement>(null);
  const review = access?.review ?? null;
  const reviewToken = review?.token;

  useEffect(() => {
    const details = section.current;
    if (!reviewToken || !details) return;
    details.open = true;
    setOpen(true);
    details.scrollIntoView?.({ block: "nearest" });
    summary.current?.focus({ preventScroll: true });
  }, [reviewToken]);

  return (
    <details
      ref={section}
      className="preview-network"
      data-approval-needed={review !== null || undefined}
      onToggle={(event) => setOpen(event.currentTarget.open)}
    >
      <summary ref={summary}>
        Preview API access
        <span className="preview-network-notice" aria-live="polite">
          {review && "Access requested"}
        </span>
        {allowedCount !== null && (
          <span className="preview-network-count">{allowedCount} allowed</span>
        )}
      </summary>
      {open && (
        <>
          {review && access && (
            <div
              className="runtime-settings preview-network-review"
              aria-busy={access.busy}
            >
              <p>
                This Pocket requests GET and HEAD access to these HTTPS
                services.
              </p>
              <ul>
                {review.services.map((service) => (
                  <li key={service.host}>
                    <strong>{service.name}</strong>
                    <code>{service.host}</code>
                    <p>{service.purpose}</p>
                  </li>
                ))}
              </ul>
              <p className="hint">
                Allow saves access for this Pocket. Ignore dismisses the request
                for this preview session.
              </p>
              <div className="preview-network-actions">
                <button
                  type="button"
                  className="preview-network-primary"
                  disabled={access.busy}
                  onClick={() => void access.decide(true)}
                >
                  Allow
                </button>
                <button
                  type="button"
                  disabled={access.busy}
                  onClick={() => void access.decide(false)}
                >
                  Ignore
                </button>
              </div>
            </div>
          )}
          {access?.error && (
            <p role="alert" className="error preview-network-error">
              {access.error}
            </p>
          )}
        </>
      )}
      <AllowedHosts
        key={projectId}
        projectId={projectId}
        open={open}
        reviewToken={reviewToken}
        canEdit={access?.canEditHosts ?? true}
        previewApis={previewApis}
        onCount={setAllowedCount}
      />
    </details>
  );
}

function AllowedHosts({
  projectId,
  open,
  reviewToken,
  canEdit,
  previewApis,
  onCount,
}: {
  projectId: string;
  open: boolean;
  reviewToken: string | undefined;
  canEdit: boolean;
  previewApis?: PreviewApi[];
  onCount?: (count: number) => void;
}) {
  const [hosts, setHosts] = useState<string[] | null>(null);
  const [declaredApis, setDeclaredApis] = useState<PreviewApi[] | null>(null);
  const [input, setInput] = useState("");
  const [loadError, setLoadError] = useState<string | null>(null);
  const [editError, setEditError] = useState<string | null>(null);
  const [pending, setPending] = useState(false);
  const duplicateId = useId();
  const inputId = useId();

  useEffect(() => {
    if (hosts !== null) onCount?.(hosts.length);
  }, [hosts, onCount]);

  useEffect(() => {
    if (!open || pending) return;
    let current = true;
    void invoke<PreviewNetwork>("project_preview_network", { projectId })
      .then((saved) => {
        if (!current) return;
        setHosts(saved.hosts);
        setDeclaredApis(saved.previewApis ?? null);
        setLoadError(null);
      })
      .catch((failure: unknown) => {
        if (current) setLoadError(String(failure));
      });
    return () => {
      current = false;
    };
  }, [projectId, open, pending, reviewToken]);

  const editable = canEdit && hosts !== null && !loadError && !pending;
  const hostname = input.trim();
  const duplicate = hosts?.includes(hostname.toLowerCase()) ?? false;
  const apisByHost = new Map<string, PreviewApi>();
  for (const api of declaredApis ?? previewApis ?? []) {
    apisByHost.set(api.host.trim().toLowerCase(), api);
  }

  async function updateHosts(next: string[], clearInput = false) {
    if (!editable) return;
    setPending(true);
    setEditError(null);
    try {
      const saved = await invoke<string[]>("set_project_preview_network", {
        projectId,
        hosts: next,
      });
      setHosts(saved);
      if (clearInput) setInput("");
    } catch (failure) {
      setEditError(String(failure));
    } finally {
      setPending(false);
    }
  }

  if (!open) return null;
  const error = editError ?? loadError;
  return (
    <div className="runtime-settings preview-network-saved" aria-busy={pending}>
      <p className="hint preview-network-intro">
        The preview can make read-only HTTPS requests (GET and HEAD) to these
        hosts. Everything else is blocked.
      </p>
      {hosts === null && !loadError && (
        <p role="status" className="hint">
          Reading preview access…
        </p>
      )}
      {hosts && (
        <>
          <ul className="preview-network-hosts" aria-label="Allowed hostnames">
            {hosts.length === 0 && (
              <li className="hint preview-network-empty">
                No hostnames yet. Requests from the preview are all blocked.
              </li>
            )}
            {hosts.map((host) => {
              const declared = apisByHost.get(host.toLowerCase());
              return (
                <li key={host}>
                  <div className="preview-network-host-info">
                    {declared ? (
                      <>
                        <span className="preview-network-host-name">
                          {declared.name}
                        </span>
                        <code className="preview-network-host-matched">
                          {host}
                        </code>
                        {declared.purpose.trim() && (
                          <p className="preview-network-host-purpose">
                            {declared.purpose}
                          </p>
                        )}
                      </>
                    ) : (
                      <>
                        <code className="preview-network-host-name">
                          {host}
                        </code>
                        <span className="preview-network-host-manual">
                          Added manually
                        </span>
                      </>
                    )}
                  </div>
                  <button
                    type="button"
                    className="preview-network-remove"
                    aria-label={`Remove ${host}`}
                    title={`Remove ${host}`}
                    disabled={!editable}
                    onClick={() =>
                      void updateHosts(hosts.filter((saved) => saved !== host))
                    }
                  >
                    <svg
                      width="14"
                      height="14"
                      viewBox="0 0 24 24"
                      fill="none"
                      stroke="currentColor"
                      strokeWidth="2"
                      aria-hidden="true"
                    >
                      <path d="M6 6l12 12M18 6L6 18" />
                    </svg>
                  </button>
                </li>
              );
            })}
          </ul>
          {!canEdit && (
            <p className="hint">
              Stop Preview to add or remove saved hostnames.
            </p>
          )}
          <form
            className="preview-network-manual"
            onSubmit={(event) => {
              event.preventDefault();
              if (hostname && !duplicate)
                void updateHosts([...hosts, hostname], true);
            }}
          >
            <label htmlFor={inputId}>Add a hostname</label>
            <div className="preview-network-manual-row">
              <input
                id={inputId}
                type="text"
                maxLength={253}
                placeholder="api.example.com"
                autoComplete="off"
                autoCapitalize="none"
                spellCheck={false}
                value={input}
                disabled={!editable}
                aria-describedby={duplicate ? duplicateId : undefined}
                onChange={(event) => setInput(event.currentTarget.value)}
              />
              <button
                type="submit"
                className="preview-network-primary"
                aria-label="Add hostname"
                disabled={!editable || !hostname || duplicate}
              >
                Add
              </button>
            </div>
            {duplicate && (
              <p id={duplicateId} className="hint">
                This hostname is already allowed.
              </p>
            )}
          </form>
        </>
      )}
      {error && (
        <p role="alert" className="error">
          {error}
        </p>
      )}
    </div>
  );
}
