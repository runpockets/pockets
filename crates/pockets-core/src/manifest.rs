//! Parsing and validation for the `pockets.dev/v1alpha1` Pocket manifest.
//!
//! Implements the experimental M1 subset documented in `docs/pocket-spec.md`
//! and `packages/pocket-spec/pocket.schema.json`. Structural rules (required
//! fields, unknown-field rejection, scalar types) are enforced by serde;
//! value constraints are enforced by [`Manifest::validate`]. Host
//! architecture matching is kept out of pure parsing in
//! [`Manifest::validate_host`].

use serde::{Deserialize, Serialize};
use std::collections::BTreeSet;

/// The only accepted value of `apiVersion`.
pub const API_VERSION: &str = "pockets.dev/v1alpha1";

/// The only accepted value of `kind`.
pub const KIND: &str = "Pocket";

/// Maximum length of an ID or storage name, per the schema `maxLength`.
pub(crate) const MAX_ID_LEN: usize = 63;
pub(crate) const MAX_PREVIEW_APIS: usize = 16;

/// The fixed bundled first-party manifest. M1 restricts execution to exactly
/// this manifest, so it is embedded at compile time.
const BUNDLED_MANIFEST: &str = include_str!("../../../examples/hello-pocket/pocket.yaml");

/// A parsed Pocket manifest.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Manifest {
    #[serde(rename = "apiVersion")]
    pub api_version: String,
    pub kind: String,
    pub metadata: Metadata,
    pub runtime: Runtime,
    pub web: Web,
    pub storage: Storage,
    #[serde(rename = "previewApis", default, skip_serializing_if = "Vec::is_empty")]
    pub preview_apis: Vec<PreviewApi>,
    #[serde(
        rename = "runtimeVariables",
        default,
        skip_serializing_if = "Vec::is_empty"
    )]
    pub runtime_variables: Vec<RuntimeVariable>,
}

/// Source-controlled declaration of an environment variable accepted by the
/// Pocket. Values never appear in the manifest; Pockets stores them locally per
/// environment.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RuntimeVariable {
    pub name: String,
    pub kind: crate::runtime_settings::VariableKind,
    pub purpose: String,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub required: bool,
}

/// Source-controlled declaration. Desktop checks its separate per-folder
/// authorization before any of these hosts can be used.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PreviewApi {
    pub host: String,
    pub name: String,
    pub purpose: String,
}

pub(crate) fn validate_preview_host(host: &str) -> Result<(), String> {
    let valid_labels = host.split('.').all(|label| {
        let alphanumeric = |c: char| c.is_ascii_lowercase() || c.is_ascii_digit();
        label.len() <= 63
            && label.starts_with(alphanumeric)
            && label.ends_with(alphanumeric)
            && label
                .bytes()
                .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == b'-')
    });
    if host.len() > 253
        || !valid_labels
        || !host.rsplit_once('.').is_some_and(|(_, tld)| {
            tld.starts_with(|c: char| c.is_ascii_lowercase())
                && !matches!(tld, "localhost" | "local" | "internal" | "test" | "invalid")
        })
        || host.ends_with(".home.arpa")
    {
        return Err("Use exact public hostnames such as api.example.com. Omit schemes, ports, paths, and wildcards.".into());
    }
    Ok(())
}

fn is_hidden_display_char(c: char) -> bool {
    c.is_control()
        || matches!(
            c,
            '\u{061c}' | '\u{200e}'..='\u{200f}' | '\u{2028}'..='\u{202e}' | '\u{2066}'..='\u{2069}'
        )
}

fn validate_display_line(field: &str, value: &str, limit: usize) -> Result<(), String> {
    if value.trim().is_empty()
        || value.chars().count() > limit
        || value.chars().any(is_hidden_display_char)
    {
        return Err(format!(
            "{field} must be a single line of up to {limit} characters."
        ));
    }
    Ok(())
}

fn validate_preview_apis(apis: &[PreviewApi]) -> Result<(), String> {
    if apis.len() > MAX_PREVIEW_APIS {
        return Err(format!(
            "previewApis supports up to {MAX_PREVIEW_APIS} hostnames."
        ));
    }
    let mut hosts = BTreeSet::new();
    for api in apis {
        validate_preview_host(&api.host)
            .map_err(|error| format!("Invalid previewApis.host. {error}"))?;
        if !hosts.insert(&api.host) {
            return Err("Each previewApis hostname must appear only once.".into());
        }
        validate_display_line("previewApis.name", &api.name, 80)?;
        validate_display_line("previewApis.purpose", &api.purpose, 240)?;
    }
    Ok(())
}

