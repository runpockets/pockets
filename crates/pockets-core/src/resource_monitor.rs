//! Read-only resource monitor: one native `resource_snapshot` command, no
//! frontend inputs, no mutations anywhere.
//!
//! The snapshot answers "what is this costing" with the freshest readable
//! numbers for exactly the
//! Pockets Pockets owns: the fixed bundled Hello example plus every
//! *installed* record in the authoritative [`Store`] (drafts are excluded;
//! [`Store::load`] already projects a persisted built image onto
//! [`Phase::Installed`]). Nothing is ever taken from the frontend — not an
//! id, name, path, or container — and the [`Controller`](crate::Controller)
//! lifecycle lock is held for the whole reconcile so a sample can never
//! interleave with a start/stop/build.
//!
//! One metric source, read-only:
//! - **pockets**: one `podman ps --all --format json` read reconciles each
//!   target by *exact native container name AND* the
//!   [`OWNERSHIP_LABEL`](crate::pocket::OWNERSHIP_LABEL) value — a foreign
//!   container wearing a managed name is `unavailable`, never reported as
//!   ours or as stopped. Only owned *running* containers' full IDs are
//!   handed to a single `podman stats --no-stream --no-trunc` call, and
//!   that call is skipped entirely when there are none (a no-argument
//!   `stats` would collect every container on the engine).
//!
//! The monitor deliberately says nothing about the whole computer or the
//! Podman VM: no host sampling, no `podman info`. It answers only "what is
//! this costing".
//!
//! Every engine failure degrades visibly instead of failing the command:
//! affected pockets become `unavailable`, the affected totals become
//! `null` (never silently zeroed), and a concrete `warnings` entry names
//! what happened. Only finite, nonnegative metrics are ever reported. No
//! sockets, no shell, no health probes, no mutation: every command runs
//! through the bounded [`runtime::run`].

use std::collections::HashMap;
use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

use serde::Deserialize;
use serde::Serialize;

use crate::manifest;
use crate::pocket;
use crate::projects::{Phase, Project, Store};
use crate::runtime;

/// Contract id of the bundled Hello Pocket. Pinned to the bundled
/// manifest's `metadata.id` by a test so the two cannot drift.
const BUNDLED_ID: &str = "hello-pocket";

/// Fallback display name used only if the bundled manifest fails to
/// parse; normally the name comes from the manifest itself.
const BUNDLED_NAME: &str = "Hello Pocket";

/// Raw `podman stats` line template. The default `--format json` rounds
/// CPU to a display string and renders memory as human text, so the
/// template is used instead: it marshals the raw stat structs, yielding
/// one JSON object per line with the full `ContainerID`, a numeric `CPU`
/// percent, and a numeric `MemUsage` in bytes (verified against the
/// installed Podman 6.2).
const STATS_FORMAT: &str = "{{json .}}";

// ---------------------------------------------------------------------------
// Serialized contract
// ---------------------------------------------------------------------------

/// Reconciled lifecycle state of one monitored Pocket.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum MonitorState {
    /// The owned container runs; metrics come from `podman stats`.
    Running,
    /// No container exists, or the owned container is exited/stopped.
    /// Contract: stopped costs zero, so metrics are real zeros.
    Stopped,
    /// The truth is unknown or foreign: engine down, unreadable state, a
    /// paused/created/other non-running state, or another tool's
    /// container claiming a managed name. Metrics are `null`.
    Unavailable,
}

/// Usage of one monitored Pocket: the bundled Hello (`id:
/// "hello-pocket"`) or an installed store project keyed by its project id.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PocketUsage {
    pub id: String,
    pub name: String,
    pub state: MonitorState,
    pub cpu_percent: Option<f64>,
    pub memory_bytes: Option<u64>,
}

/// Summed Pocket usage. A field is `null` whenever any pocket that could
/// be consuming resources (running or unavailable) lacks that metric —
/// an incomplete sum is never silently zeroed or undercounted. Stopped
/// pockets contribute exactly zero.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Totals {
    pub cpu_percent: Option<f64>,
    pub memory_bytes: Option<u64>,
}

/// The full snapshot contract sent to the frontend.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ResourceSnapshot {
    pub sampled_at_ms: u64,
    pub pockets: Vec<PocketUsage>,
    pub totals: Totals,
    pub warnings: Vec<String>,
}

// ---------------------------------------------------------------------------
// Monitored targets
// ---------------------------------------------------------------------------

/// One monitored Pocket and the exact native identity its container must
/// carry: both the precise container name and the ownership label value
/// have to match before anything is attributed to it.
#[derive(Debug, Clone, PartialEq, Eq)]
struct PocketTarget {
    id: String,
    name: String,
    container_name: String,
    ownership: String,
}

/// The bundled Hello target: fixed name and ownership from
/// [`pocket`], display name from the bundled manifest.
fn hello_target() -> PocketTarget {
    PocketTarget {
        id: BUNDLED_ID.to_owned(),
        name: manifest::bundled_manifest()
            .map(|m| m.metadata.name)
            .unwrap_or_else(|_| BUNDLED_NAME.to_owned()),
        container_name: pocket::CONTAINER_NAME.to_owned(),
        ownership: pocket::OWNERSHIP_VALUE.to_owned(),
    }
}

