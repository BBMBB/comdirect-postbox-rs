use std::{
    fs::{self, File},
    io::{Read, Write},
    path::{Path, PathBuf},
};

use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};
use tempfile::NamedTempFile;
use zeroize::{Zeroize, ZeroizeOnDrop, Zeroizing};

use crate::{
    api::Credentials,
    archive::{INDEX_FILE_NAME, private_dir},
};

const MAX_CONFIG_BYTES: u64 = 64 * 1024;

pub fn default_path(output: &Path) -> PathBuf {
    output.join("config.json")
}

pub fn legacy_path() -> Result<PathBuf> {
    let executable =
        std::env::current_exe().context("Programmverzeichnis konnte nicht ermittelt werden")?;
    Ok(executable
        .parent()
        .context("Programmverzeichnis fehlt")?
        .join("config.json"))
}

#[derive(Default, Deserialize, Serialize, Zeroize, ZeroizeOnDrop)]
#[serde(deny_unknown_fields)]
pub struct SavedCredentials {
    #[serde(default)]
    pub client_id: String,
    #[serde(default)]
    pub client_secret: String,
    #[serde(default)]
    pub username: String,
}

pub struct ConfigStore {
    path: PathBuf,
}

impl ConfigStore {
    pub fn new(path: PathBuf) -> Self {
        Self { path }
    }

    pub fn validate_path(&self) -> Result<()> {
        ensure!(
            self.path
                .file_name()
                .is_some_and(|name| !name.to_string_lossy().eq_ignore_ascii_case(INDEX_FILE_NAME)),
            "Konfiguration und Archivindex müssen verschiedene Dateien sein"
        );
        Ok(())
    }

    pub fn migrate_from(&self, source: &Path) -> Result<()> {
        self.validate_path()?;
        // An existing destination always takes precedence; never replace it on migration.
        match fs::symlink_metadata(&self.path) {
            Ok(metadata) => {
                ensure!(
                    metadata.is_file(),
                    "Konfiguration muss eine reguläre Datei sein"
                );
                return Ok(());
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => (),
            Err(error) => {
                return Err(error).context("Konfigurationspfad konnte nicht geprüft werden");
            }
        }
        match fs::symlink_metadata(source) {
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
            Err(error) => {
                return Err(error).context("Bisherige Konfiguration konnte nicht geprüft werden");
            }
            Ok(_) => (),
        }
        let saved = Self::new(source.to_owned()).load()?;
        let parent = self
            .path
            .parent()
            .filter(|p| !p.as_os_str().is_empty())
            .unwrap_or(Path::new("."));
        private_dir(parent)?;
        let mut temp = NamedTempFile::new_in(parent)?;
        serde_json::to_writer_pretty(&mut temp, &saved)?;
        temp.write_all(b"\n")?;
        temp.as_file().sync_all()?;
        temp.persist_noclobber(&self.path)
            .context("Konfiguration konnte nicht ohne Überschreiben übernommen werden")?;
        #[cfg(unix)]
        File::open(parent)?.sync_all()?;
        fs::remove_file(source).context(
            "Konfiguration wurde übernommen; bisherige Datei konnte nicht entfernt werden",
        )?;
        Ok(())
    }

    pub fn load(&self) -> Result<SavedCredentials> {
        match fs::symlink_metadata(&self.path) {
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                return Ok(SavedCredentials::default());
            }
            Err(error) => return Err(error).context("Konfiguration konnte nicht geöffnet werden"),
            Ok(metadata) => ensure!(
                metadata.is_file(),
                "Konfiguration muss eine reguläre Datei sein"
            ),
        }
        let mut bytes = Zeroizing::new(Vec::new());
        File::open(&self.path)?
            .take(MAX_CONFIG_BYTES + 1)
            .read_to_end(&mut bytes)
            .context("Konfiguration konnte nicht gelesen werden")?;
        ensure!(
            bytes.len() as u64 <= MAX_CONFIG_BYTES,
            "Konfiguration überschreitet das Größenlimit von 64 KiB"
        );
        // Accept UTF-8 BOMs from Windows editors without exposing parse input in errors.
        let json = bytes.strip_prefix(b"\xef\xbb\xbf").unwrap_or(&bytes);
        serde_json::from_slice(json).map_err(|_| anyhow::anyhow!(
            "Ungültige Konfiguration. Erwartet wird UTF-8-JSON mit client_id, client_secret und username; PIN und TAN gehören nicht in diese Datei."
        ))
    }