fn validate_runtime_variables(variables: &[RuntimeVariable]) -> Result<(), String> {
    let max = crate::runtime_settings::MAX_VARIABLES;
    if variables.len() > max {
        return Err(format!("runtimeVariables supports up to {max} variables."));
    }
    let mut names = BTreeSet::new();
    for variable in variables {
        crate::runtime_settings::validate_name(&variable.name)
            .map_err(|error| format!("Invalid runtimeVariables.name. {error}"))?;
        if !names.insert(&variable.name) {
            return Err("Each runtimeVariables name must appear only once.".into());
        }
        validate_display_line("runtimeVariables.purpose", &variable.purpose, 240)?;
    }
    Ok(())
}

/// Identity and display metadata. All strings must be nonempty.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Metadata {
    pub id: String,
    pub name: String,
    pub version: String,
    pub description: String,
}

/// Container runtime reference. A manifest never supplies a host port, host
/// mount, executable, arbitrary command, environment secret, or privilege
/// flag; the schema rejects any attempt via `deny_unknown_fields`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Runtime {
    pub image: String,
    pub architectures: Vec<Architecture>,
}

/// Web exposure of the single HTTP container.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Web {
    pub port: u16,
    pub healthcheck: String,
}

/// The single engine-managed storage mount.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Storage {
    pub name: String,
    pub path: String,
}

/// Architectures supported by the runtime. The schema enum is the only way to
/// name an architecture, so unsupported values fail deserialization.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Architecture {
    #[serde(rename = "arm64")]
    Arm64,
    #[serde(rename = "amd64")]
    Amd64,
}

impl Architecture {
    /// The manifest spelling of this architecture.
    pub fn as_str(self) -> &'static str {
        match self {
            Architecture::Arm64 => "arm64",
            Architecture::Amd64 => "amd64",
        }
    }

    /// The architecture of the current host, mapped from
    /// `std::env::consts::ARCH`. Errors for anything other than
    /// `aarch64` (arm64) or `x86_64` (amd64).
    pub fn from_host() -> Result<Self, String> {
        match std::env::consts::ARCH {
            "aarch64" => Ok(Architecture::Arm64),
            "x86_64" => Ok(Architecture::Amd64),
            other => Err(format!("unsupported host architecture: {other}")),
        }
    }
}

/// Parses and validates the fixed bundled manifest.
pub fn bundled_manifest() -> Result<Manifest, String> {
    parse_manifest(BUNDLED_MANIFEST)
}

/// Parses manifest YAML and applies all documented value constraints.
pub fn parse_manifest(yaml: &str) -> Result<Manifest, String> {
    let manifest: Manifest =
        serde_yaml_ng::from_str(yaml).map_err(|err| format!("invalid manifest: {err}"))?;
    manifest.validate()?;
    Ok(manifest)
}

impl Manifest {
    /// Enforces the value constraints documented in `docs/pocket-spec.md`.
    pub fn validate(&self) -> Result<(), String> {
        if self.api_version != API_VERSION {
            return Err(format!(
                "apiVersion must be {API_VERSION}, found {:?}",
                self.api_version
            ));
        }
        if self.kind != KIND {
            return Err(format!("kind must be {KIND}, found {:?}", self.kind));
        }

        validate_id("metadata.id", &self.metadata.id)?;
        validate_nonempty("metadata.name", &self.metadata.name)?;
        validate_nonempty("metadata.version", &self.metadata.version)?;
        validate_nonempty("metadata.description", &self.metadata.description)?;

        validate_image("runtime.image", &self.runtime.image)?;
        validate_architectures(&self.runtime.architectures)?;

        if self.web.port == 0 {
            return Err("web.port must be between 1 and 65535".to_string());
        }
        validate_healthcheck("web.healthcheck", &self.web.healthcheck)?;

        validate_id("storage.name", &self.storage.name)?;
        validate_storage_path("storage.path", &self.storage.path)?;
        validate_preview_apis(&self.preview_apis)?;
        validate_runtime_variables(&self.runtime_variables)?;

        Ok(())
    }

