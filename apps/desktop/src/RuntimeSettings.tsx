import { useEffect, useState, type FormEvent } from "react";
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
type Change =
  | { action: "set"; name: string; kind: Kind; value: string; replace: boolean }
  | { action: "remove"; name: string };

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

  return (
    <details
      className="pane-details"
      onToggle={(event) => setOpen(event.currentTarget.open)}
    >
      <summary>Runtime variables</summary>
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
}: {
  projectId: string;
  environment: Environment;
  disabled: boolean;
  setBusy: (busy: boolean) => void;
}) {
  const [variables, setVariables] = useState<Variable[] | null>(null);
  const [attempt, retry] = useState(0);
  const [error, setError] = useState<string | null>(null);
  const [notice, setNotice] = useState<string | null>(null);
  const [editing, setEditing] = useState<Variable | null>(null);
  const [kind, setKind] = useState<Kind>("secret");
  const [formVersion, resetForm] = useState(0);

  useEffect(() => {
    let current = true;
    setError(null);
    void invoke<Variable[]>("project_runtime_settings", {
      id: projectId,
      environment,
    })
      .then((result) => {
        if (current) setVariables(result);
      })
      .catch((failure: unknown) => {
        if (current) setError(String(failure));
      });
    return () => {
      current = false;
    };
  }, [projectId, environment, attempt]);

  function edit(variable: Variable | null) {
    setEditing(variable);
    setKind(variable?.kind ?? "secret");
    resetForm((version) => version + 1);
  }

  async function changeVariable(change: Change) {
    setBusy(true);
    setError(null);
    setNotice(null);
    try {
      const result = await invoke<Variable[]>(
        "change_project_runtime_settings",
        {
          id: projectId,
          environment,
          change,
        },
      );
      setVariables(result);
      if (change.action === "set" || change.name === editing?.name) edit(null);
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
      replace: editing !== null,
    });
  }

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
          {variables.length > 0 ? (
            <ul className="runtime-variables">
              {variables.map((variable) => (
                <li key={variable.name}>
                  <div className="runtime-variable-meta">
                    <code>{variable.name}</code>
                    {variable.kind === "secret" ? (
                      <span className="hint">Secret saved</span>
                    ) : (
                      <span className="runtime-variable-value">
                        = <code>{variable.value}</code>
                      </span>
                    )}
                  </div>
                  <div className="actions">
                    <button
                      type="button"
                      aria-label={`Replace ${variable.name}`}
                      disabled={disabled}
                      onClick={() => edit(variable)}
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
                defaultValue={editing?.name}
                readOnly={editing !== null}
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
                key={kind}
                name="value"
                type={kind === "secret" ? "password" : "text"}
                required={kind === "secret"}
                maxLength={4096}
                disabled={disabled}
                defaultValue={
                  kind === "plain" && editing?.kind === "plain"
                    ? editing.value
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
                {editing ? "Save replacement" : "Add variable"}
              </button>
              {editing && (
                <button
                  type="button"
                  disabled={disabled}
                  onClick={() => edit(null)}
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
