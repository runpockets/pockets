//! Podman variable delivery. Only opaque references cross the command line.
//! Engine secrets must outlive the container definition so `restart` works.

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

use serde::Deserialize;

use crate::runtime;
use crate::runtime_settings::{self, Environment};

pub(crate) type Values = BTreeMap<String, String>;

pub(crate) const BINDINGS_LABEL: &str = "dev.pockets.runtime-bindings";
const SCOPE_LABEL: &str = "dev.pockets.runtime";

pub(crate) struct Scope {
    owner: String,
    prefix: String,
}

impl Scope {
    pub(crate) fn new(id: &str, environment: Environment) -> Result<Self, String> {
        crate::projects::validate_project_id(id)?;
        let owner = format!("{}-{id}", environment.name());
        let prefix = format!("pockets-runtime-{owner}-");
        Ok(Self { owner, prefix })
    }

    fn owns_name(&self, name: &str) -> bool {
        name.strip_prefix(&self.prefix)
            .is_some_and(|suffix| crate::projects::validate_project_id(suffix).is_ok())
    }
}

#[derive(Default, Debug, Clone, PartialEq, Eq)]
pub(crate) struct Bindings(BTreeMap<String, Option<String>>);

impl Bindings {
    pub(crate) fn from_label(scope: &Scope, label: Option<&str>) -> Result<Self, String> {
        let Some(label) = label else {
            return Ok(Self::default());
        };
        let invalid = || {
            "The container has invalid Pockets runtime variable references. Stop and start it before updating.".to_owned()
        };
        if label.len() > 16 * 1024 {
            return Err(invalid());
        }
        let variables: BTreeMap<String, Option<String>> =
            serde_json::from_str(label).map_err(|_| invalid())?;
        if variables.len() > runtime_settings::MAX_VARIABLES
            || variables.iter().any(|(name, secret)| {
                runtime_settings::validate_name(name).is_err()
                    || secret
                        .as_ref()
                        .is_some_and(|secret| !scope.owns_name(secret))
            })
        {
            return Err(invalid());
        }
        Ok(Self(variables))
    }

    /// Podman can lose a referenced secret while the app keeps running.
    /// Refuse an update before stopping that app if it could not be restored.
    pub(crate) async fn verify(&self, bin: &Path, scope: &Scope) -> Result<(), String> {
        for name in self.0.values().flatten() {
            if !inspect(bin, name)
                .await
                .is_ok_and(|secret| secret.owned_by(scope, name))
            {
                return Err("The running app’s runtime variables are unavailable in Podman. Stop and start the app before updating.".into());
            }
        }
        Ok(())
    }

    /// Launch a container using bindings only. Podman rejects empty secret
    /// blobs, so an explicitly empty plain variable uses `--env=KEY=`.
    pub(crate) async fn run(&self, bin: &Path, mut args: Vec<String>) -> Result<(), String> {
        if self.0.is_empty() {
            let args: Vec<&str> = args.iter().map(String::as_str).collect();
            return runtime::run(bin, &args).await.map(|_| ());
        }
        let label = serde_json::to_string(&self.0).expect("string map");
        let flags = std::iter::once(format!("--label={BINDINGS_LABEL}={label}")).chain(
            self.0.iter().map(|(name, secret)| match secret {
                Some(secret) => format!("--secret={secret},type=env,target={name}"),
                None => format!("--env={name}="),
            }),
        );
        args.splice(1..1, flags);
        let args: Vec<&str> = args.iter().map(String::as_str).collect();
        if runtime::run_private(bin, &args, None).await {
            Ok(())
        } else {
            Err("Podman could not launch the container with its runtime variables. Check Podman and try again.".into())
        }
    }
}

/// Caller has verified there is no container using the scope. Stage one new
/// generation; a partial failure can be reclaimed by the same scoped cleanup.
pub(crate) async fn prepare(
    bin: &Path,
    scope: &Scope,
    values: &Values,
) -> Result<Bindings, String> {
    let label = format!("{SCOPE_LABEL}={}", scope.owner);
    let mut bindings = BTreeMap::new();
    for (name, value) in values {
        let reference = if value.is_empty() {
            None
        } else {
            let secret = format!("{}{}", scope.prefix, uuid::Uuid::new_v4().simple());
            let args = [
                "secret",
                "create",
                "--driver=file",
                "--label",
                &label,
                &secret,
                "-",
            ];
            if !runtime::run_private(bin, &args, Some(value.as_bytes())).await {
                let cleanup = clear(bin, scope).await;
                let message = "Podman could not prepare this Pocket’s runtime variables.";
                return Err(match cleanup {
                    Ok(()) => message.to_owned(),
                    Err(error) => format!("{message} {error}"),
                });
            }
            Some(secret)
        };
        bindings.insert(name.clone(), reference);
    }
    Ok(Bindings(bindings))
}

