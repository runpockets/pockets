//! Host-only runtime settings, keyed by native Pocket ID and environment.
//!
//! Each environment is saved atomically in an owner-only (`0700` dir / `0600`
//! file) local vault inside Pockets' data directory so secrets survive unsigned
//! binary upgrades (e.g. Homebrew) and background service restarts cleanly. On
//! macOS, readable legacy Keychain items are transparently migrated into the
//! local vault on first read and cleaned up best-effort on write/delete.

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

use serde::{Deserialize, Serialize};

pub(crate) const MAX_VARIABLES: usize = 32;
const MAX_VALUE_BYTES: usize = 4096;
const MAX_RECORD_BYTES: usize = 64 * 1024;

// Keep an interactive Keychain prompt from pausing preview refresh or installed
// apps. Project deletion also holds this lock until its record is removed.
#[derive(Default)]
pub struct RuntimeSettings(pub tokio::sync::Mutex<()>);

#[derive(Clone, Copy, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum Environment {
    Preview,
    Installed,
}

impl Environment {
    pub(crate) fn name(self) -> &'static str {
        match self {
            Self::Preview => "preview",
            Self::Installed => "installed",
        }
    }

    fn account(self, id: &str) -> String {
        format!("{id}/{}", self.name())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum VariableKind {
    Plain,
    Secret,
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct Variable {
    kind: VariableKind,
    value: String,
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct Settings {
    version: u8,
    variables: BTreeMap<String, Variable>,
}

#[derive(Serialize)]
pub struct VariableInfo {
    name: String,
    kind: VariableKind,
    #[serde(skip_serializing_if = "Option::is_none")]
    value: Option<String>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RuntimeSettingsSnapshot {
    pub variables: Vec<VariableInfo>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub declared: Vec<crate::manifest::RuntimeVariable>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub detected: Vec<String>,
}

#[derive(Deserialize)]
#[serde(tag = "action", rename_all = "camelCase", deny_unknown_fields)]
pub enum Change {
    Set {
        name: String,
        kind: VariableKind,
        value: String,
        replace: bool,
    },
    Remove {
        name: String,
    },
}

trait Credentials {
    fn get(&self, account: &str) -> Result<Option<Vec<u8>>, String>;
    fn set(&self, account: &str, bytes: &[u8]) -> Result<(), String>;
    fn remove(&self, account: &str) -> Result<(), String>;
}

struct Keychain;

#[cfg(target_os = "macos")]
mod macos {
    use security_framework::base::Error;
    use security_framework::passwords::{
        delete_generic_password_options, generic_password, set_generic_password_options,
        PasswordOptions,
    };
    use security_framework_sys::base::errSecItemNotFound;

    use super::{Credentials, Keychain};

    const SERVICE: &str = "dev.pockets.desktop.runtime";

    fn options(account: &str) -> PasswordOptions {
        let mut options = PasswordOptions::new_generic_password(SERVICE, account);
        options.set_access_synchronized(Some(false));
        options
    }

    fn keychain_error(error: Error) -> String {
        format!(
            "Pockets could not access runtime settings in Keychain ({}). Unlock Keychain and allow Pockets access, then retry.",
            error.code()
        )
    }

    impl Credentials for Keychain {
        fn get(&self, account: &str) -> Result<Option<Vec<u8>>, String> {
            match generic_password(options(account)) {
                Ok(bytes) => Ok(Some(bytes)),
                Err(error) if error.code() == errSecItemNotFound => Ok(None),
                Err(error) => Err(keychain_error(error)),
            }
        }

        fn set(&self, account: &str, bytes: &[u8]) -> Result<(), String> {
            set_generic_password_options(bytes, options(account)).map_err(keychain_error)
        }

        fn remove(&self, account: &str) -> Result<(), String> {
            match delete_generic_password_options(options(account)) {
                Ok(()) => Ok(()),
                Err(error) if error.code() == errSecItemNotFound => Ok(()),
                Err(error) => Err(keychain_error(error)),
            }
        }
    }
}

struct FileVault {
    root: std::path::PathBuf,
}

impl FileVault {
    fn for_store(store: &crate::projects::Store) -> Self {
        let base = store.root().parent().unwrap_or_else(|| store.root());
        Self {
            root: base.join("runtime-secrets"),
        }
    }

    fn entry_path(&self, account: &str) -> Result<std::path::PathBuf, String> {
        let safe = account.replace('/', "__");
        if safe.is_empty()
            || safe.contains("..")
            || safe.contains('/')
            || safe.contains('\\')
            || safe.contains('\0')
        {
            return Err("Invalid runtime settings account key.".into());
        }
        Ok(self.root.join(format!("{safe}.json")))
    }
}

impl Credentials for FileVault {
    fn get(&self, account: &str) -> Result<Option<Vec<u8>>, String> {
        let path = self.entry_path(account)?;
        match std::fs::read(&path) {
            Ok(bytes) => Ok(Some(bytes)),
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(err) => Err(format!("Pockets could not read runtime settings: {err}")),
        }
    }

    fn set(&self, account: &str, bytes: &[u8]) -> Result<(), String> {
        std::fs::create_dir_all(&self.root)
            .map_err(|err| format!("Pockets could not create runtime secrets directory: {err}"))?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = std::fs::set_permissions(&self.root, std::fs::Permissions::from_mode(0o700));
        }
        let path = self.entry_path(account)?;
        let temp = self.root.join(format!(".tmp-{}.json", uuid::Uuid::new_v4()));
        {
            use std::io::Write;
            let mut file = {
                #[cfg(unix)]
                {
                    use std::os::unix::fs::OpenOptionsExt;
                    std::fs::OpenOptions::new()
                        .create_new(true)
                        .write(true)
                        .mode(0o600)
                        .open(&temp)
                }
                #[cfg(not(unix))]
                {
                    std::fs::OpenOptions::new()
                        .create_new(true)
                        .write(true)
                        .open(&temp)
                }
            }
            .map_err(|err| format!("Pockets could not save runtime settings: {err}"))?;
            if let Err(err) = file.write_all(bytes).and_then(|()| file.sync_all()) {
                let _ = std::fs::remove_file(&temp);
                return Err(format!("Pockets could not write runtime settings: {err}"));
            }
        }
        std::fs::rename(&temp, &path).map_err(|err| {
            let _ = std::fs::remove_file(&temp);
            format!("Pockets could not commit runtime settings: {err}")
        })
    }

    fn remove(&self, account: &str) -> Result<(), String> {
        let path = self.entry_path(account)?;
        match std::fs::remove_file(&path) {
            Ok(()) => Ok(()),
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(err) => Err(format!("Pockets could not remove runtime settings: {err}")),
        }
    }
}

struct PlatformCredentials {
    vault: FileVault,
}

impl PlatformCredentials {
    fn for_store(store: &crate::projects::Store) -> Self {
        Self {
            vault: FileVault::for_store(store),
        }
    }
}

impl Credentials for PlatformCredentials {
    fn get(&self, account: &str) -> Result<Option<Vec<u8>>, String> {
        if let Some(bytes) = self.vault.get(account)? {
            return Ok(Some(bytes));
        }
        #[cfg(target_os = "macos")]
        {
            match Keychain.get(account) {
                Ok(Some(bytes)) => {
                    // Migrate readable legacy Keychain items into the owner-only
                    // FileVault so future unsigned binary upgrades never lose access.
                    let _ = self.vault.set(account, &bytes);
                    return Ok(Some(bytes));
                }
                Ok(None) => {}
                Err(err) => {
                    // Unsigned Homebrew/CLI upgrades change the binary's ad-hoc
                    // signature, causing Keychain to return -25244 / -25308 for
                    // items created by an older version. Never brick the Pocket
                    // or block deletion when a legacy item is inaccessible.
                    eprintln!("pockets: skipping inaccessible legacy macOS Keychain entry ({account}): {err}");
                }
            }
        }
        Ok(None)
    }

    fn set(&self, account: &str, bytes: &[u8]) -> Result<(), String> {
        self.vault.set(account, bytes)?;
        #[cfg(target_os = "macos")]
        {
            let _ = Keychain.remove(account);
        }
        Ok(())
    }

    fn remove(&self, account: &str) -> Result<(), String> {
        self.vault.remove(account)?;
        #[cfg(target_os = "macos")]
        {
            let _ = Keychain.remove(account);
        }
        Ok(())
    }
}

pub(crate) fn validate_name(name: &str) -> Result<(), String> {
    let mut chars = name.bytes();
    if name.len() > 128
        || !chars
            .next()
            .is_some_and(|c| c == b'_' || c.is_ascii_alphabetic())
        || !chars.all(|c| c == b'_' || c.is_ascii_alphanumeric())
    {
        return Err("Use up to 128 ASCII letters, digits, or underscores for the name. It must begin with a letter or underscore.".into());
    }
    if matches!(name, "HOST" | "PORT" | "DATA_DIR") || name.starts_with("POCKETS_") {
        return Err("That variable name is reserved by Pockets.".into());
    }
    Ok(())
}

fn validate_value(kind: VariableKind, value: &str) -> Result<(), String> {
    if matches!(kind, VariableKind::Secret) && value.is_empty() {
        return Err("Enter a secret value.".into());
    }
    if value.len() > MAX_VALUE_BYTES || value.contains('\0') {
        return Err("Use a value of up to 4 KiB without null characters.".into());
    }
    Ok(())
}

fn load(credentials: &impl Credentials, account: &str) -> Result<Settings, String> {
    let Some(bytes) = credentials.get(account)? else {
        return Ok(Settings {
            version: 1,
            variables: BTreeMap::new(),
        });
    };
    let corrupted =
        || "Pockets could not read this environment’s saved runtime settings.".to_owned();
    if bytes.len() > MAX_RECORD_BYTES {
        return Err(corrupted());
    }
    let settings: Settings = serde_json::from_slice(&bytes).map_err(|_| corrupted())?;
    if settings.version != 1
        || settings.variables.len() > MAX_VARIABLES
        || settings.variables.iter().any(|(name, variable)| {
            validate_name(name).is_err() || validate_value(variable.kind, &variable.value).is_err()
        })
    {
        return Err(corrupted());
    }
    Ok(settings)
}

fn status(settings: Settings) -> Vec<VariableInfo> {
    settings
        .variables
        .into_iter()
        .map(|(name, variable)| VariableInfo {
            name,
            kind: variable.kind,
            value: match variable.kind {
                VariableKind::Plain => Some(variable.value),
                VariableKind::Secret => None,
            },
        })
        .collect()
}

fn apply(
    credentials: &impl Credentials,
    account: &str,
    change: Change,
) -> Result<Vec<VariableInfo>, String> {
    let mut settings = load(credentials, account)?;
    match change {
        Change::Set {
            name,
            kind,
            value,
            replace,
        } => {
            validate_name(&name)?;
            validate_value(kind, &value)?;
            match (replace, settings.variables.contains_key(&name)) {
                (false, true) => {
                    return Err("That variable already exists. Select Replace to change it.".into())
                }
                (true, false) => {
                    return Err(
                        "That variable was removed. Add it again to save a new value.".into(),
                    )
                }
                _ => (),
            }
            if !replace && settings.variables.len() == MAX_VARIABLES {
                return Err(format!(
                    "This environment can hold up to {MAX_VARIABLES} variables."
                ));
            }
            settings.variables.insert(name, Variable { kind, value });
        }
        Change::Remove { name } => {
            validate_name(&name)?;
            if settings.variables.remove(&name).is_none() {
                return Ok(status(settings));
            }
        }
    }
    if settings.variables.is_empty() {
        credentials.remove(account)?;
    } else {
        let bytes =
            serde_json::to_vec(&settings).map_err(|_| "Could not encode runtime settings.")?;
        if bytes.len() > MAX_RECORD_BYTES {
            return Err("This environment’s runtime settings exceed 64 KiB.".into());
        }
        credentials.set(account, &bytes)?;
    }
    Ok(status(settings))
}

fn forget(credentials: &impl Credentials, id: &str) -> Result<(), String> {
    crate::projects::validate_project_id(id)?;
    for environment in [Environment::Preview, Environment::Installed] {
        credentials.remove(&environment.account(id))?;
    }
    Ok(())
}

pub(crate) async fn forget_project(
    store: &crate::projects::Store,
    id: String,
) -> Result<(), String> {
    let creds = PlatformCredentials::for_store(store);
    tokio::task::spawn_blocking(move || forget(&creds, &id))
        .await
        .map_err(|_| "Removing runtime settings was interrupted.".to_owned())?
}

pub(crate) async fn launch_values(
    store: &crate::projects::Store,
    id: &str,
    environment: Environment,
) -> Result<crate::runtime_env::Values, String> {
    let store = store.clone();
    let id = id.to_owned();
    tokio::task::spawn_blocking(move || {
        store.load(&id)?;
        let creds = PlatformCredentials::for_store(&store);
        let settings = load(&creds, &environment.account(&id))?;
        Ok(settings
            .variables
            .into_iter()
            .map(|(name, variable)| (name, variable.value))
            .collect())
    })
    .await
    .map_err(|_| "Loading runtime configuration was interrupted.".to_owned())?
}

const MAX_MANIFEST_BYTES: u64 = 128 * 1024;
const MAX_SCAN_DEPTH: usize = 8;
const MAX_SCAN_FILES: usize = 200;
const MAX_SCAN_FILE_BYTES: u64 = 512 * 1024;
const MAX_SCAN_TOTAL_BYTES: u64 = 2 * 1024 * 1024;

const EXCLUDED_SCAN_DIRS: &[&str] = &[
    "node_modules",
    "data",
    "dist",
    "target",
    "build",
    "coverage",
    "test",
    "tests",
    "__tests__",
];

const IGNORED_DETECTED_NAMES: &[&str] = &[
    "CI",
    "DEBUG",
    "FORCE_COLOR",
    "HOME",
    "HTTP_PROXY",
    "HTTPS_PROXY",
    "LANG",
    "LC_ALL",
    "NO_COLOR",
    "NO_PROXY",
    "NODE_ENV",
    "NODE_EXTRA_CA_CERTS",
    "NODE_OPTIONS",
    "PATH",
    "PWD",
    "TERM",
    "TZ",
    "USER",
    "http_proxy",
    "https_proxy",
    "no_proxy",
    "constructor",
    "hasOwnProperty",
    "isPrototypeOf",
    "propertyIsEnumerable",
    "toLocaleString",
    "toString",
    "valueOf",
    "__proto__",
];

/// Reads `runtimeVariables` from a linked folder's live `pocket.yaml` when it
/// is a safe, bounded regular file belonging to this project, falling back to
/// `None` if unreadable or mid-edit.
fn read_linked_manifest_variables(
    folder: &Path,
    expected_id: &str,
) -> Option<Vec<crate::manifest::RuntimeVariable>> {
    use std::io::Read;

    let path = folder.join("pocket.yaml");
    let meta = std::fs::symlink_metadata(&path).ok()?;
    if !meta.file_type().is_file() || meta.len() > MAX_MANIFEST_BYTES {
        return None;
    }
    let file = std::fs::File::open(&path).ok()?;
    let mut bytes = Vec::new();
    file.take(MAX_MANIFEST_BYTES + 1)
        .read_to_end(&mut bytes)
        .ok()?;
    if bytes.len() as u64 > MAX_MANIFEST_BYTES {
        return None;
    }
    let yaml = String::from_utf8(bytes).ok()?;
    let manifest = crate::manifest::parse_manifest(&yaml).ok()?;
    if manifest.metadata.id != expected_id {
        return None;
    }
    Some(manifest.runtime_variables)
}

fn declared_variables_for(
    project: &crate::projects::Project,
    linked_folder: Option<&Path>,
    environment: Environment,
) -> Vec<crate::manifest::RuntimeVariable> {
    let working = linked_folder
        .and_then(|folder| read_linked_manifest_variables(folder, &project.manifest.metadata.id))
        .unwrap_or_else(|| project.manifest.runtime_variables.clone());
    match environment {
        Environment::Preview => working,
        Environment::Installed => {
            let Some(installed) = project.installed.as_ref() else {
                return working;
            };
            let mut merged = installed.manifest.runtime_variables.clone();
            let mut seen: BTreeSet<String> = merged.iter().map(|v| v.name.clone()).collect();
            for var in working {
                if merged.len() >= MAX_VARIABLES {
                    break;
                }
                if seen.insert(var.name.clone()) {
                    merged.push(var);
                }
            }
            merged
        }
    }
}

fn is_detectable_env_name(name: &str) -> bool {
    validate_name(name).is_ok() && !IGNORED_DETECTED_NAMES.contains(&name)
}

#[derive(Debug, PartialEq, Eq)]
enum JsToken {
    Ident(String),
    Str(String),
    Dot,
    OptDot,
    LBracket,
    RBracket,
    LBrace,
    RBrace,
    LParen,
    RParen,
    Eq,
    Colon,
    Comma,
    Other,
}

fn is_js_ident_start(b: u8) -> bool {
    b == b'_' || b == b'$' || b.is_ascii_alphabetic()
}

fn is_js_ident_continue(b: u8) -> bool {
    is_js_ident_start(b) || b.is_ascii_digit()
}

/// Lightweight JS/TS tokenizer that skips comments and non-interpolated string
/// bodies while preserving `${...}` template expressions and string literals
/// needed to detect `process.env` reads.
fn tokenize_js(source: &str) -> Vec<JsToken> {
    let bytes = source.as_bytes();
    let len = bytes.len();
    let mut i = 0;
    let mut tokens = Vec::new();
    // Tracks brace nesting inside active template literal `${ ... }` blocks.
    let mut template_braces: Vec<usize> = Vec::new();

    fn consume_template_chunk(bytes: &[u8], mut i: usize) -> (usize, bool) {
        let len = bytes.len();
        while i < len {
            match bytes[i] {
                b'\\' => i = (i + 2).min(len),
                b'`' => return (i + 1, false),
                b'$' if i + 1 < len && bytes[i + 1] == b'{' => return (i + 2, true),
                _ => i += 1,
            }
        }
        (len, false)
    }

    while i < len {
        let b = bytes[i];
        if b.is_ascii_whitespace() {
            i += 1;
            continue;
        }
        // Line comment
        if b == b'/' && i + 1 < len && bytes[i + 1] == b'/' {
            i += 2;
            while i < len && bytes[i] != b'\n' {
                i += 1;
            }
            continue;
        }
        // Block comment
        if b == b'/' && i + 1 < len && bytes[i + 1] == b'*' {
            i += 2;
            while i + 1 < len && !(bytes[i] == b'*' && bytes[i + 1] == b'/') {
                i += 1;
            }
            i = (i + 2).min(len);
            continue;
        }
        // Single- or double-quoted string literal
        if b == b'\'' || b == b'"' {
            let quote = b;
            i += 1;
            let start = i;
            let mut has_escape = false;
            while i < len && bytes[i] != quote {
                if bytes[i] == b'\\' {
                    has_escape = true;
                    i = (i + 2).min(len);
                } else if bytes[i] == b'\n' || bytes[i] == b'\r' {
                    break;
                } else {
                    i += 1;
                }
            }
            let end = i;
            if i < len && bytes[i] == quote {
                i += 1;
            }
            if !has_escape && end > start {
                if let Ok(s) = std::str::from_utf8(&bytes[start..end]) {
                    tokens.push(JsToken::Str(s.to_owned()));
                    continue;
                }
            }
            tokens.push(JsToken::Other);
            continue;
        }
        // Template literal
        if b == b'`' {
            let (next, entered_expr) = consume_template_chunk(bytes, i + 1);
            i = next;
            if entered_expr {
                template_braces.push(0);
                tokens.push(JsToken::Other);
            } else {
                tokens.push(JsToken::Other);
            }
            continue;
        }
        // Identifier
        if is_js_ident_start(b) {
            let start = i;
            i += 1;
            while i < len && is_js_ident_continue(bytes[i]) {
                i += 1;
            }
            if let Ok(ident) = std::str::from_utf8(&bytes[start..i]) {
                tokens.push(JsToken::Ident(ident.to_owned()));
            } else {
                tokens.push(JsToken::Other);
            }
            continue;
        }
        // Punctuation
        match b {
            b'?' if i + 1 < len && bytes[i + 1] == b'.' => {
                // `?.` followed by a digit is ternary `? .5`, not optional chaining.
                if i + 2 < len && bytes[i + 2].is_ascii_digit() {
                    tokens.push(JsToken::Other);
                    i += 1;
                } else {
                    tokens.push(JsToken::OptDot);
                    i += 2;
                }
            }
            b'.' => {
                tokens.push(JsToken::Dot);
                i += 1;
            }
            b'[' => {
                tokens.push(JsToken::LBracket);
                i += 1;
            }
            b']' => {
                tokens.push(JsToken::RBracket);
                i += 1;
            }
            b'{' => {
                if let Some(depth) = template_braces.last_mut() {
                    *depth += 1;
                }
                tokens.push(JsToken::LBrace);
                i += 1;
            }
            b'}' => {
                if let Some(depth) = template_braces.last_mut() {
                    if *depth == 0 {
                        template_braces.pop();
                        tokens.push(JsToken::Other);
                        let (next, entered_expr) = consume_template_chunk(bytes, i + 1);
                        i = next;
                        if entered_expr {
                            template_braces.push(0);
                        }
                        continue;
                    }
                    *depth -= 1;
                }
                tokens.push(JsToken::RBrace);
                i += 1;
            }
            b'(' => {
                tokens.push(JsToken::LParen);
                i += 1;
            }
            b')' => {
                tokens.push(JsToken::RParen);
                i += 1;
            }
            b'=' => {
                if i + 1 < len && matches!(bytes[i + 1], b'=' | b'>') {
                    tokens.push(JsToken::Other);
                    i += 2;
                } else {
                    tokens.push(JsToken::Eq);
                    i += 1;
                }
            }
            b':' => {
                tokens.push(JsToken::Colon);
                i += 1;
            }
            b',' => {
                tokens.push(JsToken::Comma);
                i += 1;
            }
            _ => {
                tokens.push(JsToken::Other);
                i += 1;
            }
        }
    }

    tokens
}

fn is_dot(token: &JsToken) -> bool {
    matches!(token, JsToken::Dot | JsToken::OptDot)
}

fn extract_process_env_vars(source: &str, out: &mut BTreeSet<String>) {
    if !source.contains("process") || !source.contains("env") {
        return;
    }
    let tokens = tokenize_js(source);
    let len = tokens.len();

    for idx in 0..len {
        let JsToken::Ident(ref ident) = tokens[idx] else {
            continue;
        };
        if ident != "process" {
            continue;
        }
        // Ignore member accesses like `other.process.env.FOO`.
        if idx > 0 && is_dot(&tokens[idx - 1]) {
            continue;
        }
        if idx + 2 >= len
            || !is_dot(&tokens[idx + 1])
            || !matches!(&tokens[idx + 2], JsToken::Ident(env) if env == "env")
        {
            continue;
        }

        // Case 1: `process.env.NAME` / `process.env?.NAME` / `process.env["NAME"]` / `process.env?.['NAME']`
        let after = idx + 3;
        if after + 1 < len && is_dot(&tokens[after]) {
            if let JsToken::Ident(ref name) = tokens[after + 1] {
                if is_detectable_env_name(name) {
                    out.insert(name.clone());
                }
            } else if after + 3 < len
                && tokens[after + 1] == JsToken::LBracket
                && tokens[after + 3] == JsToken::RBracket
            {
                if let JsToken::Str(ref name) = tokens[after + 2] {
                    if is_detectable_env_name(name) {
                        out.insert(name.clone());
                    }
                }
            }
        } else if after + 2 < len
            && tokens[after] == JsToken::LBracket
            && tokens[after + 2] == JsToken::RBracket
        {
            if let JsToken::Str(ref name) = tokens[after + 1] {
                if is_detectable_env_name(name) {
                    out.insert(name.clone());
                }
            }
        }

        // Case 2: `{ A, B = "x", C: localC } = process.env`
        if idx >= 2 && tokens[idx - 1] == JsToken::Eq && tokens[idx - 2] == JsToken::RBrace {
            let rbrace = idx - 2;
            let mut depth = 0usize;
            let mut lbrace = None;
            for back in (0..=rbrace).rev() {
                match tokens[back] {
                    JsToken::RBrace => depth += 1,
                    JsToken::LBrace => {
                        depth -= 1;
                        if depth == 0 {
                            lbrace = Some(back);
                            break;
                        }
                    }
                    _ => {}
                }
            }
            if let Some(start) = lbrace {
                let mut cur = start + 1;
                let mut nest = 0usize;
                let mut at_entry_start = true;
                while cur < rbrace {
                    if at_entry_start && nest == 0 {
                        match &tokens[cur] {
                            JsToken::Ident(name) => {
                                if is_detectable_env_name(name) {
                                    out.insert(name.clone());
                                }
                                at_entry_start = false;
                            }
                            JsToken::Str(name)
                                if cur + 1 < rbrace && tokens[cur + 1] == JsToken::Colon =>
                            {
                                if is_detectable_env_name(name) {
                                    out.insert(name.clone());
                                }
                                at_entry_start = false;
                            }
                            _ => {
                                at_entry_start = false;
                            }
                        }
                    }
                    match tokens[cur] {
                        JsToken::LBrace | JsToken::LBracket | JsToken::LParen => nest += 1,
                        JsToken::RBrace | JsToken::RBracket | JsToken::RParen => {
                            nest = nest.saturating_sub(1);
                        }
                        JsToken::Comma if nest == 0 => {
                            at_entry_start = true;
                        }
                        _ => {}
                    }
                    cur += 1;
                }
            }
        }
    }
}

fn is_scannable_js_file(name: &str) -> bool {
    let lower = name.to_ascii_lowercase();
    if lower.contains(".test.") || lower.contains(".spec.") {
        return false;
    }
    matches!(
        Path::new(&lower).extension().and_then(|ext| ext.to_str()),
        Some("js" | "mjs" | "cjs" | "ts" | "mts" | "cts" | "jsx" | "tsx")
    )
}

fn scan_source_dir(
    dir: &Path,
    depth: usize,
    files_scanned: &mut usize,
    bytes_scanned: &mut u64,
    found: &mut BTreeSet<String>,
) {
    use std::io::Read;

    if depth > MAX_SCAN_DEPTH
        || *files_scanned >= MAX_SCAN_FILES
        || *bytes_scanned >= MAX_SCAN_TOTAL_BYTES
    {
        return;
    }
    let Ok(read_dir) = std::fs::read_dir(dir) else {
        return;
    };
    let mut entries: Vec<_> = read_dir.flatten().collect();
    entries.sort_by_key(|entry| entry.file_name());

    for entry in entries {
        if *files_scanned >= MAX_SCAN_FILES || *bytes_scanned >= MAX_SCAN_TOTAL_BYTES {
            break;
        }
        let file_name = entry.file_name();
        let Some(name) = file_name.to_str() else {
            continue;
        };
        if name.starts_with('.') {
            continue;
        }
        let path = entry.path();
        let Ok(meta) = std::fs::symlink_metadata(&path) else {
            continue;
        };
        if meta.file_type().is_symlink() {
            continue;
        }
        if meta.file_type().is_dir() {
            if EXCLUDED_SCAN_DIRS.contains(&name) {
                continue;
            }
            scan_source_dir(&path, depth + 1, files_scanned, bytes_scanned, found);
        } else if meta.file_type().is_file() && is_scannable_js_file(name) {
            if meta.len() > MAX_SCAN_FILE_BYTES {
                continue;
            }
            let remaining = MAX_SCAN_TOTAL_BYTES.saturating_sub(*bytes_scanned);
            let limit = remaining.min(MAX_SCAN_FILE_BYTES);
            let Ok(file) = std::fs::File::open(&path) else {
                continue;
            };
            let mut bytes = Vec::new();
            if file.take(limit + 1).read_to_end(&mut bytes).is_err() {
                continue;
            }
            if bytes.len() as u64 > limit {
                continue;
            }
            *files_scanned += 1;
            *bytes_scanned += bytes.len() as u64;
            if let Ok(text) = String::from_utf8(bytes) {
                extract_process_env_vars(&text, found);
            }
        }
    }
}

fn detect_source_variables(
    root: &Path,
    declared: &[crate::manifest::RuntimeVariable],
) -> Vec<String> {
    let mut found = BTreeSet::new();
    let mut files_scanned = 0usize;
    let mut bytes_scanned = 0u64;
    scan_source_dir(root, 0, &mut files_scanned, &mut bytes_scanned, &mut found);
    for decl in declared {
        found.remove(&decl.name);
    }
    found.into_iter().take(MAX_VARIABLES).collect()
}

fn snapshot_for(
    store: &crate::projects::Store,
    project: &crate::projects::Project,
    environment: Environment,
    variables: Vec<VariableInfo>,
) -> RuntimeSettingsSnapshot {
    let linked_folder = crate::development::folder(store, &project.id).ok();
    let declared = declared_variables_for(project, linked_folder.as_deref(), environment);
    let scan_root = linked_folder.or_else(|| store.source_path(&project.id).ok());
    let detected = scan_root
        .as_deref()
        .map(|root| detect_source_variables(root, &declared))
        .unwrap_or_default();
    RuntimeSettingsSnapshot {
        variables,
        declared,
        detected,
    }
}

async fn for_project(
    ctx: &crate::AppContext,
    id: String,
    environment: Environment,
    change: Option<Change>,
) -> Result<RuntimeSettingsSnapshot, String> {
    let _guard = ctx.runtime_settings.0.lock().await;
    let store = ctx.store();
    tokio::task::spawn_blocking(move || {
        let project = store.load(&id)?;
        let creds = PlatformCredentials::for_store(&store);
        let account = environment.account(&id);
        let variables = match change {
            Some(change) => apply(&creds, &account, change)?,
            None => load(&creds, &account).map(status)?,
        };
        Ok(snapshot_for(&store, &project, environment, variables))
    })
    .await
    .map_err(|_| "Accessing runtime settings was interrupted.".to_owned())?
}

pub async fn project_runtime_settings(
    ctx: &crate::AppContext,
    id: String,
    environment: Environment,
) -> Result<RuntimeSettingsSnapshot, String> {
    for_project(ctx, id, environment, None).await
}

pub async fn change_project_runtime_settings(
    ctx: &crate::AppContext,
    id: String,
    environment: Environment,
    change: Change,
) -> Result<RuntimeSettingsSnapshot, String> {
    for_project(ctx, id, environment, Some(change)).await
}

#[cfg(test)]
mod tests {
    use std::cell::{Cell, RefCell};

    use serde_json::json;

    use super::*;

    #[derive(Default)]
    struct MemoryCredentials {
        records: RefCell<BTreeMap<String, Vec<u8>>>,
        fail_save: Cell<bool>,
    }

    impl Credentials for MemoryCredentials {
        fn get(&self, account: &str) -> Result<Option<Vec<u8>>, String> {
            Ok(self.records.borrow().get(account).cloned())
        }

        fn set(&self, account: &str, bytes: &[u8]) -> Result<(), String> {
            if self.fail_save.get() {
                return Err("Keychain refused the write.".into());
            }
            self.records
                .borrow_mut()
                .insert(account.into(), bytes.into());
            Ok(())
        }

        fn remove(&self, account: &str) -> Result<(), String> {
            self.records.borrow_mut().remove(account);
            Ok(())
        }
    }

    fn set(name: &str, kind: VariableKind, value: &str, replace: bool) -> Change {
        Change::Set {
            name: name.into(),
            kind,
            value: value.into(),
            replace,
        }
    }

    #[test]
    fn runtime_variables_are_isolated_and_only_plain_values_are_returned() {
        let keychain = MemoryCredentials::default();
        let id = "1".repeat(32);
        let another_id = "2".repeat(32);
        let preview = Environment::Preview.account(&id);
        let installed = Environment::Installed.account(&id);
        let another = Environment::Preview.account(&another_id);

        let secret = set(
            "PRICING_API_KEY",
            VariableKind::Secret,
            "fixture-secret",
            false,
        );
        apply(&keychain, &preview, secret).unwrap();
        let variables = apply(
            &keychain,
            &preview,
            set("CURRENCY", VariableKind::Plain, "NZD", false),
        )
        .unwrap();
        assert_eq!(
            serde_json::to_value(variables).unwrap(),
            json!([
                { "name": "CURRENCY", "kind": "plain", "value": "NZD" },
                { "name": "PRICING_API_KEY", "kind": "secret" }
            ])
        );
        assert!(load(&keychain, &installed).unwrap().variables.is_empty());
        let unrelated_key = || {
            set(
                "PRICING_API_KEY",
                VariableKind::Secret,
                "other-fixture",
                false,
            )
        };
        apply(&keychain, &installed, unrelated_key()).unwrap();
        apply(&keychain, &another, unrelated_key()).unwrap();

        forget(&keychain, &id).unwrap();
        assert!(keychain.get(&preview).unwrap().is_none());
        assert!(keychain.get(&installed).unwrap().is_none());
        assert!(keychain.get(&another).unwrap().is_some());
    }

    #[test]
    fn updates_and_removals_are_retryable_without_partial_writes() {
        let keychain = MemoryCredentials::default();
        let account = Environment::Preview.account(&"3".repeat(32));
        let secret = |value, replace| set("TOKEN", VariableKind::Secret, value, replace);
        assert!(apply(&keychain, &account, secret("first", true)).is_err());
        apply(&keychain, &account, secret("first", false)).unwrap();
        assert!(apply(&keychain, &account, secret("second", false)).is_err());

        keychain.fail_save.set(true);
        assert!(apply(&keychain, &account, secret("second", true)).is_err());
        assert_eq!(
            load(&keychain, &account).unwrap().variables["TOKEN"].value,
            "first"
        );
        keychain.fail_save.set(false);
        apply(&keychain, &account, secret("second", true)).unwrap();
        assert_eq!(
            load(&keychain, &account).unwrap().variables["TOKEN"].value,
            "second"
        );

        for _ in 0..2 {
            let remaining = apply(
                &keychain,
                &account,
                Change::Remove {
                    name: "TOKEN".into(),
                },
            )
            .unwrap();
            assert!(remaining.is_empty());
            assert!(keychain.get(&account).unwrap().is_none());
        }
    }

    #[test]
    fn bad_input_and_unreadable_records_do_not_disclose_or_overwrite_values() {
        let keychain = MemoryCredentials::default();
        let account = Environment::Installed.account(&"4".repeat(32));
        for (name, value) in [
            ("HOST", "protected"),
            ("POCKETS_INTERNAL", "reserved"),
            ("A-B", "bad name"),
            ("TOKEN", ""),
            ("TOKEN", "bad\0value"),
        ] {
            assert!(apply(
                &keychain,
                &account,
                set(name, VariableKind::Secret, value, false)
            )
            .is_err());
        }
        assert!(keychain.get(&account).unwrap().is_none());
        let invalid = b"corrupted-fixture-credential";
        keychain.set(&account, invalid).unwrap();
        let error = apply(
            &keychain,
            &account,
            set(
                "TOKEN",
                VariableKind::Secret,
                "updated-fixture-credential",
                true,
            ),
        )
        .err()
        .unwrap();
        assert!(!error.contains("fixture-credential"));
        assert_eq!(keychain.get(&account).unwrap().unwrap(), invalid);
    }

    #[test]
    fn file_vault_persists_atomically_with_owner_only_permissions_and_clears_cleanly() {
        let dir = tempfile::tempdir().unwrap();
        let store = crate::projects::Store::new(dir.path().join("projects"));
        let vault = FileVault::for_store(&store);
        let account = Environment::Installed.account(&"5".repeat(32));

        assert!(vault.get(&account).unwrap().is_none());
        vault.set(&account, b"{\"version\":1,\"variables\":{}}").unwrap();
        assert_eq!(
            vault.get(&account).unwrap().as_deref(),
            Some(&b"{\"version\":1,\"variables\":{}}"[..])
        );

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let path = vault.entry_path(&account).unwrap();
            let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode, 0o600);
            let dir_mode = std::fs::metadata(&vault.root).unwrap().permissions().mode() & 0o777;
            assert_eq!(dir_mode, 0o700);
        }

        vault.remove(&account).unwrap();
        assert!(vault.get(&account).unwrap().is_none());
        // Removing an already-absent item is idempotent.
        vault.remove(&account).unwrap();
    }

    #[test]
    fn extracts_process_env_references_and_filters_reserved_and_comments() {
        let source = r#"
            // process.env.COMMENTED_OUT_KEY
            /* process.env.BLOCK_COMMENT_KEY */
            const doc = "process.env.STRING_LITERAL_KEY";
            const url = `https://example.com/?key=${process.env.TEMPLATE_KEY}&mode=${process.env?.OPTIONAL_KEY}`;
            const bracket1 = process.env["BRACKET_DOUBLE"];
            const bracket2 = process.env['BRACKET_SINGLE'];
            const bracket3 = process.env?.["BRACKET_OPT"];
            const {
              DESTRUCTURED_A,
              DESTRUCTURED_B = "USD",
              DESTRUCTURED_C: localC,
              "DESTRUCTURED_STR": localStr,
              HOST = "0.0.0.0",
              PORT = "8080",
              DATA_DIR = "/data",
              NODE_ENV = "production",
              POCKETS_HTTPS_GATEWAY,
              ...rest
            } = process.env;
            const nested = other.process.env.NOT_GLOBAL;
            const dyn = process.env[dynamicKey];
            const proto = process.env.hasOwnProperty("X");
        "#;
        let mut found = BTreeSet::new();
        extract_process_env_vars(source, &mut found);
        let names: Vec<&str> = found.iter().map(String::as_str).collect();
        assert_eq!(
            names,
            vec![
                "BRACKET_DOUBLE",
                "BRACKET_OPT",
                "BRACKET_SINGLE",
                "DESTRUCTURED_A",
                "DESTRUCTURED_B",
                "DESTRUCTURED_C",
                "DESTRUCTURED_STR",
                "OPTIONAL_KEY",
                "TEMPLATE_KEY",
            ]
        );
    }

    #[test]
    fn detects_source_variables_safely_and_excludes_declared_and_tests() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("server.js"),
            "const a = process.env.DECLARED_KEY; const b = process.env.UNDOCUMENTED_KEY;\n",
        )
        .unwrap();
        std::fs::create_dir_all(dir.path().join("lib")).unwrap();
        std::fs::write(
            dir.path().join("lib/client.mjs"),
            "export const base = process.env.API_BASE_URL;\n",
        )
        .unwrap();
        std::fs::create_dir_all(dir.path().join("test")).unwrap();
        std::fs::write(
            dir.path().join("test/server.test.js"),
            "const t = process.env.TEST_ONLY_VAR;\n",
        )
        .unwrap();
        std::fs::write(
            dir.path().join("helper.test.js"),
            "const t2 = process.env.SPEC_ONLY_VAR;\n",
        )
        .unwrap();
        std::fs::create_dir_all(dir.path().join("node_modules/pkg")).unwrap();
        std::fs::write(
            dir.path().join("node_modules/pkg/index.js"),
            "const dep = process.env.VENDORED_VAR;\n",
        )
        .unwrap();
        std::fs::write(
            dir.path().join(".hidden.js"),
            "const h = process.env.HIDDEN_VAR;\n",
        )
        .unwrap();
        #[cfg(unix)]
        {
            let outside = tempfile::tempdir().unwrap();
            let secret_js = outside.path().join("outside.js");
            std::fs::write(&secret_js, "const s = process.env.SYMLINK_VAR;\n").unwrap();
            std::os::unix::fs::symlink(&secret_js, dir.path().join("linked.js")).unwrap();
        }

        let declared = vec![crate::manifest::RuntimeVariable {
            name: "DECLARED_KEY".into(),
            kind: VariableKind::Secret,
            purpose: "Declared key.".into(),
            required: true,
        }];
        let detected = detect_source_variables(dir.path(), &declared);
        assert_eq!(detected, vec!["API_BASE_URL", "UNDOCUMENTED_KEY"]);
    }

    #[test]
    fn snapshot_reads_live_linked_manifest_and_falls_back_when_invalid() {
        let temp = tempfile::tempdir().unwrap();
        let store = crate::projects::Store::new(temp.path().join("projects"));
        let parent = temp.path().join("work");
        std::fs::create_dir(&parent).unwrap();
        let project = crate::scaffold::create(&store, &parent, "Demo", "demo-vars", "").unwrap();
        let folder = crate::development::folder(&store, &project.id).unwrap();

        let base_manifest = std::fs::read_to_string(folder.join("pocket.yaml")).unwrap();
        std::fs::write(
            folder.join("pocket.yaml"),
            format!(
                "{base_manifest}\nruntimeVariables:\n  - name: PRICING_API_KEY\n    kind: secret\n    required: true\n    purpose: Fetches live market quotes.\n"
            ),
        )
        .unwrap();
        std::fs::write(
            folder.join("server.js"),
            "const k = process.env.PRICING_API_KEY; const c = process.env.CURRENCY;\n",
        )
        .unwrap();

        let snap = snapshot_for(&store, &project, Environment::Preview, Vec::new());
        assert_eq!(snap.declared.len(), 1);
        assert_eq!(snap.declared[0].name, "PRICING_API_KEY");
        assert!(snap.declared[0].required);
        assert_eq!(snap.detected, vec!["CURRENCY"]);

        // Broken mid-edit pocket.yaml falls back to stored snapshot manifest cleanly.
        std::fs::write(folder.join("pocket.yaml"), "runtimeVariables: [broken").unwrap();
        let fallback = snapshot_for(&store, &project, Environment::Preview, Vec::new());
        assert!(fallback.declared.is_empty());
        assert_eq!(fallback.detected, vec!["CURRENCY", "PRICING_API_KEY"]);
    }
}