/// Targets for installed records only — a draft has no container to
/// monitor and is deliberately absent (never even reconciled). Identity
/// mirrors [`pocket::Target::managed`] exactly, from the same constants.
fn installed_targets(projects: &[Project]) -> Vec<PocketTarget> {
    projects
        .iter()
        .filter(|project| project.phase == Phase::Installed)
        .map(|project| PocketTarget {
            id: project.id.clone(),
            name: project.manifest.metadata.name.clone(),
            container_name: format!("{}{}", pocket::MANAGED_NAME_PREFIX, project.id),
            ownership: format!("{}{}", pocket::MANAGED_OWNERSHIP_PREFIX, project.id),
        })
        .collect()
}

// ---------------------------------------------------------------------------
// Command and sampling flow
// ---------------------------------------------------------------------------

/// Read-only resource snapshot of every Pocket Pockets owns.
///
/// Takes no frontend input and holds the [`Controller`] lifecycle lock for
/// the whole sample, exactly like the status and lifecycle commands. It
/// never fails on degraded sources: every one of them shows up as `null`
/// metrics plus a warning while everything that *could* be measured
/// survives — the `Result` shape is Tauri's, and the `Err` arm exists only
/// for IPC transport, never for a partial snapshot.
pub async fn resource_snapshot(ctx: &crate::AppContext) -> Result<ResourceSnapshot, String> {
    let _serializing = ctx.controller.lifecycle_guard().await;
    Ok(sample(Ok(ctx.store())).await)
}

/// The sampling flow behind the command, testable (and used by the live
/// test) without a Tauri app handle: the store arrives pre-resolved, an
/// `Err` being the command boundary's own "store could not be opened"
/// case, which is reported as a warning rather than a failed snapshot.
pub(crate) async fn sample(store: Result<Store, String>) -> ResourceSnapshot {
    let mut warnings = Vec::new();
    let sampled_at_ms = now_ms();

    // Authoritative pocket list: bundled Hello always, installed store
    // records when the store reads. A store failure shrinks the list to
    // the bundled Pocket and says so — it never sinks the whole snapshot.
    // It also marks the inventory as known-incomplete: totals must then
    // stop claiming to sum every Pocket (see [`totals_for`]).
    let mut targets = vec![hello_target()];
    let inventory_complete = match store {
        Ok(store) => match tokio::task::spawn_blocking(move || store.list()).await {
            Ok(Ok(projects)) => {
                targets.extend(installed_targets(&projects));
                true
            }
            Ok(Err(err)) => {
                warnings.push(format!(
                    "Pockets could not read your installed Pockets, so only \
                     the bundled Pocket is listed. {err}"
                ));
                false
            }
            Err(_) => {
                warnings.push(
                    "Pockets could not finish reading your installed Pockets; \
                     the file operation was interrupted. Only the bundled \
                     Pocket is listed."
                        .to_owned(),
                );
                false
            }
        },
        Err(err) => {
            warnings.push(format!(
                "Pockets could not open the project store, so only the \
                 bundled Pocket is listed. {err}"
            ));
            false
        }
    };

    // Without the binary there is nothing to ask: every pocket is
    // unavailable and `require_podman`'s actionable install message is the
    // warning. With it, the monitor asks only `ps` and, for owned running
    // containers, `stats` — never host sampling, never `podman info`.
    let pockets = match crate::require_podman() {
        Ok(bin) => reconcile(&bin, &targets, &mut warnings).await,
        Err(err) => {
            warnings.push(err);
            all_unavailable(&targets)
        }
    };

    ResourceSnapshot {
        sampled_at_ms,
        totals: totals_for(inventory_complete, &pockets),
        pockets,
        warnings,
    }
}

/// Milliseconds since the Unix epoch; 0 only if the clock is broken.
fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

// ---------------------------------------------------------------------------
// Pocket reconciliation
// ---------------------------------------------------------------------------

/// One `podman ps --all --format json` row: everything ownership and
/// state reconciliation needs, nothing else.
#[derive(Debug, Deserialize)]
struct PsRow {
    #[serde(rename = "Id")]
    id: String,
    #[serde(rename = "Names")]
    names: PsNames,
    #[serde(rename = "Labels", default)]
    labels: Option<HashMap<String, String>>,
    #[serde(rename = "State", default)]
    state: String,
}

impl PsRow {
    /// Exact Podman name match (some engines prefix a slash), mirroring
    /// the lifecycle module's `PsEntry::has_name`.
    fn has_name(&self, name: &str) -> bool {
        let matches =
            |candidate: &str| candidate == name || candidate.strip_prefix('/') == Some(name);
        match &self.names {
            PsNames::One(candidate) => matches(candidate),
            PsNames::Many(candidates) => candidates.iter().any(|candidate| matches(candidate)),
        }
    }