    /// Checks that the manifest supports the current host architecture.
    /// Separate from [`Manifest::validate`] so parsing stays
    /// architecture-neutral.
    pub fn validate_host(&self) -> Result<(), String> {
        let host = Architecture::from_host()?;
        if self.runtime.architectures.contains(&host) {
            Ok(())
        } else {
            let supported: Vec<&str> = self
                .runtime
                .architectures
                .iter()
                .map(|arch| arch.as_str())
                .collect();
            Err(format!(
                "manifest supports only [{}], host is {}",
                supported.join(", "),
                host.as_str()
            ))
        }
    }
}

/// Schema `^[a-z0-9]+(-[a-z0-9]+)*$` with `maxLength` 63: lowercase ASCII
/// alphanumeric segments joined by single hyphens.
pub(crate) fn validate_id(field: &str, value: &str) -> Result<(), String> {
    let valid = !value.is_empty()
        && value.len() <= MAX_ID_LEN
        && !value.starts_with('-')
        && !value.ends_with('-')
        && !value.contains("--")
        && value
            .chars()
            .all(|ch| ch.is_ascii_lowercase() || ch.is_ascii_digit() || ch == '-');
    if valid {
        Ok(())
    } else {
        Err(format!(
            "{field} must be at most {MAX_ID_LEN} characters of lowercase ASCII \
             alphanumeric words separated by single hyphens, found {value:?}"
        ))
    }
}

fn validate_nonempty(field: &str, value: &str) -> Result<(), String> {
    if value.is_empty() {
        Err(format!("{field} must not be empty"))
    } else {
        Ok(())
    }
}

/// Schema `^[^\s-][^\s]*$`: nonempty, no whitespace, no leading hyphen.
fn validate_image(field: &str, value: &str) -> Result<(), String> {
    if value.is_empty() {
        return Err(format!("{field} must not be empty"));
    }
    if value.starts_with('-') {
        return Err(format!(
            "{field} must not start with a hyphen, found {value:?}"
        ));
    }
    if value.chars().any(char::is_whitespace) {
        return Err(format!(
            "{field} must not contain whitespace, found {value:?}"
        ));
    }
    Ok(())
}

/// Nonempty, unique list; membership in the supported set is guaranteed by
/// the [`Architecture`] enum during deserialization.
fn validate_architectures(values: &[Architecture]) -> Result<(), String> {
    if values.is_empty() {
        return Err("runtime.architectures must not be empty".to_string());
    }
    for (index, arch) in values.iter().enumerate() {
        if values[index + 1..].contains(arch) {
            return Err(format!(
                "runtime.architectures must be unique, {} appears twice",
                arch.as_str()
            ));
        }
    }
    Ok(())
}

/// Schema `^/(?!/)[^\s?#]*$`: one leading slash, no whitespace, no query or
/// fragment.
fn validate_healthcheck(field: &str, value: &str) -> Result<(), String> {
    if !value.starts_with('/') || value.get(1..2) == Some("/") {
        return Err(format!(
            "{field} must begin with exactly one slash, found {value:?}"
        ));
    }
    if value.chars().any(char::is_whitespace) {
        return Err(format!(
            "{field} must not contain whitespace, found {value:?}"
        ));
    }
    if value.contains('?') || value.contains('#') {
        return Err(format!(
            "{field} must not contain a query string or fragment, found {value:?}"
        ));
    }
    Ok(())
}