#[derive(Deserialize)]
#[serde(rename_all = "PascalCase")]
struct Secret {
    #[serde(rename = "ID")]
    id: String,
    spec: SecretSpec,
}

#[derive(Deserialize)]
#[serde(rename_all = "PascalCase")]
struct SecretSpec {
    name: String,
    labels: Option<BTreeMap<String, String>>,
}

impl Secret {
    fn owned_by(&self, scope: &Scope, name: &str) -> bool {
        self.spec.name == name
            && self
                .spec
                .labels
                .as_ref()
                .and_then(|labels| labels.get(SCOPE_LABEL))
                == Some(&scope.owner)
    }
}

async fn inspect(bin: &Path, reference: &str) -> Result<Secret, ()> {
    let output = runtime::run(bin, &["secret", "inspect", reference])
        .await
        .map_err(|_| ())?;
    let entries: Vec<Secret> = serde_json::from_slice(&output.stdout).map_err(|_| ())?;
    let [entry]: [Secret; 1] = entries.try_into().map_err(|_| ())?;
    Ok(entry)
}

/// Delete only scope-labelled secrets with exact generated names and verified
/// Podman IDs. `secret inspect` does not request value data.
pub(crate) async fn clear(bin: &Path, scope: &Scope) -> Result<(), String> {
    let failed = || {
        "Pockets could not clean up this environment’s Podman credentials. Check Podman and retry."
            .to_owned()
    };
    let filter = format!("name=^{}", scope.prefix);
    let list = runtime::run(
        bin,
        &[
            "secret",
            "ls",
            "--filter",
            &filter,
            "--format",
            "{{.ID}} {{.Name}}",
        ],
    )
    .await
    .map_err(|_| failed())?;
    let list = std::str::from_utf8(&list.stdout).map_err(|_| failed())?;
    for row in list.lines() {
        let Some((id, name)) = row.split_once(' ') else {
            continue;
        };
        if !(12..=128).contains(&id.len())
            || !id.bytes().all(|byte| byte.is_ascii_alphanumeric())
            || !scope.owns_name(name)
        {
            continue;
        }
        let entry = inspect(bin, id).await.map_err(|_| failed())?;
        if entry.id == id
            && entry.owned_by(scope, name)
            && !runtime::run_private(bin, &["secret", "rm", "--ignore", id], None).await
        {
            return Err(failed());
        }
    }
    Ok(())
}

/// Preview teardown can leave labelled secrets without a container after a
/// crash. Installed secrets are reclaimed by explicit start or delete.
pub(crate) async fn sweep_preview_orphans(bin: &Path) -> Vec<String> {
    let Ok(output) = runtime::run(
        bin,
        &[
            "secret",
            "ls",
            "--filter",
            "name=^pockets-runtime-preview-",
            "--format",
            "{{.Name}}",
        ],
    )
    .await
    else {
        return vec!["Pockets could not list orphaned runtime credentials.".into()];
    };
    let names = String::from_utf8_lossy(&output.stdout);
    let mut scopes = BTreeSet::new();
    for name in names.lines() {
        let Some(suffix) = name.strip_prefix("pockets-runtime-preview-") else {
            continue;
        };
        let Some((id, nonce)) = suffix.split_once('-') else {
            continue;
        };
        if crate::projects::validate_project_id(id).is_ok()
            && crate::projects::validate_project_id(nonce).is_ok()
        {
            scopes.insert(id);
        }
    }
    let mut problems = Vec::new();
    for id in scopes {
        let filter = format!("name=^pockets-preview-{id}$");
        match runtime::run(
            bin,
            &["ps", "--all", "--filter", &filter, "--format", "{{.Names}}"],
        )
        .await
        {
            Ok(output) if output.stdout.iter().all(|byte| byte.is_ascii_whitespace()) => {
                let scope = Scope::new(id, Environment::Preview).expect("validated project ID");
                if let Err(error) = clear(bin, &scope).await {
                    problems.push(error);
                }
            }
            Ok(_) => (),
            Err(_) => problems
                .push("Pockets could not verify whether runtime credentials were in use.".into()),
        }
    }
    problems
}
