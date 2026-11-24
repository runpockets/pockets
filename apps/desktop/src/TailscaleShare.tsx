import { useEffect, useMemo, useState } from "react";
import { encodeQr, qrSvgPath } from "./qrCode";
import "./TailscaleShare.css";

/** Tailscale detection report returned by the native `detect_tailscale` command. */
export interface TailscaleReport {
  state: "missing" | "unavailable" | "httpsDisabled" | "ready";
  message: string;
  dnsName: string | null;
  version: string | null;
  portalUrl?: string | null;
}

/** Per-Pocket Tailscale sharing status carried on `PocketStatus`. */
export interface TailscaleShare {
  enabled: boolean;
  active: boolean;
  url: string | null;
  directUrl?: string | null;
  portalUrl?: string | null;
  message: string | null;
}

const TAILSCALE_STATE_LABELS: Record<TailscaleReport["state"], string> = {
  ready: "Ready",
  httpsDisabled: "HTTPS disabled",
  unavailable: "Unavailable",
  missing: "Missing",
};

const TAILSCALE_BADGE_CLASSES: Record<TailscaleReport["state"], string> = {
  ready: "badge-ready",
  httpsDisabled: "badge-unavailable",
  unavailable: "badge-unavailable",
  missing: "badge-missing",
};

/**
 * Crisp black-on-white SVG QR code with a 4-module quiet zone so phone cameras
 * scan reliably in both dark and light desktop themes.
 */
export function QrCodeSvg({
  value,
  label,
  size = 136,
}: {
  value: string;
  label: string;
  size?: number;
}) {
  const { viewBoxSize, path } = useMemo(
    () => qrSvgPath(encodeQr(value)),
    [value],
  );
  return (
    <svg
      className="tailscale-qr-svg"
      width={size}
      height={size}
      viewBox={`0 0 ${viewBoxSize} ${viewBoxSize}`}
      shapeRendering="crispEdges"
      role="img"
      aria-label={label}
    >
      <rect width={viewBoxSize} height={viewBoxSize} fill="#ffffff" />
      <path d={path} fill="#000000" />
    </svg>
  );
}

/**
 * Secondary management card mirroring `RuntimePanel` in `App.tsx`, showing
 * local Tailscale availability and MagicDNS hostname on demand.
 */
export function TailscaleStatusPanel({
  report,
  refreshing,
  onRefresh,
}: {
  report:
    | { status: "idle" }
    | { status: "loading" }
    | { status: "error"; message: string }
    | { status: "ready"; value: TailscaleReport };
  refreshing: boolean;
  onRefresh: () => void;
}) {
  const [copiedLauncher, setCopiedLauncher] = useState(false);
  useEffect(() => {
    if (!copiedLauncher) return;
    const timer = window.setTimeout(() => setCopiedLauncher(false), 1500);
    return () => window.clearTimeout(timer);
  }, [copiedLauncher]);
  const portalUrl =
    report.status === "ready" ? (report.value.portalUrl ?? null) : null;

  return (
    <section className="card" aria-labelledby="tailscale-heading">
      <div className="card-top">
        <h2 id="tailscale-heading" className="visually-hidden">
          Tailscale
        </h2>
        <div
          className="runtime-status"
          aria-live="polite"
          aria-busy={refreshing || report.status === "loading"}
        >
          {(report.status === "idle" || report.status === "loading") && (
            <p className="muted">Checking Tailscale…</p>
          )}
          {report.status === "error" && (
            <p className="error">Could not check Tailscale. {report.message}</p>
          )}
          {report.status === "ready" && (
            <p className="tailscale-panel-summary">
              <span
                className={`badge ${TAILSCALE_BADGE_CLASSES[report.value.state]}`}
              >
                {TAILSCALE_STATE_LABELS[report.value.state]}
              </span>
              {report.value.dnsName && (
                <code className="tailscale-panel-host">
                  {report.value.dnsName}
                </code>
              )}
            </p>
          )}
        </div>
        <button
          type="button"
          onClick={onRefresh}
          disabled={refreshing || report.status === "loading"}
        >
          {refreshing ? "Checking…" : "Refresh"}
        </button>
      </div>
      {portalUrl && (
        <div className="tailscale-launcher-box">
          <div className="tailscale-launcher-qr">
            <QrCodeSvg
              value={portalUrl}
              label="QR code for Pockets Tailscale launcher"
              size={88}
            />
          </div>
          <div className="tailscale-launcher-text">
            <p className="tailscale-launcher-title">Pocket launcher</p>
            <code className="tailscale-panel-host">{portalUrl}</code>
            <p className="hint">
              One bookmark for all your shared Pockets (active while Pockets
              Desktop is open).
            </p>
            <div className="creation-tailscale-actions">
              <button
                type="button"
                onClick={() => {
                  void navigator.clipboard
                    ?.writeText(portalUrl)
                    .then(() => setCopiedLauncher(true))
                    .catch(() => {});
                }}
              >
                {copiedLauncher ? "Copied" : "Copy launcher URL"}
              </button>
            </div>
          </div>
        </div>
      )}
      {report.status === "ready" && (
        <details className="card-details">
          <summary>detail</summary>
          <p className="muted">{report.value.message}</p>
        </details>
      )}
    </section>
  );
}
