# Resource Monitor

Resource Monitor is a first-party view included in Desktop, not a container
with elevated permissions. Choose **Open Resource Monitor** to see combined
and per-pocket CPU/memory and running/stopped status.
It includes Hello Pocket and installed created Pockets;
unbuilt drafts and unrelated containers are not pocket totals.

## Reading the numbers

- **Pocket CPU:** 100% means one virtual CPU core; a multi-core workload can
  exceed 100%. The combined figure sums the included pockets. Podman's
  single-sample report may reflect average usage since container start,
  rather than a synchronized five-second rate.
- **Pocket memory:** the engine's container memory usage in bytes, not a
  reservation or the application's storage volume size. Binary units are
  labeled MiB and GiB.

Totals cover only the listed managed pocket containers. They do not include
the Desktop application's own process, the VM's Linux OS, Podman, or hypervisor
overhead. The monitor does not sample or display whole-computer or VM usage.

Podman's VM memory capacity is a separate concept: the local machine was
configured with 2 GiB (2048 MiB) when inspected, with about 1.9 GiB usable
inside Linux. That capacity is shared by Linux, Podman, and every container
in that VM; it is not each pocket's usage or an exact measurement of physical
Mac RAM occupied by the VM. Resource Monitor does not change VM configuration.

Missing samples are unavailable, not zero. A stopped or not-yet-created pocket
uses zero; an ownership conflict or incomplete running sample does not.
Warnings identify incomplete results. If Podman is missing or unreachable,
pocket measurements are unavailable. If the installation store cannot be read,
the bundled pocket can still be listed, but combined totals are unavailable
rather than presented as if the list were complete.

## Refresh and trust boundary

Refreshes run only while the monitor is open and the document is visible,
with a five-second pause after each completed sample. There is no background
monitor service or historical metrics database. Closing the view stops new
requests; an already-running bounded read may finish.

The frontend invokes a fixed, no-argument native command. Rust discovers
Podman, reads the native installation store, verifies exact container names
and ownership labels, and requests stats only for owned running container IDs.
Sampling does not start, stop, or modify containers or the Podman machine.
Ordinary pockets receive neither these metrics nor an engine socket or
other-pocket access.

## Manual acceptance

1. Open Desktop with Podman operational and open Resource Monitor. Check that
   Hello Pocket and installed apps appear, but drafts and unrelated containers
   do not.
2. Compare running rows with `podman stats --no-stream`; expect normal sampling
   differences. Stopped rows should show zero. Use the existing lifecycle
   controls only if you deliberately want to start or stop an app.
3. Close and reopen the monitor. Hide the app and bring it back. Confirm fresh
   readings resume without duplicate refresh loops.
4. If Podman is already unavailable, confirm pocket readings are unavailable
   with an explanation. Do not shut down
   a working VM merely to run this check.

Automated component tests cover refresh lifecycle with mocked IPC; native
tests cover reconciliation and parsing. They do not by themselves verify
the native window's visibility events or IPC rendering.
