//! A per-Pocket gateway on two bridges. The app joins only its internal bridge.
//! Gateway code is bundled by Desktop and binds exclusively to that bridge.

use std::collections::{BTreeMap, BTreeSet, HashSet};
use std::net::Ipv4Addr;
use std::path::Path;

use serde::Deserialize;

use crate::{pocket::OWNERSHIP_LABEL, projects::validate_project_id, runtime};

const CONTAINER_PREFIX: &str = "pockets-egress-";
const NETWORK_PREFIX: &str = "pockets-egress-net-";
const GATEWAY: &str = concat!(
    include_str!("assets/preview-egress.cjs"),
    "\nmodule.exports.run(process.argv[1]);"
);
const HEALTH: &str = "async function ready(){ for(let i=0;i<20;i++){try{const r=await fetch(process.argv[1],{signal:AbortSignal.timeout(300)}); if(r.ok)return;}catch{} await new Promise(r=>setTimeout(r,100));}process.exitCode=1;}ready();";

struct Scope {
    container: String,
    network: String,
    owner: String,
}

impl Scope {
    fn new(id: &str) -> Result<Self, String> {
        validate_project_id(id)?;
        Ok(Self {
            container: format!("{CONTAINER_PREFIX}{id}"),
            network: format!("{NETWORK_PREFIX}{id}"),
            owner: format!("egress-{id}"),
        })
    }

    fn label(&self) -> String {
        format!("{OWNERSHIP_LABEL}={}", self.owner)
    }
}

pub(crate) struct Gateway {
    pub network: String,
    pub url: String,
}

#[derive(Deserialize)]
#[serde(rename_all = "PascalCase")]
struct Container {
    id: String,
    name: String,
    config: ContainerConfig,
    network_settings: Option<NetworkSettings>,
}

#[derive(Deserialize)]
#[serde(rename_all = "PascalCase")]
struct ContainerConfig {
    labels: Option<BTreeMap<String, String>>,
}

#[derive(Deserialize)]
#[serde(rename_all = "PascalCase")]
struct NetworkSettings {
    networks: Option<BTreeMap<String, NetworkAttachment>>,
}

#[derive(Deserialize)]
struct NetworkAttachment {
    #[serde(rename = "IPAddress")]
    ip: String,
}

#[derive(Deserialize)]
struct Network {
    id: String,
    name: String,
    driver: String,
    internal: bool,
    dns_enabled: bool,
    labels: Option<BTreeMap<String, String>>,
}

fn owned(labels: &Option<BTreeMap<String, String>>, scope: &Scope) -> bool {
    labels.as_ref().and_then(|map| map.get(OWNERSHIP_LABEL)) == Some(&scope.owner)
}

fn safe_id(id: &str) -> bool {
    id.len() == 64 && id.bytes().all(|byte| byte.is_ascii_hexdigit())
}

async fn inspect_one<T: serde::de::DeserializeOwned>(
    bin: &Path,
    args: &[&str],
) -> Result<T, String> {
    let output = runtime::run(bin, args).await?;
    let [entry]: [T; 1] = serde_json::from_slice(&output.stdout)
        .map_err(|_| "Podman returned unexpected HTTPS gateway metadata.")?;
    Ok(entry)
}

async fn remove_owned(bin: &Path, scope: &Scope) -> Result<(), String> {
    let filter = format!("name=^{}$", scope.container);
    let rows = runtime::run(
        bin,
        &["ps", "--all", "--filter", &filter, "--format", "{{.Names}}"],
    )
    .await?;
    if String::from_utf8_lossy(&rows.stdout)
        .lines()
        .any(|line| line.trim_start_matches('/') == scope.container)
    {
        let container: Container =
            inspect_one(bin, &["container", "inspect", &scope.container]).await?;
        if container.name.trim_start_matches('/') != scope.container
            || !safe_id(&container.id)
            || !owned(&container.config.labels, scope)
        {
            return Err("Pockets refused to remove a foreign HTTPS gateway container.".into());
        }
        runtime::run(bin, &["rm", "--force", "--time", "0", &container.id]).await?;
    }

    let filter = format!("name=^{}$", scope.network);
    let rows = runtime::run(
        bin,
        &[
            "network",
            "ls",
            "--filter",
            &filter,
            "--format",
            "{{.Name}}",
        ],
    )
    .await?;
    if String::from_utf8_lossy(&rows.stdout)
        .lines()
        .any(|line| line == scope.network)
    {
        let network: Network = inspect_one(bin, &["network", "inspect", &scope.network]).await?;
        if network.name != scope.network || !safe_id(&network.id) || !owned(&network.labels, scope)
        {
            return Err("Pockets refused to remove a foreign HTTPS gateway network.".into());
        }
        runtime::run(bin, &["network", "rm", &network.id]).await?;
    }
    Ok(())
}

