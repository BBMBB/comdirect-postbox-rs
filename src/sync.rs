use std::{
    path::PathBuf,
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

pub struct DocumentRow {
    pub document: Document,
    pub status: &'static str,
}

pub enum Update {
    Status(String),
    Challenge { sms: bool },
    Documents(Vec<DocumentRow>),
    Progress { index: usize, status: &'static str },
    Finished { message: String, failed: bool },
}

pub struct Worker {
    pub updates: Receiver<Update>,
    pub confirmations: Sender<Zeroizing<String>>,
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
        let (confirmations, receiver) = mpsc::channel();
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
            confirmations,
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
        // Never leave a detached thread sending authentication requests after the UI exits.
        let _ = self.join();
    }
}

fn checkpoint(cancelled: &AtomicBool) -> Result<()> {
    ensure!(
        !cancelled.load(Ordering::Relaxed),
        "Vorgang abgebrochen. Eine begonnene Anmeldung bleibt bis zur Prüfung im Webbanking lokal gesichert."
    );
    Ok(())
}

fn run(
    safety: Arc<Safety>,
    credentials: Credentials,
    output: PathBuf,
    method: TanMethod,
    updates: &Sender<Update>,
    confirmations: Receiver<Zeroizing<String>>,
    cancelled: &AtomicBool,
) -> Result<String> {
    credentials.validate()?;
    let username = Zeroizing::new(credentials.username.clone());
    let root = output.join(&account_key(&username)[..16]);
    let _ = updates.send(Update::Status("Lokales Archiv prüfen …".into()));
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
                match confirmations.recv_timeout(Duration::from_millis(100)) {
                    Ok(tan) => return Ok(tan),
                    Err(mpsc::RecvTimeoutError::Timeout) => (),
                    Err(mpsc::RecvTimeoutError::Disconnected) => bail!("TAN-Freigabe abgebrochen"),
                }
            }
        }, || checkpoint(cancelled))?;
        safety.clear(&username)?;
        let _ = updates.send(Update::Status("Dokumentenliste abrufen …".into()));
        let mut documents = api.documents(|| checkpoint(cancelled))?;
        documents.sort_by(|a, b| {
            b.date_creation
                .cmp(&a.date_creation)
                .then(a.document_id.cmp(&b.document_id))
        });
        let _ = updates.send(Update::Documents(
            documents
                .iter()
                .cloned()
                .map(|document| DocumentRow {
                    document,
                    status: "Ausstehend",
                })
                .collect(),
        ));
        let (mut downloaded, mut existing, mut skipped) = (0, 0, 0);
        for (index, doc) in documents.iter().enumerate() {
            checkpoint(cancelled)?;
            let status = if !doc.is_pdf() {
                skipped += 1;
                "Kein PDF"
            } else if archive.exists_verified(doc)? {
                existing += 1;
                "Vorhanden ✓"
            } else {
                archive.prepare_download(doc)?;
                let _ = updates.send(Update::Progress {
                    index,
                    status: "Lädt …",
                });
                let _ = updates.send(Update::Status(format!(
                    "PDF {}/{} archivieren …",
                    index + 1,
                    documents.len()
                )));
                let bytes = api.download(doc).with_context(|| {
                    format!(
                        "PDF-Abruf für Dokument {} vom {} fehlgeschlagen",
                        doc.document_id, doc.date_creation
                    )
                })?;
                archive.store(doc, &bytes)?;
                downloaded += 1;
                "Gespeichert ✓"
            };
            let _ = updates.send(Update::Progress { index, status });
        }
        Ok(format!(
            "Fertig: {downloaded} neu gespeichert, {existing} vorhanden und geprüft, {skipped} ohne PDF übersprungen."
        ))
    })();
    let _ = updates.send(Update::Status("Banksitzung beenden …".into()));
    if api.logout().is_err() {
        let _ = updates.send(Update::Status(
            "Abmeldung nicht bestätigt; Tokens werden lokal verworfen und nicht erneuert.".into(),
        ));
        return result.map(|summary| {
            format!(
                "{summary} Abmeldung nicht bestätigt; Tokens verfallen ohne weitere Erneuerung."
            )
        });
    }
    result
}