    fn ownership(&self) -> Option<&str> {
        self.labels
            .as_ref()
            .and_then(|labels| labels.get(pocket::OWNERSHIP_LABEL))
            .map(String::as_str)
    }
}

/// Podman marshals `Names` as an array; Docker-compatible engines use a
/// single string.
#[derive(Debug, Deserialize)]
#[serde(untagged)]
enum PsNames {
    One(String),
    Many(Vec<String>),
}

/// Reconcile every target against one `ps` read, then collect stats for
/// exactly the owned running containers.
async fn reconcile(
    bin: &Path,
    targets: &[PocketTarget],
    warnings: &mut Vec<String>,
) -> Vec<PocketUsage> {
    let rows = match runtime::run(bin, &["ps", "--all", "--format", "json"]).await {
        Ok(output) => match parse_ps_rows(&output.stdout) {
            Ok(rows) => rows,
            Err(err) => {
                warnings.push(format!(
                    "Pockets could not read the container list from Podman, \
                     so Pocket usage is unavailable. {err}"
                ));
                return all_unavailable(targets);
            }
        },
        Err(err) => {
            warnings.push(format!(
                "Pockets could not read the container list from Podman, so \
                 Pocket usage is unavailable. {err}"
            ));
            return all_unavailable(targets);
        }
    };

    let (mut usage, running) = classify(&rows, targets, warnings);
    apply_stats(bin, &mut usage, &running, warnings).await;
    usage
}

/// Parse `podman ps --all --format json` (a JSON array of rows).
fn parse_ps_rows(stdout: &[u8]) -> Result<Vec<PsRow>, String> {
    serde_json::from_slice(stdout)
        .map_err(|err| format!("could not parse `podman ps` output: {err}"))
}

/// Pure reconciliation of parsed rows against targets. Never claims
/// running except for an owned row in state `running`; a foreign
/// container claiming a managed name is `unavailable` (never "stopped",
/// which would misattribute the name as free), and any other engine
/// state (paused, created, exiting, …) is `unavailable` with a warning.
/// Owned missing containers and exited/stopped containers are `stopped`
/// with real zero metrics. Returns the pocket list plus `(index, full
/// id)` pairs for the running ones that are safe to hand to `stats`.
fn classify(
    rows: &[PsRow],
    targets: &[PocketTarget],
    warnings: &mut Vec<String>,
) -> (Vec<PocketUsage>, Vec<(usize, String)>) {
    let mut usage = Vec::with_capacity(targets.len());
    let mut running = Vec::new();
    for (index, target) in targets.iter().enumerate() {
        let row = rows.iter().find(|row| row.has_name(&target.container_name));
        let Some(row) = row else {
            // Missing owned container: genuinely stopped, and stopped
            // costs nothing — real zeros, not nulls.
            usage.push(PocketUsage {
                id: target.id.clone(),
                name: target.name.clone(),
                state: MonitorState::Stopped,
                cpu_percent: Some(0.0),
                memory_bytes: Some(0),
            });
            continue;
        };
        if row.ownership() != Some(target.ownership.as_str()) {
            warnings.push(format!(
                "A container named {} exists that Pockets does not manage \
                 (its {} label is not {:?}); {} is reported unavailable \
                 rather than stopped.",
                target.container_name,
                pocket::OWNERSHIP_LABEL,
                target.ownership,
                target.id
            ));
            usage.push(unavailable(target));
            continue;
        }
        match row.state.as_str() {
            "running" => {
                // The id comes from engine output and becomes a `stats`
                // argument; anything malformed forfeits the metrics, not
                // the state.
                if pocket::is_safe_container_id(&row.id) {
                    running.push((index, row.id.clone()));
                } else {
                    warnings.push(format!(
                        "Podman reported an unusable container id for Pocket \
                         {}; its live metrics could not be read.",
                        target.name
                    ));
                }
                usage.push(PocketUsage {
                    id: target.id.clone(),
                    name: target.name.clone(),
                    state: MonitorState::Running,
                    cpu_percent: None,
                    memory_bytes: None,
                });
            }
            "exited" | "stopped" => usage.push(PocketUsage {
                id: target.id.clone(),
                name: target.name.clone(),
                state: MonitorState::Stopped,
                cpu_percent: Some(0.0),
                memory_bytes: Some(0),
            }),
            other => {
                warnings.push(format!(
                    "Pocket {} is in Podman state {other:?}, which is neither \
                     running nor stopped; it is reported unavailable rather \
                     than claimed to be running.",
                    target.name
                ));
                usage.push(unavailable(target));
            }
        }
    }
    (usage, running)
}

/// One `podman stats` line's usable values: missing or non-finite/negative
/// numbers stay `None` instead of becoming zero.
#[derive(Debug, Clone, PartialEq)]
struct StatsSample {
    cpu_percent: Option<f64>,
    memory_bytes: Option<u64>,
}

