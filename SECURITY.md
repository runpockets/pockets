# Security Policy

Pockets orchestrates local containers, runtime secrets, and loopback web services on a user's machine. We take security boundaries seriously.

## Security Model Summary

- **Loopback-only control plane:** The Pockets daemon binds exclusively to `127.0.0.1`, enforces strict `Host` and `Origin` checks, and requires an authenticated local session token (`HttpOnly; SameSite=Strict`) for control API calls.
- **No engine socket exposure:** Pocket containers never receive the Podman/Docker engine socket, privileged mode, host networking, or arbitrary host directory mounts.
- **Least-privilege container execution:** Containers run with dropped Linux capabilities, `no-new-privileges`, dedicated named volumes, and loopback-only published ports.
- **Isolated preview execution:** Development previews run on an isolated container network with explicit per-host HTTPS gateway approval and disposable preview data volumes separate from installed Pocket data.
- **Secret isolation:** Runtime secrets are stored outside Pocket source snapshots, build contexts, manifests, and container command-line arguments.

## Reporting a Vulnerability

Please **do not** open a public GitHub issue for security vulnerabilities.

Instead, report vulnerabilities privately via **GitHub Private Vulnerability Reporting** on this repository's **Security** tab (`Security → Advisories → Report a vulnerability`).

Please include:
- A clear description of the vulnerability and affected component (daemon API, container boundary, preview proxy/egress gateway, secret storage, Tailscale integration, or manifest validation).
- Minimal reproduction steps or proof of concept.
- Your assessment of impact.

We aim to acknowledge reports within 72 hours and coordinate a fix and disclosure timeline with you.
