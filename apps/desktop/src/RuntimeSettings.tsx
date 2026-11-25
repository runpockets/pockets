import { useEffect, useRef, useState, type FormEvent } from "react";
import { invoke } from "./api";
import { Segmented } from "./Status";

type Environment = "preview" | "installed";
type Kind = "plain" | "secret";
type Variable =
  | { name: string; kind: "plain"; value: string }
  | {
      name: string;
      kind: "secret";
    };
type DeclaredVariable = {
  name: string;
  kind: Kind;
  purpose: string;
  required?: boolean;
};
type RuntimeSettingsSnapshot = {
  variables: Variable[];
  declared?: DeclaredVariable[];
  detected?: string[];
};
type Change =
  | { action: "set"; name: string; kind: Kind; value: string; replace: boolean }
  | { action: "remove"; name: string };
type Draft =
  | { mode: "replace"; variable: Variable }
  | { mode: "setDeclared"; declared: DeclaredVariable }
  | { mode: "prefill"; name: string };
type SummaryCounts = {
  savedCount: number;
  missingRequired: number;
};

export default function RuntimeSettings({
  projectId,
  disabled,
}: {
  projectId: string;
  disabled: boolean;
}) {
  const [open, setOpen] = useState(false);
  const [environment, setEnvironment] = useState<Environment>("preview");
  const [busy, setBusy] = useState(false);
  const [summary, setSummary] = useState<SummaryCounts | null>(null);

  useEffect(() => {
    setSummary(null);
  }, [projectId, environment]);

  return (
    <details
      className="pane-details runtime-settings-details"
      data-missing-required={(summary?.missingRequired ?? 0) > 0 || undefined}
      onToggle={(event) => setOpen(event.currentTarget.open)}
    >
      <summary>
        <span>Runtime variables</span>
        {summary && summary.missingRequired > 0 && (
          <span className="runtime-settings-notice" aria-live="polite">
            {summary.missingRequired} required
          </span>
        )}
        {summary && (
          <span className="runtime-settings-count">
            {summary.savedCount} saved
          </span>
        )}
      </summary>
      {open && (
        <div className="runtime-settings">
          <p className="hint">
            Preview and Installed app have separate values. Start the preview
            from Pockets to use its saved values. Stop and start either
            environment to apply changes.
          </p>
          {environment === "preview" && (
            <p className="hint">
              Preview code can read these values during live reload. Use
              development credentials.
            </p>
          )}
          <div className="runtime-settings-field">
            <span className="runtime-settings-label">Environment</span>
            <Segmented
              ariaLabel="Environment"
              value={environment}
              onChange={(id) => {
                if (!busy && (id === "preview" || id === "installed")) {
                  setEnvironment(id);
                }
              }}
              items={[
                { id: "preview", label: "Preview" },
                { id: "installed", label: "Installed app" },
              ]}
            />
          </div>
          <Variables
            key={`${projectId}/${environment}`}
            projectId={projectId}
            environment={environment}
            disabled={disabled || busy}
            setBusy={setBusy}
            onSummary={setSummary}
          />
        </div>
      )}
    </details>
  );
}

