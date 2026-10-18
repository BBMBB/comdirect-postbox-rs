use std::{
    collections::HashMap,
    fs::{self, File},
    io::{Read, Write},
    path::{Path, PathBuf},
};

use anyhow::{Context, Result, bail, ensure};
use chrono::{Datelike, Utc};
use filetime::FileTime;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use tempfile::NamedTempFile;
use walkdir::WalkDir;

use crate::model::{Document, safe_component};

pub const MAX_PDF_BYTES: u64 = 64 * 1024 * 1024;
const SIDECAR_SUFFIX: &str = ".postbox.json";

#[derive(Clone, Serialize, Deserialize)]
struct Record {
    schema_version: u32,
    document: Document,
    sha256: String,
    size: u64,
    archived_at: String,
}

pub struct Archive {
    root: PathBuf,
    records: HashMap<String, (PathBuf, Record)>,
}

pub fn private_dir(path: &Path) -> Result<()> {
    let mut builder = fs::DirBuilder::new();
    builder.recursive(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        builder.mode(0o700);
    }
    builder
        .create(path)
        .context("Verzeichnis konnte nicht angelegt werden")
}

impl Archive {
    pub fn open(root: &Path) -> Result<Self> {
        private_dir(root)?;
        let root = root.canonicalize()?;
        let mut records = HashMap::new();
        for entry in WalkDir::new(&root).follow_links(false) {
            let entry = entry.context("Archiv konnte nicht vollständig gelesen werden")?;
            ensure!(
                !entry.file_type().is_symlink(),
                "Symbolische Links im Archiv sind nicht erlaubt"
            );
            if !entry.file_type().is_file() {
                continue;
            }
            let Some(name) = entry
                .file_name()
                .to_str()
                .and_then(|n| n.strip_suffix(SIDECAR_SUFFIX))
            else {
                continue;
            };
            ensure!(
                entry.metadata()?.len() <= 1024 * 1024,
                "Metadatendatei ist zu groß"
            );
            let record: Record = serde_json::from_reader(File::open(entry.path())?)
                .context("Beschädigte Archiv-Metadaten; bitte aus Sicherung wiederherstellen")?;
            ensure!(
                record.schema_version == 1,
                "Unbekannte Version der Archiv-Metadaten"
            );
            record.document.validate()?;
            let pdf = entry.path().with_file_name(name);
            ensure!(
                pdf.extension().is_some_and(|e| e == "pdf"),
                "Ungültiger PDF-Pfad in Archiv-Metadaten"
            );
            ensure!(
                !records.contains_key(&record.document.document_id),
                "Doppelte Dokument-ID im Archiv"
            );
            records.insert(record.document.document_id.clone(), (pdf, record));
        }
        Ok(Self { root, records })
    }

    fn target(&self, doc: &Document) -> Result<PathBuf> {
        doc.validate()?;
        if let Some((path, _)) = self.records.get(&doc.document_id) {
            return Ok(path.clone());
        }
        // Keep titles short and remove characters disallowed by Windows.
        let title = safe_component(&doc.name, 48);
        Ok(self
            .root
            .join(doc.date()?.year().to_string())
            .join(doc.category())
            .join(format!(
                "{}_{}_{}.pdf",
                doc.date_creation, title, doc.document_id
            )))
    }

    pub fn exists_verified(&mut self, doc: &Document) -> Result<bool> {
        let path = self.target(doc)?;
        if !path.try_exists()? {
            return Ok(false);
        }
        ensure!(
            fs::symlink_metadata(&path)?.is_file(),
            "PDF-Pfad ist keine reguläre Datei"
        );
        let bytes = read_pdf(&path)?;
        let hash = checksum(&bytes);
        if let Some((_, record)) = self.records.get(&doc.document_id) {
            ensure!(
                record.size == bytes.len() as u64 && record.sha256 == hash,
                "Prüfsummenfehler im Archiv; Datei wird nicht überschrieben"
            );
            set_document_time(&path, &record.document)?;
        } else {
            // Recover a crash between publishing the PDF and publishing its sidecar.
            set_document_time(&path, doc)?;
            let record = record(doc, &bytes);
            write_sidecar(&path, &record)?;
            self.records.insert(doc.document_id.clone(), (path, record));
        }
        Ok(true)
    }

    pub fn store(&mut self, doc: &Document, bytes: &[u8]) -> Result<PathBuf> {
        validate_pdf(bytes)?;
        let path = self.prepare_download(doc)?;
        let parent = path.parent().context("Ungültiger Archivpfad")?;
        let record = if let Some((_, original)) = self.records.get(&doc.document_id) {
            ensure!(
                original.sha256 == checksum(bytes) && original.size == bytes.len() as u64,
                "Erneuter Download weicht vom archivierten Original ab"
            );
            original.clone()
        } else {
            record(doc, bytes)
        };
        let mut temp = NamedTempFile::new_in(parent)?;
        temp.write_all(bytes)?;
        temp.as_file().sync_all()?;
        set_document_time(temp.path(), &record.document)?;
        temp.persist_noclobber(&path)
            .context("PDF konnte nicht ohne Überschreiben gespeichert werden")?;
        if !sidecar_path(&path).try_exists()? {
            write_sidecar(&path, &record)?;
        }
        self.records
            .insert(doc.document_id.clone(), (path.clone(), record));
        Ok(path)
    }

