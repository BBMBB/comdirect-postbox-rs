use std::{
    collections::{BTreeMap, HashSet},
    fs::{self, File},
    io::{Read, Write},
    path::{Component, Path, PathBuf},
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
const MAX_INDEX_BYTES: u64 = 256 * 1024 * 1024;
pub const INDEX_FILE_NAME: &str = "archive-index.json";

#[derive(Clone, PartialEq, Serialize, Deserialize)]
struct Record {
    schema_version: u32,
    document: Document,
    sha256: String,
    size: u64,
    archived_at: String,
}

#[derive(Serialize, Deserialize)]
struct Index {
    schema_version: u32,
    archives: Vec<IndexedArchive>,
}

#[derive(Serialize, Deserialize)]
struct IndexedArchive {
    root: RootLocation,
    records: BTreeMap<String, IndexedRecord>,
}

#[derive(PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum RootLocation {
    Relative(String),
    Absolute(PathBuf),
}

impl RootLocation {
    fn new(root: &Path, index_directory: &Path) -> Result<Self> {
        match root.strip_prefix(index_directory) {
            Ok(relative) => Ok(Self::Relative(relative_path(relative)?)),
            Err(_) => Ok(Self::Absolute(root.to_owned())),
        }
    }

    fn validate(&self) -> Result<()> {
        match self {
            Self::Relative(path) if path.is_empty() => Ok(()),
            Self::Relative(path) => validate_relative_path(path),
            Self::Absolute(path) => {
                ensure!(
                    path.is_absolute(),
                    "Ungültiger Archivordner im zentralen Index"
                );
                Ok(())
            }
        }
    }
}

#[derive(PartialEq, Serialize, Deserialize)]
struct IndexedRecord {
    // Forward slashes keep paths within an archive portable between operating systems.
    path: String,
    #[serde(flatten)]
    record: Record,
}