function Variables({
  projectId,
  environment,
  disabled,
  setBusy,
  onSummary,
}: {
  projectId: string;
  environment: Environment;
  disabled: boolean;
  setBusy: (busy: boolean) => void;
  onSummary: (summary: SummaryCounts) => void;
}) {
  const [snapshot, setSnapshot] = useState<RuntimeSettingsSnapshot | null>(
    null,
  );
  const [attempt, retry] = useState(0);
  const [error, setError] = useState<string | null>(null);
  const [notice, setNotice] = useState<string | null>(null);
  const [draft, setDraft] = useState<Draft | null>(null);
  const [kind, setKind] = useState<Kind>("secret");
  const [formVersion, resetForm] = useState(0);
  const valueInputRef = useRef<HTMLInputElement>(null);

  function applySnapshot(result: RuntimeSettingsSnapshot) {
    setSnapshot(result);
    const savedNames = new Set(result.variables.map((v) => v.name));
    const missingRequired = (result.declared ?? []).filter(
      (decl) => decl.required && !savedNames.has(decl.name),
    ).length;
    onSummary({
      savedCount: result.variables.length,
      missingRequired,
    });
  }

  useEffect(() => {
    let current = true;
    setError(null);
    void invoke<RuntimeSettingsSnapshot>("project_runtime_settings", {
      id: projectId,
      environment,
    })
      .then((result) => {
        if (current) applySnapshot(result);
      })
      .catch((failure: unknown) => {
        if (current) setError(String(failure));
      });
    return () => {
      current = false;
    };
  }, [projectId, environment, attempt]);

  const variables = snapshot?.variables ?? null;
  const declared = snapshot?.declared ?? [];
  const detected = snapshot?.detected ?? [];

  useEffect(() => {
    if (draft !== null) {
      valueInputRef.current?.focus();
    }
  }, [draft, formVersion]);

  function startDraft(next: Draft | null) {
    setDraft(next);
    if (next?.mode === "replace") {
      setKind(next.variable.kind);
    } else if (next?.mode === "setDeclared") {
      setKind(next.declared.kind);
    } else {
      setKind("secret");
    }
    resetForm((version) => version + 1);
  }

  async function changeVariable(change: Change) {
    setBusy(true);
    setError(null);
    setNotice(null);
    try {
      const result = await invoke<RuntimeSettingsSnapshot>(
        "change_project_runtime_settings",
        {
          id: projectId,
          environment,
          change,
        },
      );
      applySnapshot(result);
      const draftName =
        draft?.mode === "replace"
          ? draft.variable.name
          : draft?.mode === "setDeclared"
            ? draft.declared.name
            : draft?.mode === "prefill"
              ? draft.name
              : null;
      if (change.action === "set" || change.name === draftName) {
        startDraft(null);
      }
      setNotice(
        change.action === "set" ? "Variable saved." : "Variable removed.",
      );
    } catch (failure) {
      setError(String(failure));
    } finally {
      setBusy(false);
    }
  }

  function save(event: FormEvent<HTMLFormElement>) {
    event.preventDefault();
    if (disabled) return;
    const fields = new FormData(event.currentTarget);
    const value = String(fields.get("value") ?? "");
    if (kind === "secret") {
      const input = event.currentTarget.elements.namedItem(
        "value",
      ) as HTMLInputElement;
      input.value = "";
    }
    void changeVariable({
      action: "set",
      name: String(fields.get("name") ?? "").trim(),
      kind,
      value,
      replace: draft?.mode === "replace",
    });
  }

  const savedByName = new Map<string, Variable>();
  for (const variable of variables ?? []) {
    savedByName.set(variable.name, variable);
  }
  const declaredNames = new Set(declared.map((decl) => decl.name));
  const manualVariables = (variables ?? []).filter(
    (variable) => !declaredNames.has(variable.name),
  );
  const suggestions = detected.filter(
    (name) => !savedByName.has(name) && !declaredNames.has(name),
  );

  const lockedName =
    draft?.mode === "replace"
      ? draft.variable.name
      : draft?.mode === "setDeclared"
        ? draft.declared.name
        : null;
  const defaultName =
    lockedName ?? (draft?.mode === "prefill" ? draft.name : undefined);
  const submitLabel =
    draft?.mode === "replace"
      ? "Save replacement"
      : draft?.mode === "setDeclared"
        ? "Save variable"
        : "Add variable";

  return (
    <>
      {error && (
        <p className="error" role="alert">
          {error}
        </p>
      )}
      {notice && (
        <p className="hint" role="status">
          {notice}
        </p>
      )}
      {variables === null ? (
        error ? (
          <button type="button" onClick={() => retry((count) => count + 1)}>
            Retry
          </button>
        ) : (
          <p className="hint" role="status">
            Reading runtime variables…
          </p>
        )
      ) : (
        <>
          {declared.length > 0 || manualVariables.length > 0 ? (
            <ul className="runtime-variables">
              {declared.map((decl) => {
                const saved = savedByName.get(decl.name);
                return (
                  <li key={decl.name} className="runtime-variable-row">
                    <div className="runtime-variable-info">
                      <div className="runtime-variable-meta">
                        <code>{decl.name}</code>
                        <span
                          className={
                            decl.required
                              ? "runtime-variable-tag runtime-variable-tag-required"
                              : "runtime-variable-tag"
                          }
                        >
                          {decl.required ? "Required" : "Optional"}
                        </span>
                        {saved ? (
                          saved.kind === "secret" ? (
                            <span className="hint">Secret saved</span>
                          ) : (
                            <span className="runtime-variable-value">
                              = <code>{saved.value}</code>
                            </span>
                          )
                        ) : (
                          <span
                            className={
                              decl.required
                                ? "runtime-variable-missing"
                                : "hint"
                            }
                          >
                            Not set
                          </span>
                        )}
                      </div>
                      <p className="runtime-variable-purpose">{decl.purpose}</p>
                    </div>
                    <div className="actions">
                      {saved ? (
                        <>
                          <button
                            type="button"
                            aria-label={`Replace ${decl.name}`}
                            disabled={disabled}
                            onClick={() =>
                              startDraft({ mode: "replace", variable: saved })
                            }
                          >
                            Replace
                          </button>
                          <button
                            type="button"
                            aria-label={`Remove ${decl.name}`}
                            disabled={disabled}
                            onClick={() =>
                              void changeVariable({
                                action: "remove",
                                name: decl.name,
                              })
                            }
                          >
                            Remove
                          </button>
                        </>
                      ) : (
                        <button
                          type="button"
                          aria-label={`Set ${decl.name}`}
                          disabled={disabled}
                          onClick={() =>
                            startDraft({ mode: "setDeclared", declared: decl })
                          }
                        >
                          Set
                        </button>
                      )}
                    </div>
                  </li>
                );
              })}
              {manualVariables.map((variable) => (
                <li key={variable.name} className="runtime-variable-row">
                  <div className="runtime-variable-info">
                    <div className="runtime-variable-meta">
                      <code>{variable.name}</code>
                      {variable.kind === "secret" ? (
                        <span className="hint">Secret saved</span>
                      ) : (
                        <span className="runtime-variable-value">
                          = <code>{variable.value}</code>
                        </span>
                      )}
                      {declared.length > 0 && (
                        <span className="runtime-variable-tag">
                          Added manually
                        </span>
                      )}
                    </div>
                  </div>
                  <div className="actions">
                    <button
                      type="button"
                      aria-label={`Replace ${variable.name}`}
                      disabled={disabled}
                      onClick={() => startDraft({ mode: "replace", variable })}
                    >
                      Replace
                    </button>
                    <button
                      type="button"
                      aria-label={`Remove ${variable.name}`}
                      disabled={disabled}
                      onClick={() =>
                        void changeVariable({
                          action: "remove",
                          name: variable.name,
                        })
                      }
                    >
                      Remove
                    </button>
                  </div>
                </li>
              ))}
            </ul>
          ) : (
            <p className="hint">No variables saved for this environment.</p>
          )}
          {suggestions.length > 0 && (
            <div className="runtime-settings-field">
              <span className="runtime-settings-label">
                Detected in source (not declared in <code>pocket.yaml</code>)
              </span>
              <div
                className="runtime-detected-list"
                role="group"
                aria-label="Detected in source"
              >
                {suggestions.map((name) => (
                  <button
                    key={name}
                    type="button"
                    className="runtime-detected-chip"
                    disabled={disabled}
                    aria-label={`Use detected variable ${name}`}
                    onClick={() => startDraft({ mode: "prefill", name })}
                  >
                    + <code>{name}</code>
                  </button>
                ))}
              </div>
            </div>
          )}
          <form
            key={formVersion}
            className="runtime-variable-form"
            autoComplete="off"
            onSubmit={save}
          >
            <label>
              Variable name
              <input
                name="name"
                required
                maxLength={128}
                placeholder="PRICING_API_KEY"
                defaultValue={defaultName}
                readOnly={lockedName !== null}
                disabled={disabled}
              />
            </label>
            <div className="runtime-settings-field">
              <span className="runtime-settings-label">Type</span>
              <Segmented
                ariaLabel="Type"
                value={kind}
                onChange={(id) => {
                  if (!disabled && (id === "secret" || id === "plain")) {
                    setKind(id);
                  }
                }}
                items={[
                  { id: "secret", label: "Secret" },
                  { id: "plain", label: "Plain text" },
                ]}
              />
            </div>
            <label>
              {kind === "secret" ? "Secret value" : "Value"}
              <input
                ref={valueInputRef}
                key={kind}
                name="value"
                type={kind === "secret" ? "password" : "text"}
                required={kind === "secret"}
                maxLength={4096}
                disabled={disabled}
                defaultValue={
                  kind === "plain" &&
                  draft?.mode === "replace" &&
                  draft.variable.kind === "plain"
                    ? draft.variable.value
                    : undefined
                }
              />
            </label>
            <div className="actions">
              <button
                type="submit"
                className="button-primary"
                disabled={disabled}
              >
                {submitLabel}
              </button>
              {lockedName !== null && (
                <button
                  type="button"
                  disabled={disabled}
                  onClick={() => startDraft(null)}
                >
                  Cancel
                </button>
              )}
            </div>
          </form>
        </>
      )}
    </>
  );
}