    pub fn prepare_download(&self, doc: &Document) -> Result<PathBuf> {
        let path = self.target(doc)?;
        let parent = path.parent().context("Ungültiger Archivpfad")?;
        // Refuse existing symlinks before creating any child directories.
        for ancestor in parent.ancestors().take_while(|p| p.starts_with(&self.root)) {
            if let Ok(meta) = fs::symlink_metadata(ancestor) {
                ensure!(
                    meta.is_dir() && !meta.file_type().is_symlink(),
                    "Unsicherer Archivpfad"
                );
            }
        }
        private_dir(parent)?;
        // Check destination access before a download marks the bank document as read.
        NamedTempFile::new_in(parent).context("Archivordner ist nicht beschreibbar")?;
        Ok(path)
    }
}

fn record(doc: &Document, bytes: &[u8]) -> Record {
    Record {
        schema_version: 1,
        document: doc.clone(),
        sha256: checksum(bytes),
        size: bytes.len() as u64,
        archived_at: Utc::now().to_rfc3339(),
    }
}

fn sidecar_path(path: &Path) -> PathBuf {
    let mut name = path.as_os_str().to_owned();
    name.push(SIDECAR_SUFFIX);
    PathBuf::from(name)
}

fn write_sidecar(path: &Path, record: &Record) -> Result<()> {
    let mut temp = NamedTempFile::new_in(path.parent().context("Ungültiger Metadatenpfad")?)?;
    serde_json::to_writer_pretty(&mut temp, record)?;
    temp.write_all(b"\n")?;
    temp.as_file().sync_all()?;
    temp.persist_noclobber(sidecar_path(path))
        .context("Metadaten konnten nicht gespeichert werden")?;
    Ok(())
}

fn read_pdf(path: &Path) -> Result<Vec<u8>> {
    let mut bytes = Vec::new();
    File::open(path)?
        .take(MAX_PDF_BYTES + 1)
        .read_to_end(&mut bytes)?;
    validate_pdf(&bytes)?;
    Ok(bytes)
}

pub fn validate_pdf(bytes: &[u8]) -> Result<()> {
    ensure!(
        bytes.len() as u64 <= MAX_PDF_BYTES,
        "PDF überschreitet das Limit von 64 MiB"
    );
    if !bytes.starts_with(b"%PDF-")
        || !bytes[bytes.len().saturating_sub(1024)..]
            .windows(5)
            .any(|w| w == b"%%EOF")
    {
        bail!("Unvollständige oder ungültige PDF; Datei wird nicht archiviert");
    }
    Ok(())
}

fn checksum(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}

fn set_document_time(path: &Path, doc: &Document) -> Result<()> {
    let time = FileTime::from_unix_time(doc.timestamp()?.timestamp(), 0);
    filetime::set_file_mtime(path, time).context("Dokumentdatum konnte nicht gesetzt werden")
}

#[cfg(test)]
mod tests {
    use super::*;
    const PDF: &[u8] = b"%PDF-1.4\noriginal bytes\n%%EOF\n";

    fn doc() -> Document {
        serde_json::from_value(serde_json::json!({"documentId":"id-123", "name":"Kontoauszug",
            "dateCreation":"2022-03-04", "mimeType":"application/pdf", "documentMetadata":{"alreadyRead":false}})).unwrap()
    }

    #[test]
    fn archive_preserves_bytes_date_and_recognizes_renamed_files() {
        let dir = tempfile::tempdir().unwrap();
        let mut archive = Archive::open(dir.path()).unwrap();
        let path = archive.store(&doc(), PDF).unwrap();
        assert_eq!(fs::read(&path).unwrap(), PDF);
        assert_eq!(
            FileTime::from_last_modification_time(&fs::metadata(&path).unwrap()).unix_seconds(),
            doc().timestamp().unwrap().timestamp()
        );
        let renamed = path.with_file_name("verschoben.pdf");
        fs::rename(&path, &renamed).unwrap();
        fs::rename(sidecar_path(&path), sidecar_path(&renamed)).unwrap();
        let mut archive = Archive::open(dir.path()).unwrap();
        let mut changed = doc();
        changed.name = "Neuer Titel".into();
        assert!(archive.exists_verified(&changed).unwrap());
        fs::write(&renamed, b"%PDF-1.4\nmanipulated\n%%EOF").unwrap();
        assert!(archive.exists_verified(&changed).is_err());
    }

    #[test]
    fn lost_pdf_is_restored_but_never_overwritten() {
        let dir = tempfile::tempdir().unwrap();
        let mut archive = Archive::open(dir.path()).unwrap();
        let path = archive.store(&doc(), PDF).unwrap();
        assert!(archive.store(&doc(), PDF).is_err());
        fs::remove_file(&path).unwrap();
        assert!(!archive.exists_verified(&doc()).unwrap());
        assert!(
            archive
                .store(&doc(), b"%PDF-1.4\ndifferent\n%%EOF")
                .is_err()
        );
        archive.store(&doc(), PDF).unwrap();
        assert!(archive.exists_verified(&doc()).unwrap());
    }

    #[test]
    fn interrupted_sidecar_write_is_recovered_and_invalid_pdf_rejected() {
        let dir = tempfile::tempdir().unwrap();
        let mut archive = Archive::open(dir.path()).unwrap();
        let path = archive.store(&doc(), PDF).unwrap();
        fs::remove_file(sidecar_path(&path)).unwrap();
        let mut archive = Archive::open(dir.path()).unwrap();
        assert!(archive.exists_verified(&doc()).unwrap());
        assert!(sidecar_path(&path).exists());
        assert!(validate_pdf(b"%PDF-truncated").is_err());
        assert!(validate_pdf(b"<html>Login</html>").is_err());
    }
}