/// Live metrics for exactly the owned running containers, matched by full
/// container id (never by name or prefix). The stats call is skipped
/// entirely with no ids — no arguments would make `podman stats` collect
/// every container on the engine.
async fn apply_stats(
    bin: &Path,
    usage: &mut [PocketUsage],
    running: &[(usize, String)],
    warnings: &mut Vec<String>,
) {
    if running.is_empty() {
        return;
    }
    let mut args: Vec<&str> = vec![
        "stats",
        "--no-stream",
        "--no-trunc",
        "--format",
        STATS_FORMAT,
    ];
    args.extend(running.iter().map(|(_, id)| id.as_str()));
    let stdout = match runtime::run(bin, &args).await {
        Ok(output) => output.stdout,
        Err(err) => {
            warnings.push(format!(
                "Pockets could not read live resource usage from Podman. {err}"
            ));
            return;
        }
    };
    let samples = match parse_stats_rows(&stdout) {
        Ok(samples) => samples,
        Err(err) => {
            warnings.push(format!(
                "Pockets could not read live resource usage from Podman. {err}"
            ));
            return;
        }
    };
    apply_stats_samples(usage, running, &samples, warnings);
}

/// Pure application of parsed stats rows onto the running pockets. Any
/// running pocket whose row or field is missing keeps `null` metrics and
/// gets a warning — an incomplete sample is never silently zeroed.
fn apply_stats_samples(
    usage: &mut [PocketUsage],
    running: &[(usize, String)],
    samples: &HashMap<String, StatsSample>,
    warnings: &mut Vec<String>,
) {
    for (index, id) in running {
        let pocket = &mut usage[*index];
        let Some(sample) = samples.get(id) else {
            warnings.push(format!(
                "Podman returned no resource usage for the running Pocket {}; \
                 its metrics are unavailable.",
                pocket.name
            ));
            continue;
        };
        pocket.cpu_percent = sample.cpu_percent;
        pocket.memory_bytes = sample.memory_bytes;
        if sample.cpu_percent.is_none() {
            warnings.push(format!(
                "Podman reported no usable CPU usage for the running Pocket \
                 {}; its CPU metric is unavailable.",
                pocket.name
            ));
        }
        if sample.memory_bytes.is_none() {
            warnings.push(format!(
                "Podman reported no usable memory usage for the running \
                 Pocket {}; its memory metric is unavailable.",
                pocket.name
            ));
        }
    }
}

/// Parse `podman stats --format '{{json .}}'` output: one JSON object per
/// line. A single unparseable or id-less line fails the whole read —
/// partial stats must not masquerade as a complete sample; the caller
/// then nulls every running pocket visibly.
fn parse_stats_rows(stdout: &[u8]) -> Result<HashMap<String, StatsSample>, String> {
    let text = String::from_utf8_lossy(stdout);
    let mut samples = HashMap::new();
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let value: serde_json::Value = serde_json::from_str(line)
            .map_err(|err| format!("could not parse `podman stats` output: {err}"))?;
        let id = value
            .get("ContainerID")
            .and_then(serde_json::Value::as_str)
            .filter(|id| pocket::is_safe_container_id(id))
            .ok_or("`podman stats` output carries a row without a usable ContainerID")?;
        let sample = StatsSample {
            cpu_percent: value
                .get("CPU")
                .and_then(serde_json::Value::as_f64)
                .and_then(sane_percent),
            memory_bytes: value
                .get("MemUsage")
                .and_then(serde_json::Value::as_f64)
                .and_then(sane_bytes_f64),
        };
        samples.insert(id.to_owned(), sample);
    }
    Ok(samples)
}

// ---------------------------------------------------------------------------
// Aggregation and small guards
// ---------------------------------------------------------------------------

/// Sum Pocket usage into the snapshot totals, honoring inventory
/// completeness. When the store could not be read the pocket list is
/// known to be missing installed Pockets — only the bundled Hello
/// survives on it — and a total over a knowingly partial list would
/// silently understate the whole. Both fields then go `null`
/// unconditionally, while the visible Hello row and the store warning
/// remain exactly as reported.
fn totals_for(inventory_complete: bool, pockets: &[PocketUsage]) -> Totals {
    if inventory_complete {
        aggregate(pockets)
    } else {
        Totals {
            cpu_percent: None,
            memory_bytes: None,
        }
    }
}

/// Sum Pocket usage into the totals. Stopped contributes exact zeros; a
/// running or unavailable pocket with a missing field makes the whole
/// field `null` (undercounting an unknown consumer would be a lie);
/// fields aggregate independently. Sums stay inside the contract:
/// memory uses `checked_add` (an overflowing total is `null`, never a
/// wrapped number) and every CPU running sum passes `sane_percent`, so a
/// sum that reaches infinity is `null` rather than a non-finite number.
fn aggregate(pockets: &[PocketUsage]) -> Totals {
    let mut cpu = Some(0.0f64);
    let mut memory = Some(0u64);
    for pocket in pockets {
        match pocket.state {
            MonitorState::Stopped => {}
            MonitorState::Running => {
                cpu = match (cpu, pocket.cpu_percent) {
                    (Some(total), Some(value)) => sane_percent(total + value),
                    _ => None,
                };
                memory = match (memory, pocket.memory_bytes) {
                    (Some(total), Some(value)) => total.checked_add(value),
                    _ => None,
                };
            }
            MonitorState::Unavailable => {
                cpu = None;
                memory = None;
            }
        }
    }
    Totals {
        cpu_percent: cpu,
        memory_bytes: memory,
    }
}