pub struct Archive {
    root: PathBuf,
    index_path: PathBuf,
    index: Index,
    selected: usize,
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
    pub fn open(root: &Path, index_path: &Path) -> Result<Self> {
        let parent = index_path
            .parent()
            .filter(|p| !p.as_os_str().is_empty())
            .unwrap_or(Path::new("."));
        private_dir(parent)?;
        let parent = parent.canonicalize()?;
        let index_path = parent.join(index_path.file_name().context("Ungültiger Indexpfad")?);
        let mut index = if regular_file_exists(&index_path)? {
            ensure!(
                fs::metadata(&index_path)?.len() <= MAX_INDEX_BYTES,
                "Zentraler Archivindex ist zu groß"
            );
            serde_json::from_reader::<_, Index>(File::open(&index_path)?.take(MAX_INDEX_BYTES + 1))
                .context(
                    "Zentraler Archivindex ist beschädigt; bitte aus Sicherung wiederherstellen",
                )?
        } else {
            Index {
                schema_version: 1,
                archives: Vec::new(),
            }
        };
        ensure!(
            index.schema_version == 1,
            "Unbekannte Version des zentralen Archivindex"
        );
        let mut roots = HashSet::new();
        for archive in &index.archives {
            archive.root.validate()?;
            ensure!(
                roots.insert(serde_json::to_string(&archive.root)?),
                "Doppelter Archivordner im zentralen Index"
            );
            let mut paths = HashSet::new();
            for (id, entry) in &archive.records {
                validate_record(&entry.record)?;
                validate_pdf_path(&entry.path)?;
                ensure!(
                    id == &entry.record.document.document_id,
                    "Widersprüchliche Dokument-ID im zentralen Index"
                );
                ensure!(
                    paths.insert(&entry.path),
                    "Mehrere Dokumente verwenden denselben PDF-Pfad"
                );
            }
        }
        private_dir(root)?;
        let root = root.canonicalize()?;
        let location = RootLocation::new(&root, &parent)?;
        location.validate()?;
        let selected = match index.archives.iter().position(|a| a.root == location) {
            Some(selected) => selected,
            None => {
                index.archives.push(IndexedArchive {
                    root: location,
                    records: BTreeMap::new(),
                });
                index.archives.len() - 1
            }
        };
        let records = &mut index.archives[selected].records;
        let mut sidecars = Vec::new();
        // Collect and validate the entire legacy archive before changing any files.
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
            let record: Record = serde_json::from_reader(
                File::open(entry.path())?.take(1024 * 1024 + 1),
            )
            .context("Beschädigte Archiv-Metadaten; bitte aus Sicherung wiederherstellen")?;
            validate_record(&record)?;
            let pdf = entry.path().with_file_name(name);
            let path = relative_path(pdf.strip_prefix(&root)?)?;
            validate_pdf_path(&path)?;
            let id = record.document.document_id.clone();
            let imported = IndexedRecord { path, record };
            if let Some(existing) = records.get(&id) {
                ensure!(
                    existing == &imported,
                    "Widersprüchliche Metadaten für dieselbe Dokument-ID; Begleitdateien bleiben erhalten"
                );
            } else {
                ensure!(
                    !records.values().any(|r| r.path == imported.path),
                    "Doppelter PDF-Pfad in Archiv-Metadaten"
                );
                records.insert(id, imported);
            }
            sidecars.push(entry.into_path());
        }
        let archive = Self {
            root,
            index_path,
            index,
            selected,
        };
        // Atomic replacement and flushing happen before the first legacy file is removed.
        // Repeating this migration after an interruption accepts identical records.
        archive.save_index()?;
        for sidecar in sidecars {
            fs::remove_file(&sidecar)
                .context("Metadaten sind zentral gespeichert; alte Begleitdatei konnte nicht entfernt werden. Beim nächsten Start wird die Übernahme fortgesetzt")?;
        }
        Ok(archive)
    }

    fn records(&self) -> &BTreeMap<String, IndexedRecord> {
        &self.index.archives[self.selected].records
    }

    fn save_index(&self) -> Result<()> {
        regular_file_exists(&self.index_path)?;
        let parent = self.index_path.parent().context("Ungültiger Indexpfad")?;
        let mut temp = NamedTempFile::new_in(parent)
            .context("Ordner des Archivindex ist nicht beschreibbar")?;
        serde_json::to_writer_pretty(&mut temp, &self.index)?;
        temp.write_all(b"\n")?;
        ensure!(
            temp.as_file().metadata()?.len() <= MAX_INDEX_BYTES,
            "Zentraler Archivindex überschreitet das Größenlimit"
        );
        temp.as_file().sync_all()?;
        temp.persist(&self.index_path)
            .context("Zentraler Archivindex konnte nicht atomar gespeichert werden")?;
        #[cfg(unix)]
        File::open(parent)?
            .sync_all()
            .context("Zentraler Archivindex konnte nicht dauerhaft gesichert werden")?;
        Ok(())
    }

    fn remember(&mut self, path: &Path, record: Record) -> Result<()> {
        let path = relative_path(path.strip_prefix(&self.root)?)?;
        validate_pdf_path(&path)?;
        let id = record.document.document_id.clone();
        let previous = self.index.archives[self.selected]
            .records
            .insert(id.clone(), IndexedRecord { path, record });
        if let Err(error) = self.save_index() {
            let records = &mut self.index.archives[self.selected].records;
            if let Some(previous) = previous {
                records.insert(id, previous);
            } else {
                records.remove(&id);
            }
            return Err(error);
        }
        Ok(())
    }

    fn target(&self, doc: &Document) -> Result<PathBuf> {
        doc.validate()?;
        if let Some(entry) = self.records().get(&doc.document_id) {
            return Ok(self.root.join(&entry.path));
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
        if let Some(entry) = self.records().get(&doc.document_id) {
            let record = &entry.record;
            ensure!(
                record.size == bytes.len() as u64 && record.sha256 == hash,
                "Prüfsummenfehler im Archiv; Datei wird nicht überschrieben"
            );
            set_document_time(&path, &record.document)?;
        } else {
            // Recover a crash between publishing the PDF and updating the central index.
            set_document_time(&path, doc)?;
            let record = record(doc, &bytes);
            self.remember(&path, record)?;
        }
        Ok(true)
    }

    pub fn store(&mut self, doc: &Document, bytes: &[u8]) -> Result<PathBuf> {
        validate_pdf(bytes)?;
        let path = self.prepare_download(doc)?;
        let parent = path.parent().context("Ungültiger Archivpfad")?;
        let record = if let Some(entry) = self.records().get(&doc.document_id) {
            let original = &entry.record;
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
        self.remember(&path, record)?;
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

fn regular_file_exists(path: &Path) -> Result<bool> {
    match fs::symlink_metadata(path) {
        Ok(metadata) => {
            ensure!(
                metadata.is_file(),
                "Archivindex muss eine reguläre Datei sein"
            );
            Ok(true)
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(error).context("Archivindex konnte nicht geprüft werden"),
    }
}

fn validate_record(record: &Record) -> Result<()> {
    ensure!(
        record.schema_version == 1,
        "Unbekannte Version der Archiv-Metadaten"
    );
    record.document.validate()?;
    ensure!(
        record.document.is_pdf(),
        "Metadaten gehören nicht zu einem PDF-Dokument"
    );
    ensure!(
        record.size <= MAX_PDF_BYTES
            && record.sha256.len() == 64
            && record
                .sha256
                .bytes()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b)),
        "Ungültige Größe oder Prüfsumme in Archiv-Metadaten"
    );
    Ok(())
}

fn relative_path(path: &Path) -> Result<String> {
    path.components()
        .map(|component| match component {
            Component::Normal(part) => part
                .to_str()
                .map(str::to_owned)
                .context("Archivpfad ist kein gültiges UTF-8"),
            _ => bail!("Ungültiger relativer Archivpfad"),
        })
        .collect::<Result<Vec<_>>>()
        .map(|parts| parts.join("/"))
}

fn validate_relative_path(path: &str) -> Result<()> {
    ensure!(
        !path.is_empty()
            && !path.contains(['\\', ':'])
            && path
                .split('/')
                .all(|part| !part.is_empty() && part != "." && part != ".."),
        "Unsicherer relativer Pfad im Archivindex"
    );
    Ok(())
}

fn validate_pdf_path(path: &str) -> Result<()> {
    validate_relative_path(path)?;
    ensure!(
        path.ends_with(".pdf"),
        "Ungültiger PDF-Pfad in Archiv-Metadaten"
    );
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

    fn sidecar_path(pdf: &Path) -> PathBuf {
        let mut path = pdf.as_os_str().to_owned();
        path.push(SIDECAR_SUFFIX);
        path.into()
    }

    fn legacy_file(
        root: &Path,
        relative: &str,
        doc: &Document,
        present: bool,
    ) -> (PathBuf, Record) {
        let pdf = root.join(relative);
        private_dir(pdf.parent().unwrap()).unwrap();
        if present {
            fs::write(&pdf, PDF).unwrap();
            set_document_time(&pdf, doc).unwrap();
        }
        let record = record(doc, PDF);
        fs::write(sidecar_path(&pdf), serde_json::to_vec(&record).unwrap()).unwrap();
        (pdf, record)
    }

    fn assert_only_pdfs(root: &Path) {
        for entry in WalkDir::new(root) {
            let entry = entry.unwrap();
            if entry.file_type().is_file() {
                assert_eq!(entry.path().extension().unwrap(), "pdf");
            }
        }
    }

    #[test]
    fn central_index_preserves_bytes_dates_metadata_and_changed_titles() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("postbox/12345678");
        let index = dir.path().join(INDEX_FILE_NAME);
        let mut archive = Archive::open(&root, &index).unwrap();
        let path = archive.store(&doc(), PDF).unwrap();
        assert_eq!(fs::read(&path).unwrap(), PDF);
        assert_eq!(
            FileTime::from_last_modification_time(&fs::metadata(&path).unwrap()).unix_seconds(),
            doc().timestamp().unwrap().timestamp()
        );
        let archived_at = archive.records()["id-123"].record.archived_at.clone();
        drop(archive);
        let mut archive = Archive::open(&root, &index).unwrap();
        let mut changed = doc();
        changed.name = "Neuer Titel".into();
        changed.date_creation = "2026-10-17".into();
        assert!(archive.exists_verified(&changed).unwrap());
        assert_eq!(archive.target(&changed).unwrap(), path);
        assert_eq!(archive.records()["id-123"].record.document, doc());
        assert_eq!(archive.records()["id-123"].record.archived_at, archived_at);
        assert_only_pdfs(&root);
        fs::write(&path, b"%PDF-1.4\nmanipulated\n%%EOF").unwrap();
        assert!(archive.exists_verified(&changed).is_err());
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                fs::metadata(index).unwrap().permissions().mode() & 0o777,
                0o600
            );
        }
    }

    #[test]
    fn migrated_missing_pdf_keeps_original_hash_date_and_metadata() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("postbox/12345678");
        let index = dir.path().join(INDEX_FILE_NAME);
        let (present, original) =
            legacy_file(&root, "2022/Kontoauszuege/manuell.pdf", &doc(), true);
        let modified = fs::metadata(&present).unwrap().modified().unwrap();
        let mut missing_doc = doc();
        missing_doc.document_id = "deleted".into();
        let (missing, missing_record) =
            legacy_file(&root, "2022/Kontoauszuege/fehlend.pdf", &missing_doc, false);
        let mut archive = Archive::open(&root, &index).unwrap();
        assert!(archive.records()["id-123"].record == original);
        assert!(archive.records()["deleted"].record == missing_record);
        assert_eq!(
            fs::metadata(&present).unwrap().modified().unwrap(),
            modified
        );
        assert!(archive.exists_verified(&doc()).unwrap());
        assert!(!archive.exists_verified(&missing_doc).unwrap());
        assert!(
            archive
                .store(&missing_doc, b"%PDF-1.4\ndifferent\n%%EOF")
                .is_err()
        );
        assert!(!missing.exists());
        missing_doc.date_creation = "2026-10-17".into();
        assert_eq!(archive.store(&missing_doc, PDF).unwrap(), missing);
        assert_eq!(
            FileTime::from_last_modification_time(&fs::metadata(&missing).unwrap()).unix_seconds(),
            doc().timestamp().unwrap().timestamp()
        );
        drop(archive);
        let mut archive = Archive::open(&root, &index).unwrap();
        assert!(archive.exists_verified(&missing_doc).unwrap());
        assert_only_pdfs(&root);
    }

    #[test]
    fn interrupted_migration_can_resume_with_some_sidecars_already_removed() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("postbox");
        let index = dir.path().join(INDEX_FILE_NAME);
        let (pdf, original) = legacy_file(&root, "2022/auszug.pdf", &doc(), true);
        let sidecar_bytes = fs::read(sidecar_path(&pdf)).unwrap();
        drop(Archive::open(&root, &index).unwrap());
        assert!(!sidecar_path(&pdf).exists());
        fs::write(sidecar_path(&pdf), sidecar_bytes).unwrap();
        let mut other = doc();
        other.document_id = "another".into();
        legacy_file(&root, "2022/zweite.pdf", &other, false);
        let mut archive = Archive::open(&root, &index).unwrap();
        assert_eq!(archive.records().len(), 2);
        assert!(archive.records()["id-123"].record == original);
        assert!(archive.exists_verified(&doc()).unwrap());
        assert!(!archive.exists_verified(&other).unwrap());
        assert_only_pdfs(&root);
    }

    #[test]
    fn invalid_or_conflicting_sidecars_are_preserved_without_changing_index() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("postbox");
        let index = dir.path().join(INDEX_FILE_NAME);
        let (pdf, original) = legacy_file(&root, "2022/auszug.pdf", &doc(), true);
        drop(Archive::open(&root, &index).unwrap());
        let saved_index = fs::read(&index).unwrap();
        let mut conflicting = original.clone();
        conflicting.sha256 = checksum(b"different");
        for bytes in [
            b"broken JSON".to_vec(),
            serde_json::to_vec(&conflicting).unwrap(),
        ] {
            fs::write(sidecar_path(&pdf), &bytes).unwrap();
            assert!(Archive::open(&root, &index).is_err());
            assert_eq!(fs::read(sidecar_path(&pdf)).unwrap(), bytes);
            assert_eq!(fs::read(&index).unwrap(), saved_index);
            assert_eq!(fs::read(&pdf).unwrap(), PDF);
        }
        // Validation of every sidecar must finish before the first one is removed.
        fs::write(sidecar_path(&pdf), serde_json::to_vec(&original).unwrap()).unwrap();
        let broken = root.join("2022/z.pdf.postbox.json");
        fs::write(&broken, b"broken").unwrap();
        assert!(Archive::open(&root, &index).is_err());
        assert!(sidecar_path(&pdf).exists());
        assert!(broken.exists());
        assert_eq!(fs::read(&index).unwrap(), saved_index);
    }

    #[test]
    fn invalid_or_unavailable_index_never_deletes_legacy_files() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("postbox");
        let index = dir.path().join(INDEX_FILE_NAME);
        let (pdf, _) = legacy_file(&root, "2022/auszug.pdf", &doc(), true);
        fs::create_dir(&index).unwrap();
        assert!(Archive::open(&root, &index).is_err());
        assert!(sidecar_path(&pdf).exists());
        fs::remove_dir(&index).unwrap();
        fs::write(&index, b"broken index").unwrap();
        assert!(Archive::open(&root, &index).is_err());
        assert_eq!(fs::read(&index).unwrap(), b"broken index");
        assert!(sidecar_path(&pdf).exists());
        assert_eq!(fs::read(&pdf).unwrap(), PDF);
    }

    #[test]
    fn multiple_accounts_and_targets_share_one_index_without_mixing_documents() {
        let dir = tempfile::tempdir().unwrap();
        let index = dir.path().join("settings").join(INDEX_FILE_NAME);
        let roots = [
            dir.path().join("postbox/12345678"),
            dir.path().join("postbox/87654321"),
            dir.path().join("other/12345678"),
        ];
        for (i, root) in roots.iter().enumerate() {
            let mut archive = Archive::open(root, &index).unwrap();
            assert!(!archive.exists_verified(&doc()).unwrap());
            archive
                .store(&doc(), format!("%PDF-1.4\naccount {i}\n%%EOF").as_bytes())
                .unwrap();
        }
        for (i, root) in roots.iter().enumerate() {
            let mut archive = Archive::open(root, &index).unwrap();
            assert!(archive.exists_verified(&doc()).unwrap());
            assert_eq!(
                fs::read(archive.target(&doc()).unwrap()).unwrap(),
                format!("%PDF-1.4\naccount {i}\n%%EOF").as_bytes()
            );
            assert_only_pdfs(root);
        }
        let saved: Index = serde_json::from_slice(&fs::read(&index).unwrap()).unwrap();
        assert_eq!(saved.archives.len(), 3);
    }

    #[test]
    fn portable_directory_can_move_with_index_and_relative_archive_paths() {
        let dir = tempfile::tempdir().unwrap();
        let app = dir.path().join("app");
        let root = app.join("postbox/12345678");
        let index = app.join(INDEX_FILE_NAME);
        let (pdf, _) = legacy_file(&root, "2022/manuell-umbenannt.pdf", &doc(), true);
        let modified = fs::metadata(pdf).unwrap().modified().unwrap();
        drop(Archive::open(&root, &index).unwrap());
        let moved = dir.path().join("moved");
        fs::rename(app, &moved).unwrap();
        let mut archive = Archive::open(
            &moved.join("postbox/12345678"),
            &moved.join(INDEX_FILE_NAME),
        )
        .unwrap();
        let mut changed = doc();
        changed.name = "Different title".into();
        assert!(archive.exists_verified(&changed).unwrap());
        assert_eq!(archive.records().len(), 1);
        assert_eq!(
            fs::metadata(archive.target(&changed).unwrap())
                .unwrap()
                .modified()
                .unwrap(),
            modified
        );
    }

    #[test]
    fn unsafe_index_paths_and_unknown_versions_are_rejected_without_overwriting() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("postbox");
        let index = dir.path().join(INDEX_FILE_NAME);
        let mut archive = Archive::open(&root, &index).unwrap();
        let pdf = archive.store(&doc(), PDF).unwrap();
        drop(archive);
        let saved: serde_json::Value = serde_json::from_slice(&fs::read(&index).unwrap()).unwrap();
        for path in [
            "../outside.pdf",
            "/outside.pdf",
            "C:/outside.pdf",
            "2022/../../outside.pdf",
            "2022\\outside.pdf",
        ] {
            let mut invalid = saved.clone();
            invalid["archives"][0]["records"]["id-123"]["path"] = path.into();
            let bytes = serde_json::to_vec(&invalid).unwrap();
            fs::write(&index, &bytes).unwrap();
            assert!(Archive::open(&root, &index).is_err());
            assert_eq!(fs::read(&index).unwrap(), bytes);
            assert_eq!(fs::read(&pdf).unwrap(), PDF);
        }
        let mut invalid = saved;
        invalid["schema_version"] = 99.into();
        fs::write(&index, serde_json::to_vec(&invalid).unwrap()).unwrap();
        assert!(Archive::open(&root, &index).is_err());
    }

    #[test]
    fn lost_pdf_is_restored_but_never_overwritten_after_restart() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("postbox");
        let index = dir.path().join(INDEX_FILE_NAME);
        let mut archive = Archive::open(&root, &index).unwrap();
        let path = archive.store(&doc(), PDF).unwrap();
        assert!(archive.store(&doc(), PDF).is_err());
        fs::remove_file(&path).unwrap();
        drop(archive);
        let mut archive = Archive::open(&root, &index).unwrap();
        assert!(!archive.exists_verified(&doc()).unwrap());
        assert!(
            archive
                .store(&doc(), b"%PDF-1.4\ndifferent\n%%EOF")
                .is_err()
        );
        archive.store(&doc(), PDF).unwrap();
        assert!(archive.exists_verified(&doc()).unwrap());
        assert_only_pdfs(&root);
    }

    #[test]
    fn interrupted_index_update_is_recovered_and_invalid_pdf_rejected() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("postbox");
        let index = dir.path().join(INDEX_FILE_NAME);
        let mut archive = Archive::open(&root, &index).unwrap();
        let path = archive.store(&doc(), PDF).unwrap();
        let mut saved: Index = serde_json::from_slice(&fs::read(&index).unwrap()).unwrap();
        saved.archives[0].records.clear();
        fs::write(&index, serde_json::to_vec(&saved).unwrap()).unwrap();
        drop(archive);
        let mut archive = Archive::open(&root, &index).unwrap();
        assert!(archive.exists_verified(&doc()).unwrap());
        assert!(path.exists());
        drop(archive);
        assert!(
            Archive::open(&root, &index)
                .unwrap()
                .exists_verified(&doc())
                .unwrap()
        );
        assert_only_pdfs(&root);
        assert!(validate_pdf(b"%PDF-truncated").is_err());
        assert!(validate_pdf(b"<html>Login</html>").is_err());
    }
}
