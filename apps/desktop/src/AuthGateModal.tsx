import { useEffect, useState } from "react";
import { authenticateSession, registerAuthRequired } from "./api";

export default function AuthGateModal() {
  const [open, setOpen] = useState(false);
  const [tokenInput, setTokenInput] = useState("");
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);

  useEffect(() => {
    registerAuthRequired(() => setOpen(true));
    return () => registerAuthRequired(null);
  }, []);

  if (!open) return null;

  async function submit(event: React.FormEvent) {
    event.preventDefault();
    if (busy || tokenInput.trim() === "") return;
    setBusy(true);
    setError(null);
    try {
      await authenticateSession(tokenInput.trim());
      setOpen(false);
      window.location.reload();
    } catch (failure) {
      setError(failure instanceof Error ? failure.message : String(failure));
    } finally {
      setBusy(false);
    }
  }

  return (
    <div className="folder-picker-backdrop" role="presentation">
      <div
        className="folder-picker-dialog"
        role="dialog"
        aria-modal="true"
        aria-labelledby="auth-gate-title"
      >
        <div className="folder-picker-header">
          <h2 id="auth-gate-title">Connect this browser to Pockets</h2>
          <p className="hint">
            Each browser needs a one-time sign-in so other websites and running
            Pockets cannot call your local Pockets daemon.
          </p>
        </div>

        <p className="hint">
          Run <code>pockets url</code> in your terminal and paste the one-time
          sign-in link (or session token) below:
        </p>

        <form className="folder-picker-pathbar" onSubmit={(e) => void submit(e)}>
          <input
            type="password"
            value={tokenInput}
            disabled={busy}
            aria-label="Pockets sign-in URL or token"
            onChange={(event) => setTokenInput(event.target.value)}
            placeholder="Paste sign-in URL or session token"
            autoFocus
          />
          <button
            type="submit"
            className="button-primary"
            disabled={busy || tokenInput.trim() === ""}
          >
            {busy ? "Connecting…" : "Connect"}
          </button>
        </form>

        {error && (
          <p role="alert" className="error folder-picker-error">
            {error}
          </p>
        )}
      </div>
    </div>
  );
}