/// Absolute container path of ordinary alphanumeric, underscore, dot, or
/// hyphen segments, with `.` and `..` segments prohibited.
fn validate_storage_path(field: &str, value: &str) -> Result<(), String> {
    let path_error = || {
        format!(
            "{field} must be an absolute container path of ordinary \
                                alphanumeric, underscore, dot, or hyphen segments without \
                                '.' or '..' segments, found {value:?}"
        )
    };
    let Some(rest) = value.strip_prefix('/') else {
        return Err(path_error());
    };
    let mut segments = rest.split('/').peekable();
    // A leading "//" leaves an empty first segment; the path must also have
    // at least one segment and no trailing slash.
    while let Some(segment) = segments.next() {
        let is_last = segments.peek().is_none();
        let ordinary = !segment.is_empty()
            && segment != "."
            && segment != ".."
            && segment
                .chars()
                .all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '_' | '.' | '-'));
        if !ordinary {
            return Err(if is_last && segment.is_empty() && !value.is_empty() {
                format!("{field} must not end with a slash, found {value:?}")
            } else {
                path_error()
            });
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bundled_manifest_is_valid() {
        let manifest = bundled_manifest().expect("bundled manifest must parse");
        assert_eq!(manifest.api_version, API_VERSION);
        assert_eq!(manifest.metadata.id, "hello-pocket");
        assert_eq!(manifest.web.port, 8080);
        assert_eq!(
            manifest.runtime.architectures,
            vec![Architecture::Arm64, Architecture::Amd64]
        );
        // The bundled manifest lists both supported architectures, so the
        // host check must pass on every machine the desktop app runs on.
        manifest
            .validate_host()
            .expect("bundled manifest must support the host architecture");
    }

    #[test]
    fn unknown_fields_are_rejected() {
        let mutated = BUNDLED_MANIFEST.replace("  port: 8080", "  port: 8080\n  hostPort: 1234");
        assert_ne!(mutated, BUNDLED_MANIFEST, "mutation must apply");
        let err = parse_manifest(&mutated).expect_err("hostPort must be rejected");
        assert!(err.contains("hostPort"), "unexpected error: {err}");
    }

    #[test]
    fn security_sensitive_mutations_are_rejected() {
        let cases = [
            // Host path traversal through the container mount path.
            ("path: /data", "path: /../../etc/passwd"),
            ("path: /data", "path: /data/../secrets"),
            // Query strings could smuggle credentials into health probes.
            ("healthcheck: /health", "healthcheck: /health?token=abc"),
            // A leading hyphen could be parsed as a CLI argument.
            (
                "image: localhost/pockets-hello:0.1.0",
                "image: -localhost/pockets-hello:0.1.0",
            ),
            // Port must be 1..=65535.
            ("port: 8080", "port: 0"),
            ("port: 8080", "port: 65536"),
            // Architecture list must be unique.
            (
                "architectures: [arm64, amd64]",
                "architectures: [arm64, arm64]",
            ),
            // IDs must be lowercase ASCII with no underscores.
            ("id: hello-pocket", "id: Hello_Pocket"),
        ];
        for (from, to) in cases {
            let mutated = BUNDLED_MANIFEST.replace(from, to);
            assert_ne!(mutated, BUNDLED_MANIFEST, "mutation must apply: {from}");
            let result = parse_manifest(&mutated);
            assert!(result.is_err(), "must be rejected {to:?}, got {result:?}");
        }
    }

    #[test]
    fn runtime_variables_manifest_validation() {
        let valid = format!(
            "{BUNDLED_MANIFEST}\nruntimeVariables:\n  - name: PRICING_API_KEY\n    kind: secret\n    required: true\n    purpose: Authenticates market data lookups.\n  - name: CURRENCY\n    kind: plain\n    purpose: Default display currency.\n"
        );
        let parsed = parse_manifest(&valid).expect("valid runtimeVariables must parse");
        assert_eq!(parsed.runtime_variables.len(), 2);
        assert_eq!(parsed.runtime_variables[0].name, "PRICING_API_KEY");
        assert!(parsed.runtime_variables[0].required);
        assert_eq!(parsed.runtime_variables[1].name, "CURRENCY");
        assert!(!parsed.runtime_variables[1].required);

        let single = "  - name: PRICING_API_KEY\n    kind: secret\n    purpose: Authenticates market data lookups.\n";
        for invalid in [
            format!("{single}{single}"),
            single.replace("PRICING_API_KEY", "HOST"),
            single.replace("PRICING_API_KEY", "PORT"),
            single.replace("PRICING_API_KEY", "DATA_DIR"),
            single.replace("PRICING_API_KEY", "POCKETS_HTTPS_GATEWAY"),
            single.replace("PRICING_API_KEY", "BAD-NAME"),
            single.replace("kind: secret", "kind: token"),
            format!("{single}    value: leaked\n"),
            single.replace("Authenticates market data lookups.", "\"Line 1\\nLine 2\""),
        ] {
            let doc = format!("{BUNDLED_MANIFEST}\nruntimeVariables:\n{invalid}");
            assert!(
                parse_manifest(&doc).is_err(),
                "expected invalid runtimeVariables to be rejected:\n{invalid}"
            );
        }
    }
}
