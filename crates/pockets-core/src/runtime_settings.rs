//! Host-only runtime settings, keyed by native Pocket ID and environment.
//!
//! Each environment is saved atomically in one Keychain item.

use std::collections::BTreeMap;

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

#[derive(Clone, Copy, Deserialize, Serialize)]
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

#[cfg_attr(target_os = "macos", allow(dead_code))]
struct FileVault {
    root: std::path::PathBuf,
}

#[cfg_attr(target_os = "macos", allow(dead_code))]
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

enum PlatformCredentials {
    #[cfg(target_os = "macos")]
    MacKeychain(Keychain),
    #[cfg(not(target_os = "macos"))]
    Vault(FileVault),
}

impl PlatformCredentials {
    fn for_store(store: &crate::projects::Store) -> Self {
        #[cfg(target_os = "macos")]
        {
            let _ = store;
            Self::MacKeychain(Keychain)
        }
        #[cfg(not(target_os = "macos"))]
        {
            Self::Vault(FileVault::for_store(store))
        }
    }
}

impl Credentials for PlatformCredentials {
    fn get(&self, account: &str) -> Result<Option<Vec<u8>>, String> {
        match self {
            #[cfg(target_os = "macos")]
            Self::MacKeychain(k) => k.get(account),
            #[cfg(not(target_os = "macos"))]
            Self::Vault(v) => v.get(account),
        }
    }

    fn set(&self, account: &str, bytes: &[u8]) -> Result<(), String> {
        match self {
            #[cfg(target_os = "macos")]
            Self::MacKeychain(k) => k.set(account, bytes),
            #[cfg(not(target_os = "macos"))]
            Self::Vault(v) => v.set(account, bytes),
        }
    }

    fn remove(&self, account: &str) -> Result<(), String> {
        match self {
            #[cfg(target_os = "macos")]
            Self::MacKeychain(k) => k.remove(account),
            #[cfg(not(target_os = "macos"))]
            Self::Vault(v) => v.remove(account),
        }
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

async fn for_project(
    ctx: &crate::AppContext,
    id: String,
    environment: Environment,
    change: Option<Change>,
) -> Result<Vec<VariableInfo>, String> {
    let _guard = ctx.runtime_settings.0.lock().await;
    let store = ctx.store();
    tokio::task::spawn_blocking(move || {
        store.load(&id)?;
        let creds = PlatformCredentials::for_store(&store);
        let account = environment.account(&id);
        match change {
            Some(change) => apply(&creds, &account, change),
            None => load(&creds, &account).map(status),
        }
    })
    .await
    .map_err(|_| "Accessing runtime settings was interrupted.".to_owned())?
}

pub async fn project_runtime_settings(
    ctx: &crate::AppContext,
    id: String,
    environment: Environment,
) -> Result<Vec<VariableInfo>, String> {
    for_project(ctx, id, environment, None).await
}

pub async fn change_project_runtime_settings(
    ctx: &crate::AppContext,
    id: String,
    environment: Environment,
    change: Change,
) -> Result<Vec<VariableInfo>, String> {
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
}
