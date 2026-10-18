use std::{
    fs,
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
        mpsc::{self, Receiver, Sender},
    },
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};

use anyhow::{Context, Result, bail, ensure};
use zeroize::Zeroizing;

use crate::{
    api::{Api, Credentials, TanMethod},
    archive::Archive,
    model::Document,
    safety::{Safety, account_key},
};

#[derive(Clone)]
pub struct DocumentRow {
    pub document: Document,
    pub status: &'static str,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Inventory {
    pub online: usize,
    pub existing: usize,
    pub pending: usize,
    pub skipped: usize,
}

pub enum Command {
    Tan(Zeroizing<String>),
    StartDownloads,
}

pub enum Update {
    Status(String),
    Challenge {
        sms: bool,
    },
    Overview {
        rows: Vec<DocumentRow>,
        inventory: Inventory,
    },
    Progress {
        index: usize,
        status: &'static str,
    },
    Finished {
        message: String,
        failed: bool,
    },
}

pub struct Worker {
    pub updates: Receiver<Update>,
    pub commands: Sender<Command>,
    pub cancelled: Arc<AtomicBool>,
    handle: Option<JoinHandle<()>>,
}

impl Worker {
    pub fn start(
        safety: Arc<Safety>,
        credentials: Credentials,
        output: PathBuf,
        method: TanMethod,
    ) -> Self {
        let (sender, updates) = mpsc::channel();
        let (commands, receiver) = mpsc::channel();
        let cancelled = Arc::new(AtomicBool::new(false));
        let flag = cancelled.clone();
        let handle = thread::spawn(move || {
            let result = run(
                safety,
                credentials,
                output,
                method,
                &sender,
                receiver,
                &flag,
            );
            let (message, failed) = match result {
                Ok(summary) => (summary, false),
                Err(error) => (format!("{error:#}"), true),
            };
            let _ = sender.send(Update::Finished { message, failed });
        });
        Self {
            updates,
            commands,
            cancelled,
            handle: Some(handle),
        }
    }

    pub fn is_finished(&self) -> bool {
        self.handle.as_ref().is_none_or(|h| h.is_finished())
    }

    pub fn join(&mut self) -> Result<()> {
        if let Some(handle) = self.handle.take() {
            ensure!(
                handle.join().is_ok(),
                "Hintergrundprozess unerwartet beendet. Anmeldesicherung vor erneutem Versuch prüfen."
            );
        }
        Ok(())
    }
}

impl Drop for Worker {
    fn drop(&mut self) {
        self.cancelled.store(true, Ordering::Relaxed);
        // Never leave a detached thread sending requests after the UI exits.
        let _ = self.join();
    }
}

fn checkpoint(cancelled: &AtomicBool) -> Result<()> {
    ensure!(!cancelled.load(Ordering::Relaxed), "Vorgang abgebrochen.");
    Ok(())
}

