use std::{
    fs::{self, File, OpenOptions},
    io::Write,
    path::{Path, PathBuf},
};

use anyhow::{Context, Result, ensure};
use fs2::FileExt;
use sha2::{Digest, Sha256};

use crate::archive::private_dir;

pub fn account_key(username: &str) -> String {
    format!("{:x}", Sha256::digest(username.as_bytes()))
}

pub struct Safety {
    root: PathBuf,
    _lock: File,
}

impl Safety {
    pub fn open(root: &Path) -> Result<Self> {
        private_dir(root)?;
        let lock = OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(root.join("application.lockfile"))?;
        lock.try_lock_exclusive()
            .context("Eine andere Instanz läuft bereits; keine parallelen Anmeldungen erlaubt")?;
        Ok(Self {
            root: root.to_owned(),
            _lock: lock,
        })
    }

    fn path(&self, username: &str) -> PathBuf {
        self.root
            .join(format!("auth-{}.pending", account_key(username)))
    }

    pub fn begin(&self, username: &str) -> Result<()> {
        let path = self.path(username);
        ensure!(
            !path.try_exists()?,
            "Lokale Anmeldesicherung aktiv. Erst im comdirect-Webbanking erfolgreich mit TAN anmelden; anschließend --entsperren verwenden. Es wurde keine Bankanfrage gesendet."
        );
        let mut options = OpenOptions::new();
        options.create_new(true).write(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let mut file = options.open(path)?;
        file.write_all(
            b"Anmeldung begonnen. Bei Abbruch zuerst im Webbanking mit TAN anmelden.\n",
        )?;
        file.sync_all()?;
        Ok(())
    }

    pub fn clear(&self, username: &str) -> Result<()> {
        match fs::remove_file(self.path(username)) {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(e).context("Lokale Anmeldesicherung konnte nicht aufgehoben werden"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn failure_latch_survives_process_restart_and_archive_changes() {
        let dir = tempfile::tempdir().unwrap();
        {
            let state = Safety::open(dir.path()).unwrap();
            state.begin("12345678").unwrap();
            assert!(state.begin("12345678").is_err());
            assert!(Safety::open(dir.path()).is_err());
        }
        let state = Safety::open(dir.path()).unwrap();
        assert!(state.begin("12345678").is_err());
        state.clear("12345678").unwrap();
        state.begin("12345678").unwrap();
        assert!(
            !state
                .path("12345678")
                .to_string_lossy()
                .contains("12345678")
        );
    }
}
