use std::{
    fs::{self, File},
    io::{Read, Write},
    path::{Path, PathBuf},
};

use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};
use tempfile::NamedTempFile;
use zeroize::{Zeroize, ZeroizeOnDrop, Zeroizing};

use crate::{api::Credentials, archive::private_dir};

const MAX_CONFIG_BYTES: u64 = 64 * 1024;

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