pub(crate) async fn stop(bin: &Path, id: &str) -> Result<(), String> {
    remove_owned(bin, &Scope::new(id)?).await
}

pub(crate) async fn start(
    bin: &Path,
    id: &str,
    hosts: &[String],
    image: &str,
) -> Result<Gateway, String> {
    let scope = Scope::new(id)?;
    let result = provision(bin, &scope, hosts, image).await;
    if let Err(error) = result {
        return match remove_owned(bin, &scope).await {
            Ok(()) => Err(error),
            Err(cleanup) => Err(format!("{error} Gateway cleanup also failed. {cleanup}")),
        };
    }
    result
}

async fn provision(
    bin: &Path,
    scope: &Scope,
    hosts: &[String],
    image: &str,
) -> Result<Gateway, String> {
    let label = scope.label();
    runtime::run(
        bin,
        &[
            "network",
            "create",
            "--driver",
            "bridge",
            "--internal",
            "--disable-dns",
            "--label",
            &label,
            &scope.network,
        ],
    )
    .await?;
    let network: Network = inspect_one(bin, &["network", "inspect", &scope.network]).await?;
    if network.name != scope.network
        || network.driver != "bridge"
        || !network.internal
        || network.dns_enabled
        || !owned(&network.labels, scope)
    {
        return Err("Podman did not create the expected isolated HTTPS gateway network.".into());
    }

    let attachment = format!("{}:interface_name=pockets-in", scope.network);
    let hosts = serde_json::to_string(hosts).expect("hosts are strings");
    let args = [
        "run",
        "--detach",
        "--pull=never",
        "--name",
        &scope.container,
        "--label",
        &label,
        "--network",
        &attachment,
        "--network",
        "podman",
        "--memory=192m",
        "--pids-limit=64",
        "--read-only",
        "--cap-drop=ALL",
        "--security-opt=no-new-privileges",
        "--user=1000:1000",
        image,
        "node",
        "-e",
        GATEWAY,
        &hosts,
    ];
    // Suppress diagnostics that would otherwise echo the bundled program.
    if !runtime::run_private(bin, &args, None).await {
        return Err("Podman could not start the HTTPS gateway. Check that its standard podman bridge is available.".into());
    }
    let container: Container =
        inspect_one(bin, &["container", "inspect", &scope.container]).await?;
    if container.name.trim_start_matches('/') != scope.container
        || !safe_id(&container.id)
        || !owned(&container.config.labels, scope)
    {
        return Err("Podman did not create the expected HTTPS gateway container.".into());
    }
    let address = container
        .network_settings
        .and_then(|net| net.networks)
        .and_then(|networks| networks.get(&scope.network)?.ip.parse::<Ipv4Addr>().ok())
        .filter(|ip| !ip.is_unspecified() && !ip.is_loopback())
        .ok_or("The HTTPS gateway has no address on the isolated bridge.")?;
    let url = format!("http://{address}:3137");
    if !runtime::run_private(
        bin,
        &[
            "exec",
            &container.id,
            "node",
            "-e",
            HEALTH,
            &format!("{url}/health"),
        ],
        None,
    )
    .await
    {
        return Err("The HTTPS gateway did not become ready on the isolated bridge.".into());
    }
    Ok(Gateway {
        network: scope.network.clone(),
        url,
    })
}

pub(crate) async fn sweep(bin: &Path, protected: &HashSet<String>) -> Vec<String> {
    let mut issues = Vec::new();
    let mut ids = BTreeSet::new();
    let networks = runtime::run(
        bin,
        &[
            "network",
            "ls",
            "--filter",
            "name=^pockets-egress-net-",
            "--format",
            "{{.Name}}",
        ],
    )
    .await;
    let containers = runtime::run(
        bin,
        &[
            "ps",
            "--all",
            "--filter",
            "name=^pockets-egress-",
            "--format",
            "{{.Names}}",
        ],
    )
    .await;
    for (output, prefix) in [(networks, NETWORK_PREFIX), (containers, CONTAINER_PREFIX)] {
        match output {
            Ok(output) => {
                for name in String::from_utf8_lossy(&output.stdout).lines() {
                    if let Some(id) = name.trim_start_matches('/').strip_prefix(prefix) {
                        if validate_project_id(id).is_ok() && !protected.contains(id) {
                            ids.insert(id.to_owned());
                        }
                    }
                }
            }
            Err(error) => issues.push(error),
        }
    }
    for id in ids {
        if let Err(error) = stop(bin, &id).await {
            issues.push(error);
        }
    }
    issues
}