fn account_directory(output: &Path, username: &str) -> Result<PathBuf> {
    ensure!(
        username.len() == 8 && username.bytes().all(|b| b.is_ascii_digit()),
        "Ungültiger Anmeldename für den Archivordner"
    );
    let named = output.join(username);
    let legacy = output.join(&account_key(username)[..16]);
    let is_directory = |path: &Path| -> Result<bool> {
        match fs::symlink_metadata(path) {
            Ok(metadata) => {
                ensure!(
                    metadata.is_dir() && !metadata.file_type().is_symlink(),
                    "Archivpfad ist kein reguläres Verzeichnis"
                );
                Ok(true)
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
            Err(error) => Err(error).context("Archivverzeichnis konnte nicht geprüft werden"),
        }
    };
    if is_directory(&legacy)? {
        ensure!(
            !is_directory(&named)?,
            "Es existieren bereits zwei Kontoarchive: {} und {}. Bitte diese vor dem Start zusammenführen; es wurden keine Dateien überschrieben oder Bankanfragen gesendet.",
            legacy.display(),
            named.display()
        );
        fs::rename(&legacy, &named).context(
            "Vorhandenes Kontoarchiv konnte nicht auf den Anmeldenamen umbenannt werden",
        )?;
    }
    Ok(named)
}

fn run(
    safety: Arc<Safety>,
    credentials: Credentials,
    output: PathBuf,
    method: TanMethod,
    updates: &Sender<Update>,
    commands: Receiver<Command>,
    cancelled: &AtomicBool,
) -> Result<String> {
    credentials.validate()?;
    let username = Zeroizing::new(credentials.username.clone());
    let _ = updates.send(Update::Status("Lokales Archiv prüfen …".into()));
    let root = account_directory(&output, &username)?;
    let mut archive = Archive::open(&root)?;
    let mut api = Api::new(credentials)?;
    checkpoint(cancelled)?;
    safety.begin(&username)?;
    let _ = updates.send(Update::Status("Einmalige Anmeldung bei comdirect …".into()));
    let result = (|| {
        api.login(method, |challenge| {
            let _ = updates.send(Update::Challenge { sms: challenge.typ == "M_TAN" });
            let deadline = Instant::now() + Duration::from_secs(240);
            loop {
                checkpoint(cancelled)?;
                ensure!(Instant::now() < deadline, "TAN-Freigabe abgelaufen. Keine neue TAN angefordert; zuerst im Webbanking anmelden.");
                match commands.recv_timeout(Duration::from_millis(100)) {
                    Ok(Command::Tan(tan)) => return Ok(tan),
                    Ok(Command::StartDownloads) => bail!("Downloadbestätigung vor abgeschlossener Anmeldung abgelehnt"),
                    Err(mpsc::RecvTimeoutError::Timeout) => (),
                    Err(mpsc::RecvTimeoutError::Disconnected) => bail!("TAN-Freigabe abgebrochen"),
                }
            }
        }, || checkpoint(cancelled)).context("Anmeldung nicht vollständig abgeschlossen. Die lokale Anmeldesicherung bleibt aktiv; vor einem neuen Versuch zuerst im Webbanking mit TAN anmelden.")?;
        safety.clear(&username)?;
        review_and_download(&mut api, &mut archive, updates, &commands, cancelled)
    })();
    let _ = updates.send(Update::Status("Banksitzung beenden …".into()));
    if api.logout().is_err() {
        return result.map(|summary| {
            format!(
                "{summary} Abmeldung nicht bestätigt; Tokens verfallen ohne weitere Erneuerung."
            )
        });
    }
    result
}

struct DownloadPlan {
    rows: Vec<DocumentRow>,
    pending: Vec<usize>,
    inventory: Inventory,
}

impl DownloadPlan {
    fn build(
        documents: Vec<Document>,
        archive: &mut Archive,
        mut progress: impl FnMut(usize, usize) -> Result<()>,
    ) -> Result<Self> {
        let mut inventory = Inventory {
            online: documents.len(),
            ..Inventory::default()
        };
        let mut rows = Vec::with_capacity(documents.len());
        let mut pending = Vec::new();
        for (index, document) in documents.into_iter().enumerate() {
            progress(index, inventory.online)?;
            let status = if !document.is_pdf() {
                inventory.skipped += 1;
                "Kein PDF"
            } else if archive.exists_verified(&document)? {
                inventory.existing += 1;
                "Vorhanden ✓"
            } else {
                inventory.pending += 1;
                pending.push(index);
                "Ausstehend"
            };
            rows.push(DocumentRow { document, status });
        }
        Ok(Self {
            rows,
            pending,
            inventory,
        })
    }
}

#[derive(Debug, PartialEq, Eq)]
enum Approval {
    Confirmed,
    Cancelled,
    Expired,
}

fn wait_for_download_approval(
    commands: &Receiver<Command>,
    cancelled: &AtomicBool,
    deadline: Instant,
) -> Result<Approval> {
    loop {
        if cancelled.load(Ordering::Relaxed) {
            return Ok(Approval::Cancelled);
        }
        if Instant::now() >= deadline {
            return Ok(Approval::Expired);
        }
        let wait = deadline
            .saturating_duration_since(Instant::now())
            .min(Duration::from_millis(100));
        match commands.recv_timeout(wait) {
            Ok(Command::StartDownloads) => {
                if cancelled.load(Ordering::Relaxed) {
                    return Ok(Approval::Cancelled);
                }
                if Instant::now() >= deadline {
                    return Ok(Approval::Expired);
                }
                return Ok(Approval::Confirmed);
            }
            Ok(Command::Tan(_)) => bail!("Eine TAN-Eingabe kann keine Downloads bestätigen"),
            Err(mpsc::RecvTimeoutError::Timeout) => (),
            Err(mpsc::RecvTimeoutError::Disconnected) => return Ok(Approval::Cancelled),
        }
    }
}

pub(crate) fn review_and_download(
    api: &mut Api,
    archive: &mut Archive,
    updates: &Sender<Update>,
    commands: &Receiver<Command>,
    cancelled: &AtomicBool,
) -> Result<String> {
    let _ = updates.send(Update::Status("Dokumentenliste abrufen …".into()));
    let mut documents = api.documents(|| checkpoint(cancelled))?;
    documents.sort_by(|a, b| {
        b.date_creation
            .cmp(&a.date_creation)
            .then(a.document_id.cmp(&b.document_id))
    });
    let plan = DownloadPlan::build(documents, archive, |index, total| {
        checkpoint(cancelled)?;
        let _ = updates.send(Update::Status(format!(
            "Lokalen Bestand prüfen: {} / {total} …",
            index + 1
        )));
        Ok(())
    })?;
    updates
        .send(Update::Overview {
            rows: plan.rows.clone(),
            inventory: plan.inventory,
        })
        .context("Übersicht konnte nicht angezeigt werden; keine PDFs heruntergeladen")?;
    match wait_for_download_approval(commands, cancelled, api.access_token_deadline())? {
        Approval::Cancelled => return Ok("Beendet ohne Download. Es wurden keine PDFs abgerufen.".into()),
        Approval::Expired => return Ok("Die Banksitzung ist während der Bestandsprüfung oder Übersicht abgelaufen. Es wurden keine PDFs abgerufen. Bitte für einen neuen Abruf erneut anmelden.".into()),
        Approval::Confirmed => (),
    }
    let (mut downloaded, mut existing) = (0, plan.inventory.existing);
    for index in plan.pending {
        checkpoint(cancelled)?;
        let doc = &plan.rows[index].document;
        // A file may have appeared while the user reviewed the overview.
        if archive.exists_verified(doc)? {
            existing += 1;
            let _ = updates.send(Update::Progress {
                index,
                status: "Vorhanden ✓",
            });
            continue;
        }
        archive.prepare_download(doc)?;
        let _ = updates.send(Update::Progress {
            index,
            status: "Lädt …",
        });
        let _ = updates.send(Update::Status(format!(
            "PDF {} / {} herunterladen …",
            downloaded + 1,
            plan.inventory.pending
        )));
        let bytes = api.download(doc).with_context(|| {
            format!(
                "PDF-Abruf für Dokument {} vom {} fehlgeschlagen",
                doc.document_id, doc.date_creation
            )
        })?;
        archive.store(doc, &bytes)?;
        downloaded += 1;
        let _ = updates.send(Update::Progress {
            index,
            status: "Gespeichert ✓",
        });
    }
    Ok(format!(
        "Fertig: {downloaded} neu gespeichert, {existing} vorhanden und geprüft, {} ohne PDF übersprungen.",
        plan.inventory.skipped
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    const PDF: &[u8] = b"%PDF-1.4\noriginal\n%%EOF\n";

    fn document(id: &str, mime: &str) -> Document {
        serde_json::from_value(serde_json::json!({"documentId":id,"name":"Finanzreport","dateCreation":"2024-02-03","mimeType":mime})).unwrap()
    }

    #[test]
    fn plan_counts_only_verified_online_pdfs_and_stops_on_corruption() {
        let dir = tempfile::tempdir().unwrap();
        let mut archive = Archive::open(dir.path()).unwrap();
        let existing = document("existing", "application/pdf");
        let path = archive.store(&existing, PDF).unwrap();
        archive
            .store(&document("offline-only", "application/pdf"), PDF)
            .unwrap();
        let documents = vec![
            existing.clone(),
            document("missing", "application/pdf"),
            document("html", "text/html"),
        ];
        let plan = DownloadPlan::build(documents.clone(), &mut archive, |_, _| Ok(())).unwrap();
        assert_eq!(
            plan.inventory,
            Inventory {
                online: 3,
                existing: 1,
                pending: 1,
                skipped: 1
            }
        );
        assert_eq!(plan.pending, [1]);
        fs::write(path, b"%PDF-1.4\nchanged\n%%EOF\n").unwrap();
        assert!(DownloadPlan::build(documents, &mut archive, |_, _| Ok(())).is_err());
    }

    #[test]
    fn approval_requires_a_download_command_and_honors_cancel_and_expiry() {
        let cancelled = AtomicBool::new(false);
        let (sender, commands) = mpsc::channel();
        sender
            .send(Command::Tan(Zeroizing::new("123456".into())))
            .unwrap();
        assert!(
            wait_for_download_approval(
                &commands,
                &cancelled,
                Instant::now() + Duration::from_secs(1)
            )
            .is_err()
        );
        sender.send(Command::StartDownloads).unwrap();
        assert_eq!(
            wait_for_download_approval(
                &commands,
                &cancelled,
                Instant::now() + Duration::from_secs(1)
            )
            .unwrap(),
            Approval::Confirmed
        );
        cancelled.store(true, Ordering::Relaxed);
        assert_eq!(
            wait_for_download_approval(
                &commands,
                &cancelled,
                Instant::now() + Duration::from_secs(1)
            )
            .unwrap(),
            Approval::Cancelled
        );
        cancelled.store(false, Ordering::Relaxed);
        assert_eq!(
            wait_for_download_approval(&commands, &cancelled, Instant::now()).unwrap(),
            Approval::Expired
        );
    }

    #[test]
    fn legacy_archive_is_renamed_without_changing_pdf_dates_or_ids() {
        let dir = tempfile::tempdir().unwrap();
        let legacy = dir.path().join(&account_key("12345678")[..16]);
        let mut archive = Archive::open(&legacy).unwrap();
        let doc = document("existing", "application/pdf");
        let original = archive.store(&doc, PDF).unwrap();
        let modified = fs::metadata(original).unwrap().modified().unwrap();
        drop(archive);
        let named = account_directory(dir.path(), "12345678").unwrap();
        assert_eq!(named, dir.path().join("12345678"));
        assert!(!legacy.exists());
        let mut archive = Archive::open(&named).unwrap();
        assert!(archive.exists_verified(&doc).unwrap());
        let pdf = walkdir::WalkDir::new(&named)
            .into_iter()
            .map(Result::unwrap)
            .find(|entry| entry.path().extension().is_some_and(|e| e == "pdf"))
            .unwrap();
        assert_eq!(
            fs::metadata(pdf.path()).unwrap().modified().unwrap(),
            modified
        );
    }

    #[test]
    fn two_existing_account_directories_are_not_overwritten() {
        let dir = tempfile::tempdir().unwrap();
        let legacy = dir.path().join(&account_key("12345678")[..16]);
        let named = dir.path().join("12345678");
        fs::create_dir(&legacy).unwrap();
        fs::create_dir(&named).unwrap();
        assert!(account_directory(dir.path(), "12345678").is_err());
        assert!(legacy.exists() && named.exists());
        assert!(account_directory(dir.path(), "../../outside").is_err());
    }
}
