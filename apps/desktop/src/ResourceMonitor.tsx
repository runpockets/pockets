import { useEffect, useRef, useState } from "react";
import { invoke } from "./api";
import { formatBytes, formatPercent } from "./formatBytes";
import "./ResourceMonitor.css";

/** Per-Pocket usage. Pocket CPU is per virtual core: 100% is one core. */
interface PocketUsage {
  id: string;
  name: string;
  state: "running" | "stopped" | "unavailable";
  cpuPercent: number | null;
  memoryBytes: number | null;
}

/**
 * Snapshot returned by the native `resource_snapshot` command (no arguments).
 *
 * Only the managed Pockets are reported — never the whole computer or the
 * Podman machine's VM. Per-Pocket CPU is Podman's single-shot stats report,
 * which may reflect the average since the container started rather than a
 * synchronized sampling rate. `totals` sums the Pocket values only.
 */
interface ResourceSnapshot {
  sampledAtMs: number;
  pockets: PocketUsage[];
  totals: {
    cpuPercent: number | null;
    memoryBytes: number | null;
  };
  warnings: string[];
}

const REFRESH_MS = 5000;

const STATE_LABELS: Record<PocketUsage["state"], string> = {
  running: "Running",
  stopped: "Stopped",
  unavailable: "Unavailable",
};

function errorMessage(err: unknown): string {
  if (typeof err === "string") return err;
  if (err instanceof Error) return err.message;
  return "The request failed for an unknown reason.";
}

/** Formatted percent, or `null` for unavailable values. */
function percentText(value: number | null): string | null {
  if (value === null || !Number.isFinite(value)) return null;
  return formatPercent(value);
}

/** "1.5 GiB of 8 GiB" when the total is known, else just the used amount. */
function memoryText(
  bytes: number | null,
  totalBytes?: number | null,
): string | null {
  if (bytes === null || !Number.isFinite(bytes) || bytes < 0) return null;
  const used = formatBytes(bytes);
  if (
    totalBytes === null ||
    totalBytes === undefined ||
    !Number.isFinite(totalBytes) ||
    totalBytes < 0
  ) {
    return used;
  }
  return `${used} of ${formatBytes(totalBytes)}`;
}

function formatTime(ms: number): string | null {
  const date = new Date(ms);
  return Number.isFinite(date.getTime()) ? date.toLocaleTimeString() : null;
}

/** Em dash for sighted users, "Unavailable" for assistive tech — never 0. */
function Dash() {
  return (
    <span className="resource-dash">
      <span aria-hidden="true">—</span>
      <span className="resource-sr-only">Unavailable</span>
    </span>
  );
}

/**
 * Managed-Pockets-only resource usage card. Closed by default: no
 * `resource_snapshot` request happens until it is opened, and closing tears
 * the polling loop down and retires the previous sample so a reopen always
 * shows fresh data.
 *
 * While open, the next sample starts 5s after the previous one settles — a
 * setTimeout chain armed in the reply's finally, so requests can never
 * overlap, even across a close followed by an immediate reopen (the in-flight
 * request is tracked in a ref shared by all open sessions). Polling pauses
 * while the page is hidden and resumes with a fresh sample on return. Late
 * replies from a closed or unmounted session are discarded; refresh failures
 * keep the previous values clearly marked stale.
 */