    pub fn save(&self, credentials: &Credentials) -> Result<()> {
        credentials.validate()?;
        match fs::symlink_metadata(&self.path) {
            Ok(metadata) => ensure!(
                metadata.is_file(),
                "Konfiguration muss eine reguläre Datei sein"
            ),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => (),
            Err(error) => {
                return Err(error).context("Konfigurationspfad konnte nicht geprüft werden");
            }
        }
        let parent = self
            .path
            .parent()
            .filter(|p| !p.as_os_str().is_empty())
            .unwrap_or(Path::new("."));
        private_dir(parent)?;
        // NamedTempFile uses mode 0600 on Unix. Atomic replacement also fixes the
        // permissions of an existing config file without leaving a partial JSON file.
        let mut temp = NamedTempFile::new_in(parent)?;
        let saved = SavedCredentials {
            client_id: credentials.client_id.clone(),
            client_secret: credentials.client_secret.clone(),
            username: credentials.username.clone(),
        };
        serde_json::to_writer_pretty(&mut temp, &saved)?;
        temp.write_all(b"\n")?;
        temp.as_file().sync_all()?;
        temp.persist(&self.path).context(
            "Konfiguration konnte nicht gespeichert werden; es wurde keine Bankanfrage gesendet",
        )?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn credentials() -> Credentials {
        Credentials {
            client_id: "example-client".into(),
            client_secret: "example-secret".into(),
            username: "12345678".into(),
            pin: "DO_NOT_PERSIST_PIN".into(),
        }
    }

    #[test]
    fn config_roundtrip_and_replacement_never_store_pin() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("settings/config.json");
        let store = ConfigStore::new(path.clone());
        assert!(store.load().unwrap().username.is_empty());
        assert!(!path.exists());
        let mut input = credentials();
        store.save(&input).unwrap();
        let saved = store.load().unwrap();
        assert_eq!(saved.client_id, input.client_id);
        assert_eq!(saved.client_secret, input.client_secret);
        assert_eq!(saved.username, input.username);
        let json: serde_json::Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
        assert_eq!(json.as_object().unwrap().len(), 3);
        assert!(!fs::read_to_string(&path).unwrap().contains(&input.pin));
        input.client_secret = "updated-secret".into();
        store.save(&input).unwrap();
        assert_eq!(store.load().unwrap().client_secret, "updated-secret");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                fs::metadata(&path).unwrap().permissions().mode() & 0o777,
                0o600
            );
            assert_eq!(
                fs::metadata(path.parent().unwrap())
                    .unwrap()
                    .permissions()
                    .mode()
                    & 0o777,
                0o700
            );
        }
    }

    #[test]
    fn partial_config_and_windows_bom_are_supported() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.json");
        fs::write(&path, b"\xef\xbb\xbf{\"username\":\"12345678\"}").unwrap();
        let saved = ConfigStore::new(path).load().unwrap();
        assert_eq!(saved.username, "12345678");
        assert!(saved.client_secret.is_empty());
    }

    #[test]
    fn config_migration_preserves_credentials_and_respects_existing_destinations() {
        let dir = tempfile::tempdir().unwrap();
        let source = dir.path().join("app/config.json");
        let legacy = ConfigStore::new(source.clone());
        legacy.save(&credentials()).unwrap();
        let path = default_path(&dir.path().join("postbox"));
        let store = ConfigStore::new(path.clone());
        store.migrate_from(&source).unwrap();
        assert!(!source.exists());
        assert_eq!(
            store.load().unwrap().client_secret,
            credentials().client_secret
        );
        let contents = fs::read_to_string(&path).unwrap();
        assert!(!contents.contains(&credentials().pin));
        // Re-running startup neither recreates the old file nor overwrites the new one.
        store.migrate_from(&source).unwrap();
        legacy.save(&credentials()).unwrap();
        let mut changed = credentials();
        changed.username = "87654321".into();
        store.save(&changed).unwrap();
        store.migrate_from(&source).unwrap();
        assert_eq!(store.load().unwrap().username, "87654321");
        assert!(source.exists());
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                fs::metadata(path).unwrap().permissions().mode() & 0o777,
                0o600
            );
        }
    }

    #[test]
    fn migration_preserves_invalid_input_and_rejects_index_as_config() {
        let dir = tempfile::tempdir().unwrap();
        let source = dir.path().join("old.json");
        fs::write(&source, b"invalid config").unwrap();
        let target = dir.path().join("postbox/config.json");
        assert!(
            ConfigStore::new(target.clone())
                .migrate_from(&source)
                .is_err()
        );
        assert_eq!(fs::read(source).unwrap(), b"invalid config");
        assert!(!target.exists());
        for name in ["archive-index.json", "ARCHIVE-INDEX.JSON"] {
            assert!(
                ConfigStore::new(PathBuf::from(name))
                    .validate_path()
                    .is_err()
            );
        }
    }

    #[test]
    fn invalid_config_errors_do_not_echo_secrets_and_file_is_preserved() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.json");
        let contents = r#"{"client_secret":"PRIVATE_VALUE","pin":"PRIVATE_PIN"}"#;
        fs::write(&path, contents).unwrap();
        let store = ConfigStore::new(path.clone());
        let error = store.load().err().unwrap().to_string();
        assert!(!error.contains("PRIVATE_"));
        assert_eq!(fs::read_to_string(&path).unwrap(), contents);
        fs::write(&path, vec![b' '; MAX_CONFIG_BYTES as usize + 1]).unwrap();
        assert!(store.load().is_err());
    }
}