/// All-targets unavailable: the engine could not answer at all.
fn all_unavailable(targets: &[PocketTarget]) -> Vec<PocketUsage> {
    targets.iter().map(unavailable).collect()
}

fn unavailable(target: &PocketTarget) -> PocketUsage {
    PocketUsage {
        id: target.id.clone(),
        name: target.name.clone(),
        state: MonitorState::Unavailable,
        cpu_percent: None,
        memory_bytes: None,
    }
}

/// Accept only a finite, nonnegative CPU percent; `null` beats a lie.
fn sane_percent(value: f64) -> Option<f64> {
    if value.is_finite() && value >= 0.0 {
        Some(value)
    } else {
        None
    }
}

/// Accept only a finite, nonnegative byte count that fits below the
/// `u64` ceiling; the exclusive upper bound keeps the cast from ever
/// saturating a nonsense value into `u64::MAX`.
fn sane_bytes_f64(value: f64) -> Option<u64> {
    if value.is_finite() && value >= 0.0 && value < u64::MAX as f64 {
        Some(value.round() as u64)
    } else {
        None
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::projects::SourceSummary;

    fn test_summary() -> SourceSummary {
        SourceSummary {
            files: vec!["app/main.go".to_owned(), "go.mod".to_owned()],
            total_bytes: 2048,
            containerfile: "FROM scratch\n".to_owned(),
            source_digest: "a".repeat(64),
        }
    }

    fn installed(dir: &std::path::Path, mark_built: bool) -> Project {
        let store = Store::new(dir.join("projects"));
        let mut project = store
            .create(
                manifest::bundled_manifest().expect("bundled manifest"),
                test_summary(),
            )
            .expect("create");
        if mark_built {
            project.phase = Phase::Installed;
            project.built_image_id = Some(format!("sha256:{}", "b".repeat(64)));
            store.save(&project).expect("mark installed");
        }
        store.load(&project.id).expect("reload")
    }

    fn ps_row(id: &str, name: &str, ownership: Option<&str>, state: &str) -> PsRow {
        let labels = ownership
            .map(|value| HashMap::from([(pocket::OWNERSHIP_LABEL.to_owned(), value.to_owned())]));
        PsRow {
            id: id.to_owned(),
            names: PsNames::Many(vec![name.to_owned()]),
            labels,
            state: state.to_owned(),
        }
    }

    fn target(id: &str, container_name: &str, ownership: &str) -> PocketTarget {
        PocketTarget {
            id: id.to_owned(),
            name: format!("{id} name"),
            container_name: container_name.to_owned(),
            ownership: ownership.to_owned(),
        }
    }

    const ID: &str = "c15e1e008c3f7a888a837e8fea4958b7e934ef09d0082fe1749a0b88fb5b6029";

    #[test]
    fn bundled_identity_matches_manifest_and_lifecycle_constants() {
        let manifest = manifest::bundled_manifest().expect("bundled manifest");
        assert_eq!(
            BUNDLED_ID, manifest.metadata.id,
            "contract id is the manifest id"
        );
        assert_eq!(
            pocket::CONTAINER_NAME,
            format!("pockets-{BUNDLED_ID}"),
            "bundled container name derives from the same id"
        );
        assert_eq!(pocket::OWNERSHIP_VALUE, BUNDLED_ID);
    }

    #[test]
    fn installed_targets_exclude_drafts_and_carry_manifest_names() {
        let dir = tempfile::TempDir::new().expect("temp root");
        let draft = installed(dir.path(), false);
        let built = installed(dir.path(), true);

        let store = Store::new(dir.path().join("projects"));
        let all = store.list().expect("list");
        let targets = installed_targets(&all);

        assert_eq!(
            targets.iter().map(|t| t.id.as_str()).collect::<Vec<_>>(),
            vec![built.id.as_str()],
            "only installed records are monitored"
        );
        assert_eq!(targets[0].name, built.manifest.metadata.name);
        assert_eq!(
            targets[0].container_name,
            format!("{}{}", pocket::MANAGED_NAME_PREFIX, built.id)
        );
        assert_eq!(
            targets[0].ownership,
            format!("{}{}", pocket::MANAGED_OWNERSHIP_PREFIX, built.id)
        );
        assert_ne!(draft.phase, Phase::Installed);
    }

    #[test]
    fn classify_missing_container_is_stopped_with_real_zeros() {
        let targets = vec![target("hello", "pockets-hello-pocket", "hello-pocket")];
        let mut warnings = Vec::new();
        let (usage, running) = classify(&[], &targets, &mut warnings);
        assert_eq!(usage[0].state, MonitorState::Stopped);
        assert_eq!(usage[0].cpu_percent, Some(0.0));
        assert_eq!(usage[0].memory_bytes, Some(0));
        assert!(running.is_empty());
        assert!(warnings.is_empty());
    }

    #[test]
    fn classify_owned_states() {
        let targets = vec![target("hello", "pockets-hello-pocket", "hello-pocket")];
        let mut warnings = Vec::new();

        // Owned and running: collected for stats by full id, metrics pending.
        let rows = vec![ps_row(
            ID,
            "pockets-hello-pocket",
            Some("hello-pocket"),
            "running",
        )];
        let (usage, running) = classify(&rows, &targets, &mut warnings);
        assert_eq!(usage[0].state, MonitorState::Running);
        assert_eq!(usage[0].cpu_percent, None);
        assert_eq!(running, vec![(0, ID.to_owned())]);

        // Exited and stopped: stopped with zeros, no stats request.
        for state in ["exited", "stopped"] {
            let rows = vec![ps_row(
                ID,
                "pockets-hello-pocket",
                Some("hello-pocket"),
                state,
            )];
            let (usage, running) = classify(&rows, &targets, &mut warnings);
            assert_eq!(usage[0].state, MonitorState::Stopped, "{state}");
            assert_eq!(usage[0].memory_bytes, Some(0));
            assert!(running.is_empty());
        }

        // Any other state must not be claimed as running.
        for state in ["paused", "created", "exiting", "removing"] {
            let rows = vec![ps_row(
                ID,
                "pockets-hello-pocket",
                Some("hello-pocket"),
                state,
            )];
            let mut scoped = Vec::new();
            let (usage, running) = classify(&rows, &targets, &mut scoped);
            assert_eq!(usage[0].state, MonitorState::Unavailable, "{state}");
            assert_eq!(usage[0].cpu_percent, None);
            assert!(running.is_empty());
            assert_eq!(scoped.len(), 1, "{state} warns once");
        }

        // A slash-prefixed name still matches.
        let rows = vec![ps_row(
            ID,
            "/pockets-hello-pocket",
            Some("hello-pocket"),
            "running",
        )];
        let (usage, running) = classify(&rows, &targets, &mut warnings);
        assert_eq!(usage[0].state, MonitorState::Running);
        assert_eq!(running, vec![(0, ID.to_owned())]);
    }

    #[test]
    fn classify_foreign_collision_is_unavailable_not_stopped() {
        let targets = vec![target("hello", "pockets-hello-pocket", "hello-pocket")];

        // Same label key, foreign value.
        let rows = vec![ps_row(
            ID,
            "pockets-hello-pocket",
            Some("someone-else"),
            "running",
        )];
        let mut warnings = Vec::new();
        let (usage, running) = classify(&rows, &targets, &mut warnings);
        assert_eq!(usage[0].state, MonitorState::Unavailable);
        assert_eq!(usage[0].memory_bytes, None);
        assert!(
            running.is_empty(),
            "a foreign container is never stats-collected"
        );
        assert_eq!(warnings.len(), 1);

        // No Pockets label at all.
        let rows = vec![ps_row(ID, "pockets-hello-pocket", None, "exited")];
        let mut warnings = Vec::new();
        let (usage, _) = classify(&rows, &targets, &mut warnings);
        assert_eq!(
            usage[0].state,
            MonitorState::Unavailable,
            "a foreign stopped container must not read as ours"
        );
        assert_eq!(warnings.len(), 1);

        // Another project's ownership value is foreign for this target.
        let targets = vec![target(
            &"a".repeat(32),
            &format!("{}{}", pocket::MANAGED_NAME_PREFIX, "a".repeat(32)),
            &format!("{}{}", pocket::MANAGED_OWNERSHIP_PREFIX, "a".repeat(32)),
        )];
        let rows = vec![ps_row(
            ID,
            &format!("{}{}", pocket::MANAGED_NAME_PREFIX, "a".repeat(32)),
            Some(&format!(
                "{}{}",
                pocket::MANAGED_OWNERSHIP_PREFIX,
                "b".repeat(32)
            )),
            "running",
        )];
        let mut warnings = Vec::new();
        let (usage, running) = classify(&rows, &targets, &mut warnings);
        assert_eq!(usage[0].state, MonitorState::Unavailable);
        assert!(running.is_empty());
    }

    #[test]
    fn classify_unsafe_container_id_forfeits_stats_but_not_running_state() {
        let targets = vec![target("hello", "pockets-hello-pocket", "hello-pocket")];
        let rows = vec![ps_row(
            "not a safe id; --rm",
            "pockets-hello-pocket",
            Some("hello-pocket"),
            "running",
        )];
        let mut warnings = Vec::new();
        let (usage, running) = classify(&rows, &targets, &mut warnings);
        assert_eq!(usage[0].state, MonitorState::Running);
        assert!(running.is_empty(), "the malformed id never reaches stats");
        assert_eq!(warnings.len(), 1);
    }

    #[test]
    fn stats_rows_parse_raw_numeric_lines() {
        let stdout = format!(
            "{{\"ContainerID\":\"{ID}\",\"Name\":\"pockets-hello-pocket\",\"CPU\":0.014,\"MemUsage\":17633280,\"MemPerc\":0.86}}\n\n\
             {{\"ContainerID\":\"aaaa\",\"Name\":\"other\",\"CPU\":\"1.2%\",\"MemUsage\":\"17MiB / 2GiB\"}}\n"
        );
        let samples = parse_stats_rows(stdout.as_bytes()).expect("valid lines parse");
        assert_eq!(samples.len(), 2);
        assert_eq!(samples[ID].cpu_percent, Some(0.014));
        assert_eq!(samples[ID].memory_bytes, Some(17633280));
        // Rounded display strings from another engine are unusable, not zero.
        assert_eq!(samples["aaaa"].cpu_percent, None);
        assert_eq!(samples["aaaa"].memory_bytes, None);
    }

    #[test]
    fn stats_rows_reject_broken_output_and_bad_values() {
        // Unparseable line: whole read fails visibly.
        assert!(parse_stats_rows(b"{\"ContainerID\":\"abc\"\nnot json\n").is_err());
        // Row without a usable id: whole read fails visibly.
        assert!(parse_stats_rows(b"{\"CPU\":1}").is_err());
        // Negative and non-finite values null out per field.
        let samples = parse_stats_rows(
            b"{\"ContainerID\":\"abc\",\"CPU\":-3,\"MemUsage\":18446744073709551616}",
        )
        .expect("line parses");
        assert_eq!(
            samples["abc"],
            StatsSample {
                cpu_percent: None,
                memory_bytes: None
            }
        );
    }

    #[test]
    fn apply_stats_nulls_incomplete_samples_with_warnings() {
        let mut warnings = Vec::new();
        let mut usage = vec![
            PocketUsage {
                id: "a".into(),
                name: "A".into(),
                state: MonitorState::Running,
                cpu_percent: None,
                memory_bytes: None,
            },
            PocketUsage {
                id: "b".into(),
                name: "B".into(),
                state: MonitorState::Running,
                cpu_percent: None,
                memory_bytes: None,
            },
        ];
        let running = vec![(0, ID.to_owned()), (1, "bbbb".to_owned())];
        let mut samples = HashMap::new();
        // A: only memory is usable. B has no row at all.
        samples.insert(
            ID.to_owned(),
            StatsSample {
                cpu_percent: None,
                memory_bytes: Some(123),
            },
        );

        apply_stats_samples(&mut usage, &running, &samples, &mut warnings);

        assert_eq!(usage[0].cpu_percent, None);
        assert_eq!(usage[0].memory_bytes, Some(123));
        assert_eq!(usage[1].cpu_percent, None);
        assert_eq!(usage[1].memory_bytes, None);
        assert_eq!(warnings.len(), 2, "one per missing fact, got {warnings:?}");
    }

    #[test]
    fn totals_never_silently_zero_incomplete_fields() {
        let pocket = |state, cpu, mem| PocketUsage {
            id: "p".into(),
            name: "P".into(),
            state,
            cpu_percent: cpu,
            memory_bytes: mem,
        };

        // Stopped contributes exact zeros; complete running values sum.
        let totals = aggregate(&[
            pocket(MonitorState::Running, Some(1.5), Some(100)),
            pocket(MonitorState::Stopped, Some(0.0), Some(0)),
        ]);
        assert_eq!(totals.cpu_percent, Some(1.5));
        assert_eq!(totals.memory_bytes, Some(100));

        // Fields are independent: one pocket lacking memory nulls only
        // the memory total, while a second unavailable pocket nulls both.
        let totals = aggregate(&[
            pocket(MonitorState::Running, Some(1.5), None),
            pocket(MonitorState::Running, Some(2.5), Some(50)),
        ]);
        assert_eq!(totals.cpu_percent, Some(4.0));
        assert_eq!(totals.memory_bytes, None);
        let totals = aggregate(&[
            pocket(MonitorState::Running, Some(1.5), Some(10)),
            pocket(MonitorState::Unavailable, None, None),
        ]);
        assert_eq!(totals.cpu_percent, None);
        assert_eq!(totals.memory_bytes, None);
    }

    #[test]
    fn totals_overflow_and_nonfinite_sums_become_null() {
        let running = |cpu, mem| PocketUsage {
            id: "p".into(),
            name: "P".into(),
            state: MonitorState::Running,
            cpu_percent: cpu,
            memory_bytes: mem,
        };

        // A memory sum beyond u64 must null, never wrap into nonsense;
        // the independent CPU field still sums normally.
        let totals = aggregate(&[
            running(Some(1.0), Some(u64::MAX)),
            running(Some(1.0), Some(1)),
        ]);
        assert_eq!(
            totals.memory_bytes, None,
            "an overflowing memory total must not report a wrapped number"
        );
        assert_eq!(totals.cpu_percent, Some(2.0));

        // A CPU sum that reaches infinity is non-finite and must null
        // out; the memory field keeps summing exact zeros.
        let totals = aggregate(&[
            running(Some(f64::MAX), Some(0)),
            running(Some(f64::MAX), Some(0)),
        ]);
        assert_eq!(
            totals.cpu_percent, None,
            "a non-finite CPU total must be null, not infinity"
        );
        assert_eq!(totals.memory_bytes, Some(0));
    }

    #[test]
    fn incomplete_store_inventory_nulls_totals_but_keeps_hello_row() {
        let hello = PocketUsage {
            id: BUNDLED_ID.to_owned(),
            name: BUNDLED_NAME.to_owned(),
            state: MonitorState::Running,
            cpu_percent: Some(2.0),
            memory_bytes: Some(1024),
        };

        // With a complete inventory the visible rows sum as usual.
        let totals = totals_for(true, std::slice::from_ref(&hello));
        assert_eq!(totals.cpu_percent, Some(2.0));
        assert_eq!(totals.memory_bytes, Some(1024));

        // Store unreadable: the pocket list is knowingly partial (Hello
        // only), so the totals must not claim to cover every Pocket even
        // though the single visible row has complete metrics.
        let totals = totals_for(false, std::slice::from_ref(&hello));
        assert_eq!(totals.cpu_percent, None);
        assert_eq!(totals.memory_bytes, None);

        // Even an all-zero partial list must not present itself as the
        // real total.
        let stopped = PocketUsage {
            state: MonitorState::Stopped,
            cpu_percent: Some(0.0),
            memory_bytes: Some(0),
            ..hello
        };
        let totals = totals_for(false, std::slice::from_ref(&stopped));
        assert_eq!(totals.cpu_percent, None);
        assert_eq!(totals.memory_bytes, None);
    }

    #[test]
    fn snapshot_contract_uses_camel_case_field_names() {
        let snapshot = ResourceSnapshot {
            sampled_at_ms: 1795064828687,
            pockets: vec![PocketUsage {
                id: BUNDLED_ID.into(),
                name: BUNDLED_NAME.into(),
                state: MonitorState::Running,
                cpu_percent: None,
                memory_bytes: Some(7),
            }],
            totals: Totals {
                cpu_percent: None,
                memory_bytes: Some(7),
            },
            warnings: vec!["w".into()],
        };
        let value = serde_json::to_value(&snapshot).expect("serializes");
        // Field *names* are the contract; JSON objects are unordered.
        let sorted_keys = |value: &serde_json::Value| -> Vec<String> {
            let mut keys: Vec<String> =
                value.as_object().expect("object").keys().cloned().collect();
            keys.sort();
            keys
        };
        assert_eq!(
            sorted_keys(&value),
            ["pockets", "sampledAtMs", "totals", "warnings"]
        );
        assert_eq!(value["pockets"][0]["state"], "running");
        assert_eq!(
            sorted_keys(&value["pockets"][0]),
            ["cpuPercent", "id", "memoryBytes", "name", "state"]
        );
        assert_eq!(value["totals"]["cpuPercent"], serde_json::Value::Null);
        assert_eq!(value["totals"]["memoryBytes"], 7);
    }

    #[test]
    fn sanity_guards_reject_impossible_numbers() {
        assert_eq!(sane_percent(0.0), Some(0.0));
        assert_eq!(sane_percent(-0.1), None);
        assert_eq!(sane_percent(f64::NAN), None);
        assert_eq!(sane_percent(f64::INFINITY), None);
        assert_eq!(sane_bytes_f64(0.0), Some(0));
        assert_eq!(sane_bytes_f64(-1.0), None);
        assert_eq!(sane_bytes_f64(f64::INFINITY), None);
    }

    /// Opt-in read-only live check against the real engine: samples
    /// through the production [`sample`] helper with an empty temporary
    /// project store, so the only monitored Pocket is the bundled Hello.
    /// It starts, stops, and removes nothing — Podman is only ever asked
    /// `ps`, plus `stats` for owned running containers — and never touches
    /// the real store.
    #[tokio::test]
    #[ignore = "requires operational Podman (read-only; the bundled Hello may or may not be running)"]
    async fn live_resource_snapshot_is_readable() {
        let dir = tempfile::TempDir::new().expect("temp store root");
        let store = Store::new(dir.path().join("projects"));
        let snapshot = sample(Ok(store)).await;
        println!(
            "{}",
            serde_json::to_string_pretty(&snapshot).expect("snapshot serializes")
        );

        assert_eq!(
            snapshot.pockets.len(),
            1,
            "an empty store leaves only the bundled Hello: {:?}",
            snapshot.pockets
        );
        assert_eq!(snapshot.pockets[0].id, BUNDLED_ID);
        assert!(
            snapshot.warnings.is_empty(),
            "an operational engine must sample cleanly: {:?}",
            snapshot.warnings
        );
    }
}