export default function ResourceMonitor({
  controlledOpen,
  embedded = false,
}: {
  /** When provided (e.g. inside a parent `<details>` disclosure), sampling
   *  follows the disclosure directly instead of requiring a second click. */
  controlledOpen?: boolean;
  /** Hides the redundant inner card header when wrapped in a labelled disclosure. */
  embedded?: boolean;
} = {}) {
  const [uncontrolledOpen, setUncontrolledOpen] = useState(false);
  const open = controlledOpen ?? uncontrolledOpen;
  const [snapshot, setSnapshot] = useState<ResourceSnapshot | null>(null);
  const [refreshing, setRefreshing] = useState(false);
  const [error, setError] = useState<string | null>(null);

  // Shared across open sessions so a close+immediate-reopen can never run a
  // second request while the previous session's request is still in flight.
  const inFlightRef = useRef(false);
  const ownerRef = useRef(0);
  const sessionRef = useRef(0);
  // Kick left behind by a session that had to defer its first sample.
  const pendingKickRef = useRef<(() => void) | null>(null);

  const handleClose = () => {
    setUncontrolledOpen(false);
    // A new open always starts from a fresh sample; the old readings are
    // retired rather than resurfacing as un-marked stale data.
    setSnapshot(null);
    setError(null);
    setRefreshing(false);
  };

  useEffect(() => {
    if (!open) {
      setSnapshot(null);
      setError(null);
      setRefreshing(false);
      return;
    }
    const session = ++sessionRef.current;
    // Per-session flag: closing or unmounting stops this loop from applying
    // replies, rescheduling, or kicking anything.
    let active = true;
    let timer: number | null = null;

    const clearTimer = () => {
      if (timer !== null) {
        window.clearTimeout(timer);
        timer = null;
      }
    };

    const schedule = () => {
      clearTimer();
      // Hidden: pause entirely; the visibilitychange listener resumes.
      if (document.visibilityState === "hidden") return;
      timer = window.setTimeout(() => void sample(), REFRESH_MS);
    };

    const kick = () => {
      void sample();
    };

    const sample = async () => {
      if (!active || document.visibilityState === "hidden") return;
      if (inFlightRef.current) {
        // A request is still pending — ours (its finally reschedules) or a
        // closed session's. Defer instead of overlapping; for the latter,
        // leave a kick so the loop resumes when the shared slot frees up.
        if (ownerRef.current !== session) pendingKickRef.current = kick;
        return;
      }
      inFlightRef.current = true;
      ownerRef.current = session;
      setRefreshing(true);
      try {
        const value = await invoke<ResourceSnapshot>("resource_snapshot");
        if (!active) return;
        setSnapshot(value);
        setError(null);
      } catch (err) {
        // Keep showing the previous sample, but the alert and the stale
        // marker make it unmistakably not current data.
        if (!active) return;
        setError(errorMessage(err));
      } finally {
        inFlightRef.current = false;
        if (active) {
          setRefreshing(false);
          schedule();
        } else {
          const pending = pendingKickRef.current;
          pendingKickRef.current = null;
          pending?.();
        }
      }
    };

    const onVisibility = () => {
      if (document.visibilityState === "hidden") {
        clearTimer();
      } else {
        // Back on screen: take a fresh sample right away; its completion
        // reschedules the normal cadence.
        clearTimer();
        kick();
      }
    };

    document.addEventListener("visibilitychange", onVisibility);
    kick();

    return () => {
      active = false;
      clearTimer();
      document.removeEventListener("visibilitychange", onVisibility);
    };
  }, [open]);

  return (
    <section className="card" aria-labelledby="resource-heading">
      {embedded ? (
        <h2 id="resource-heading" className="visually-hidden">
          Resource Monitor
        </h2>
      ) : (
        <div className="card-top">
          <h2 id="resource-heading" className="card-title">
            Resource Monitor
          </h2>
          {controlledOpen === undefined &&
            (open ? (
              // Deliberately never disabled: closing is always available, even
              // while a request is in flight (its reply is then discarded).
              <button type="button" onClick={handleClose}>
                Close
              </button>
            ) : (
              <button type="button" onClick={() => setUncontrolledOpen(true)}>
                Open Resource Monitor
              </button>
            ))}
        </div>
      )}

      {!open && (
        <p className="hint">
          CPU and memory usage for the Pockets this app manages. Nothing is
          sampled until you open it.
        </p>
      )}

      {open && (
        <div className="resource-body" aria-busy={refreshing}>
          {error && (
            <p className="error" role="alert">
              {snapshot
                ? `Could not refresh the sample. ${error} The values below are from the previous sample and may be stale.`
                : `Could not collect a resource sample. ${error}`}
            </p>
          )}

          {snapshot && snapshot.warnings.length > 0 && (
            <div className="resource-warnings" role="status">
              <p className="resource-warnings-title">Native warnings</p>
              <ul>
                {snapshot.warnings.map((warning, index) => (
                  <li key={`${index}-${warning}`}>{warning}</li>
                ))}
              </ul>
            </div>
          )}

          <p className="hint resource-meta">
            {!snapshot
              ? error
                ? "No sample collected yet."
                : "Collecting the first sample…"
              : refreshing
                ? "Refreshing…"
                : `Updated ${formatTime(snapshot.sampledAtMs) ?? "—"}`}
            {snapshot !== null && error !== null && (
              <>
                {" "}
                <span className="resource-stale">Stale</span>
              </>
            )}
          </p>

          <div className="resource-scopes">
            <Scope
              title="Pockets total"
              subtitle="Sum across the Pockets below"
              cpuLabel="CPU"
              cpu={percentText(snapshot?.totals.cpuPercent ?? null)}
              memory={snapshot ? memoryText(snapshot.totals.memoryBytes) : null}
              foot="Covers only the managed containers listed below — not Pockets itself, the Podman machine, or other workloads on this computer."
            />
          </div>

          <div className="resource-pockets">
            <h3 className="resource-scope-title">Pockets</h3>
            {snapshot ? (
              snapshot.pockets.length === 0 ? (
                <p className="hint">No Pockets to report.</p>
              ) : (
                <>
                  <div className="resource-table-wrap">
                    <table className="resource-table">
                      <caption className="resource-sr-only">
                        Status, CPU, and memory for each Pocket
                      </caption>
                      <thead>
                        <tr>
                          <th scope="col">Pocket</th>
                          <th scope="col">Status</th>
                          <th scope="col" className="resource-col-num">
                            CPU
                            <span className="resource-col-note">
                              {" "}
                              (100% = one core)
                            </span>
                          </th>
                          <th scope="col" className="resource-col-num">
                            Memory
                          </th>
                        </tr>
                      </thead>
                      <tbody>
                        {snapshot.pockets.map((pocket) => (
                          <tr key={pocket.id}>
                            <th scope="row">{pocket.name}</th>
                            <td>
                              <span className={`badge badge-${pocket.state}`}>
                                {STATE_LABELS[pocket.state]}
                              </span>
                            </td>
                            <td className="resource-col-num">
                              {percentText(pocket.cpuPercent) ?? <Dash />}
                            </td>
                            <td className="resource-col-num">
                              {memoryText(pocket.memoryBytes) ?? <Dash />}
                            </td>
                          </tr>
                        ))}
                      </tbody>
                    </table>
                  </div>
                  <p className="hint">
                    Pocket CPU is Podman&apos;s single-shot stats report for
                    each container and may reflect the average since that Pocket
                    started — it is not a synchronized five-second rate. 100%
                    means one virtual core fully used, so a Pocket can exceed
                    100%.
                  </p>
                </>
              )
            ) : (
              <p className="hint">
                Pocket metrics arrive with the first sample.
              </p>
            )}
          </div>
        </div>
      )}
    </section>
  );
}

function Scope({
  title,
  subtitle,
  cpuLabel,
  cpu,
  memory,
  foot,
}: {
  title: string;
  subtitle: string;
  cpuLabel: string;
  cpu: string | null;
  memory: string | null;
  foot: string;
}) {
  return (
    <section className="resource-scope" aria-label={`${title} usage`}>
      <h3 className="resource-scope-title">{title}</h3>
      <p className="resource-scope-subtitle">{subtitle}</p>
      <dl className="resource-facts">
        <dt>{cpuLabel}</dt>
        <dd>{cpu ?? <Dash />}</dd>
        <dt>Memory</dt>
        <dd>{memory ?? <Dash />}</dd>
      </dl>
      <p className="resource-scope-foot">{foot}</p>
    </section>
  );
}
