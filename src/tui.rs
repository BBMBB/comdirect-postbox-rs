use std::{
    path::PathBuf,
    sync::{Arc, atomic::Ordering},
    time::Duration,
};

use anyhow::{Context, Result, bail};
use crossterm::event::{self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use ratatui::{
    DefaultTerminal, Frame,
    layout::{Constraint, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Cell, Gauge, Paragraph, Row, Table, TableState, Wrap},
};
use zeroize::{Zeroize, Zeroizing};

use crate::{
    api::{Credentials, TanMethod},
    config::ConfigStore,
    safety::Safety,
    sync::{Command, DocumentRow, Inventory, Update, Worker},
};

const ACCENT: Color = Color::Rgb(235, 211, 79);
const MUTED: Color = Color::Rgb(153, 166, 181);

enum Screen {
    Login,
    Busy,
    Overview { inventory: Inventory },
    Challenge { sms: bool },
    Finished,
    Unlock,
}

struct App {
    screen: Screen,
    inputs: Vec<Zeroizing<String>>,
    focus: usize,
    tan: Zeroizing<String>,
    status: String,
    failed: bool,
    rows: Vec<DocumentRow>,
    table: TableState,
    output: PathBuf,
    method: TanMethod,
    quitting: bool,
    demo: bool,
    config: Option<ConfigStore>,
}

impl App {
    fn new(output: PathBuf, method: TanMethod, unlock: bool, demo: bool) -> Self {
        let mut app = Self {
            screen: if unlock {
                Screen::Unlock
            } else {
                Screen::Login
            },
            inputs: (0..4).map(|_| Zeroizing::new(String::new())).collect(),
            focus: 0,
            tan: Zeroizing::new(String::new()),
            status: "Bereit. Noch keine Verbindung zur Bank.".into(),
            failed: false,
            rows: Vec::new(),
            table: TableState::default(),
            output,
            method,
            quitting: false,
            demo,
            config: None,
        };
        if demo {
            for (id, name, date, status) in [
                (
                    "demo-1",
                    "Finanzreport September 2026",
                    "2026-09-30",
                    "Ausstehend",
                ),
                (
                    "demo-2",
                    "Wertpapierabrechnung Kauf",
                    "2026-09-23",
                    "Vorhanden ✓",
                ),
                (
                    "demo-3",
                    "Jahressteuerbescheinigung",
                    "2026-03-12",
                    "Ausstehend",
                ),
            ] {
                app.rows.push(DocumentRow {
                    document: crate::model::Document {
                        document_id: id.into(),
                        name: name.into(),
                        date_creation: date.into(),
                        mime_type: "application/pdf".into(),
                        extra: Default::default(),
                    },
                    status,
                });
            }
            let rows = std::mem::take(&mut app.rows);
            app.update(Update::Overview {
                rows,
                inventory: Inventory {
                    online: 3,
                    existing: 1,
                    pending: 2,
                    skipped: 0,
                },
            });
        }
        app
    }

    fn configure(&mut self, store: ConfigStore) -> Result<()> {
        store.archive_index_path()?;
        let mut saved = store.load()?;
        if matches!(self.screen, Screen::Unlock) {
            *self.inputs[0] = std::mem::take(&mut saved.username);
            self.focus = usize::from(!self.inputs[0].is_empty());
        } else {
            *self.inputs[0] = std::mem::take(&mut saved.client_id);
            *self.inputs[1] = std::mem::take(&mut saved.client_secret);
            *self.inputs[2] = std::mem::take(&mut saved.username);
            self.focus = self.inputs[..3]
                .iter()
                .position(|input| input.is_empty())
                .unwrap_or(3);
            self.status = "F5 speichert API-Zugang und Zugangsnummer lokal. PIN und TAN werden nicht gespeichert. Noch keine Bankverbindung.".into();
        }
        self.config = Some(store);
        Ok(())
    }

    fn draw(&mut self, frame: &mut Frame) {
        let area = frame.area();
        if area.width < 64 || area.height < 22 {
            frame.render_widget(
                Paragraph::new(
                    "Bitte das Terminal auf mindestens 64 × 22 Zeichen vergrößern.\nEsc: Beenden",
                )
                .wrap(Wrap { trim: false })
                .block(Block::bordered().title(" comdirect Postbox ")),
                area,
            );
            return;
        }
        let compact = area.height < 27;
        let layout = Layout::vertical([
            Constraint::Length(2),
            Constraint::Min(11),
            Constraint::Length(if compact { 4 } else { 5 }),
            Constraint::Length(if compact { 1 } else { 2 }),
        ])
        .margin(1)
        .split(area);
        frame.render_widget(
            Paragraph::new(Line::from(vec![
                Span::styled(
                    "comdirect",
                    Style::default().fg(ACCENT).add_modifier(Modifier::BOLD),
                ),
                Span::raw("  /  POSTBOX ARCHIV"),
            ])),
            layout[0],
        );
        match self.screen {
            Screen::Login | Screen::Unlock => self.draw_form(frame, layout[1]),
            Screen::Overview { inventory } => self.draw_overview(frame, layout[1], inventory),
            Screen::Challenge { sms } => {
                let text = if sms {
                    "Die Bank hat eine mobileTAN gesendet.\n\nSechsstellige TAN eingeben und mit Enter einmalig senden.\n\n"
                } else {
                    "Bitte die Anmeldung in der comdirect photoTAN-App freigeben.\n\nErst NACH erfolgreicher Freigabe hier Enter drücken.\nEs gibt keine automatische Abfrage und keine neue TAN-Anforderung.\n\n"
                };
                frame.render_widget(
                    Paragraph::new(format!("{text}{}", "*".repeat(self.tan.chars().count())))
                        .wrap(Wrap { trim: false })
                        .block(
                            Block::bordered()
                                .title(" Zweiter Faktor ")
                                .border_style(Style::default().fg(ACCENT)),
                        ),
                    layout[1],
                );
            }
            Screen::Finished if self.failed => frame.render_widget(
                Paragraph::new(self.status.as_str())
                    .wrap(Wrap { trim: false })
                    .block(
                        Block::bordered()
                            .title(" Vorgang abgebrochen ")
                            .border_style(Style::default().fg(Color::LightRed)),
                    ),
                layout[1],
            ),
            _ => self.draw_documents(frame, layout[1]),
        }
        let status_color = if self.failed { Color::LightRed } else { MUTED };
        frame.render_widget(
            Paragraph::new(if self.failed && matches!(self.screen, Screen::Finished) {
                "Keine automatische Wiederholung. Hinweise oben vor einem neuen Versuch beachten."
            } else {
                self.status.as_str()
            })
            .style(Style::default().fg(status_color))
            .wrap(Wrap { trim: false })
            .block(Block::bordered().title(" Status ")),
            layout[2],
        );
        let keys = match self.screen {
            Screen::Login => "Tab: Feld wechseln · F5: Anmelden & prüfen · Esc: Beenden",
            Screen::Unlock => {
                "Tab: Feld wechseln  ·  F5: Lokale Sicherung aufheben  ·  Esc: Beenden"
            }
            Screen::Challenge { .. } => "Enter: Einmalig bestätigen  ·  Esc: Abbrechen",
            Screen::Overview { inventory } if inventory.pending == 0 => {
                "F5: Abschließen · ↑/↓: Dokument wählen · Esc / Q: Beenden"
            }
            Screen::Overview { .. } => "F5: Downloads starten · ↑/↓: Auswahl · Esc / Q: Beenden",
            Screen::Busy => "↑/↓: Dokument wählen  ·  Esc / Q: Sicher abbrechen",
            Screen::Finished => "↑/↓: Dokument wählen  ·  Esc / Q: Beenden",
        };
        frame.render_widget(
            Paragraph::new(keys).style(Style::default().fg(MUTED)),
            layout[3],
        );
    }

    fn draw_form(&self, frame: &mut Frame, area: Rect) {
        let unlock = matches!(self.screen, Screen::Unlock);
        let block = Block::bordered().title(if unlock {
            " Lokale Anmeldesicherung "
        } else {
            " Zugangsdaten · PIN nur im Arbeitsspeicher "
        });
        let inner = block.inner(area);
        frame.render_widget(block, area);
        let compact = area.height < 16;
        let chunks = Layout::vertical([
            Constraint::Length(if unlock { 3 } else { 2 }),
            Constraint::Min(4),
            Constraint::Length(1),
        ])
        .margin(1)
        .split(inner);
        let intro = if unlock {
            "Nach erfolgreichem Weblogin mit TAN entsperren.\nZugangsnummer + WEBLOGIN BESTÄTIGT, dann F5.\nNur lokale Sicherung; keine Bankanfrage."
        } else if compact {
            "Mit F5 anmelden und den Bestand prüfen.\nDownloads erst nach weiterer Bestätigung."
        } else {
            "Mit F5 anmelden und den lokalen Bestand mit der Postbox vergleichen.\nDie Übersicht wird vor dem Download zur Bestätigung angezeigt."
        };
        frame.render_widget(
            Paragraph::new(intro)
                .wrap(Wrap { trim: false })
                .style(Style::default().fg(MUTED)),
            chunks[0],
        );
        let labels: &[&str] = if unlock {
            &["Zugangsnummer", "Bestätigung"]
        } else {
            &["API-Client-ID", "API-Client-Secret", "Zugangsnummer", "PIN"]
        };
        let spacing = if chunks[1].height >= labels.len() as u16 * 2 {
            2
        } else {
            1
        };
        let inputs =
            Layout::vertical(labels.iter().map(|_| Constraint::Length(spacing))).split(chunks[1]);
        for (index, label) in labels.iter().enumerate() {
            let selected = self.focus == index;
            let input = &self.inputs[index];
            let visible = if unlock && index == 1 {
                input.to_string()
            } else {
                "*".repeat(input.chars().count().min(48))
            };
            frame.render_widget(
                Paragraph::new(Line::from(vec![
                    Span::styled(
                        format!("{} {:<18}", if selected { "›" } else { " " }, label),
                        Style::default().fg(if selected { ACCENT } else { MUTED }),
                    ),
                    Span::raw(format!("{visible}{}", if selected { "▏" } else { "" })),
                ])),
                inputs[index],
            );
        }
        frame.render_widget(
            Paragraph::new(format!(
                "Ziel: {} · TAN: {}",
                self.output.display(),
                match self.method {
                    TanMethod::Push => "photoTAN-Push",
                    TanMethod::Sms => "mobileTAN",
                }
            ))
            .style(Style::default().fg(MUTED)),
            chunks[2],
        );
    }

    fn draw_overview(&mut self, frame: &mut Frame, area: Rect, inventory: Inventory) {
        let layout = Layout::vertical([Constraint::Length(7), Constraint::Min(4)]).split(area);
        let title = if self.demo {
            " Übersicht · OFFLINE-DEMO "
        } else {
            " Übersicht vor dem Download "
        };
        frame.render_widget(
            Paragraph::new(vec![
                Line::from(format!(
                    "Online verfügbar:             {} Dokumente",
                    inventory.online
                )),
                Line::from(format!(
                    "Davon offline vorhanden:      {} PDFs",
                    inventory.existing
                )),
                Line::styled(
                    format!("Herunterzuladen:               {} PDFs", inventory.pending),
                    Style::default().fg(ACCENT).add_modifier(Modifier::BOLD),
                ),
                Line::from(format!(
                    "Andere Formate / übersprungen: {}",
                    inventory.skipped
                )),
            ])
            .block(Block::bordered().title(title)),
            layout[0],
        );
        self.draw_table(frame, layout[1]);
    }

    fn approve_downloads(&mut self, key: KeyEvent) -> bool {
        if key.code != KeyCode::F(5) || !matches!(self.screen, Screen::Overview { .. }) {
            return false;
        }
        if self.demo {
            for row in &mut self.rows {
                if row.status == "Ausstehend" {
                    row.status = "Gespeichert ✓";
                }
            }
            self.screen = Screen::Finished;
            self.status = "Offline-Demo · 2 neu gespeichert, 1 vorhanden und geprüft · keine Bankverbindung, keine Dateien geschrieben".into();
        } else {
            self.screen = Screen::Busy;
            self.status = "Downloads bestätigt. Fehlende PDFs werden archiviert …".into();
        }
        true
    }

    fn draw_documents(&mut self, frame: &mut Frame, area: Rect) {
        let layout = Layout::vertical([Constraint::Length(3), Constraint::Min(3)]).split(area);
        let done = self
            .rows
            .iter()
            .filter(|row| row.status != "Ausstehend" && row.status != "Lädt …")
            .count();
        let ratio = if self.rows.is_empty() {
            0.0
        } else {
            done as f64 / self.rows.len() as f64
        };
        frame.render_widget(
            Gauge::default()
                .block(Block::bordered().title(if self.demo {
                    " OFFLINE-DEMO "
                } else {
                    " Archivierung "
                }))
                .gauge_style(Style::default().fg(ACCENT))
                .ratio(ratio)
                .label(format!("{done} / {} Dokumente", self.rows.len())),
            layout[0],
        );
        self.draw_table(frame, layout[1]);
    }

    fn draw_table(&mut self, frame: &mut Frame, area: Rect) {
        let rows = self.rows.iter().map(|row| {
            Row::new(vec![
                Cell::from(row.document.date_creation.clone()),
                Cell::from(row.document.name.clone()),
                Cell::from(row.document.category()),
                Cell::from(row.status),
            ])
        });
        let table = Table::new(
            rows,
            [
                Constraint::Length(10),
                Constraint::Min(14),
                Constraint::Length(14),
                Constraint::Length(15),
            ],
        )
        .header(
            Row::new(["Datum", "Dokument", "Ordner", "Ergebnis"])
                .style(Style::default().fg(ACCENT))
                .bottom_margin(1),
        )
        .block(Block::bordered().title(" Postbox "))
        .row_highlight_style(Style::default().bg(Color::Rgb(38, 47, 58)))
        .highlight_symbol("› ");
        frame.render_stateful_widget(table, area, &mut self.table);
    }

    fn update(&mut self, update: Update) {
        match update {
            Update::Status(message) => self.status = message,
            Update::Challenge { sms } => {
                self.screen = Screen::Challenge { sms };
                self.status =
                    "Genau eine TAN-Challenge angefordert. Bitte sorgfältig freigeben.".into();
            }
            Update::Overview { rows, inventory } => {
                self.screen = Screen::Overview { inventory };
                self.status = if inventory.pending == 0 {
                    "Keine Downloads nötig. F5 schließt ab; Esc beendet die Sitzung.".into()
                } else {
                    format!(
                        "F5 startet {} Downloads. Die abgerufenen Dokumente werden bei comdirect als gelesen markiert.",
                        inventory.pending
                    )
                };
                self.rows = rows;
                if !self.rows.is_empty() {
                    self.table.select(Some(0));
                }
            }
            Update::Progress { index, status } => {
                if let Some(row) = self.rows.get_mut(index) {
                    row.status = status;
                }
                self.table.select(Some(index));
            }
            Update::Finished { message, failed } => {
                self.screen = Screen::Finished;
                self.status = message;
                self.failed = failed;
            }
        }
    }

    fn edit(input: &mut Zeroizing<String>, key: KeyEvent) {
        match key.code {
            KeyCode::Backspace => {
                // Truncate only after overwriting the removed character's bytes.
                if let Some((last, _)) = input.char_indices().last() {
                    let tail = input[last..].len();
                    input.replace_range(last.., &"\0".repeat(tail));
                    input.truncate(last);
                }
            }
            KeyCode::Char('u') if key.modifiers.contains(KeyModifiers::CONTROL) => input.zeroize(),
            KeyCode::Char(c)
                if !key
                    .modifiers
                    .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT)
                    && !c.is_control()
                    && input.len() < 512 =>
            {
                input.push(c)
            }
            _ => (),
        }
    }
}

struct RestoreTerminal;
impl Drop for RestoreTerminal {
    fn drop(&mut self) {
        ratatui::restore();
    }
}

pub fn run(
    output: PathBuf,
    method: TanMethod,
    unlock: bool,
    demo: bool,
    safety: Option<Arc<Safety>>,
    config: Option<ConfigStore>,
) -> Result<()> {
    let mut app = App::new(output, method, unlock, demo);
    if let Some(store) = config {
        app.configure(store)?;
    }
    let mut terminal = ratatui::try_init().context("Terminal konnte nicht gestartet werden")?;
    let _restore = RestoreTerminal;
    event_loop(&mut terminal, &mut app, safety)?;
    if app.failed && matches!(app.screen, Screen::Finished) {
        bail!(app.status);
    }
    Ok(())
}

fn event_loop(
    terminal: &mut DefaultTerminal,
    app: &mut App,
    safety: Option<Arc<Safety>>,
) -> Result<()> {
    let mut worker: Option<Worker> = None;
    loop {
        if let Some(active) = &mut worker {
            while let Ok(update) = active.updates.try_recv() {
                app.update(update);
            }
            if active.is_finished() {
                let result = active.join();
                while let Ok(update) = active.updates.try_recv() {
                    app.update(update);
                }
                if let Err(error) = result {
                    app.update(Update::Finished {
                        message: error.to_string(),
                        failed: true,
                    });
                }
                worker = None;
                if app.quitting {
                    return Ok(());
                }
            }
        }
        terminal.draw(|frame| app.draw(frame))?;
        if !event::poll(Duration::from_millis(100))? {
            continue;
        }
        let Event::Key(key) = event::read()? else {
            continue;
        };
        if key.kind == KeyEventKind::Release {
            continue;
        }
        let quit = key.code == KeyCode::Esc
            || (key.code == KeyCode::Char('c') && key.modifiers.contains(KeyModifiers::CONTROL))
            || (matches!(
                app.screen,
                Screen::Busy | Screen::Finished | Screen::Overview { .. }
            ) && matches!(key.code, KeyCode::Char('q' | 'Q')));
        if quit {
            if let Some(active) = &worker {
                active.cancelled.store(true, Ordering::Relaxed);
                app.quitting = true;
                app.status =
                    "Abbruch angefordert. Laufende Anfrage abschließen und Banksitzung beenden …"
                        .into();
            } else {
                return Ok(());
            }
            continue;
        }
        if app.quitting {
            continue;
        }
        match app.screen {
            Screen::Login | Screen::Unlock => {
                let count = if matches!(app.screen, Screen::Unlock) {
                    2
                } else {
                    4
                };
                match key.code {
                    KeyCode::Tab | KeyCode::Enter => app.focus = (app.focus + 1) % count,
                    KeyCode::BackTab => app.focus = (app.focus + count - 1) % count,
                    KeyCode::F(5) if matches!(app.screen, Screen::Unlock) => {
                        let username = &app.inputs[0];
                        if username.len() != 8
                            || !username.bytes().all(|c| c.is_ascii_digit())
                            || app.inputs[1].as_str() != "WEBLOGIN BESTÄTIGT"
                        {
                            app.failed = true;
                            app.status =
                                "Achtstellige Zugangsnummer und exakt WEBLOGIN BESTÄTIGT eingeben."
                                    .into();
                        } else {
                            let state = safety.as_ref().context("Lokaler Zustand fehlt")?;
                            state.clear(username)?;
                            app.inputs.iter_mut().for_each(|input| input.zeroize());
                            app.screen = Screen::Finished;
                            app.failed = false;
                            app.status = "Lokale Anmeldesicherung aufgehoben. Die App für eine neue Anmeldung erneut starten.".into();
                        }
                    }
                    KeyCode::F(5) => {
                        let credentials = Credentials {
                            client_id: app.inputs[0].to_string(),
                            client_secret: app.inputs[1].to_string(),
                            username: app.inputs[2].to_string(),
                            pin: app.inputs[3].to_string(),
                        };
                        if let Err(error) = credentials.validate() {
                            app.failed = true;
                            app.status = error.to_string();
                            continue;
                        }
                        let state = safety.as_ref().context("Lokaler Zustand fehlt")?.clone();
                        let index_path = app
                            .config
                            .as_ref()
                            .context("Konfiguration fehlt")?
                            .archive_index_path()?;
                        if let Some(store) = &app.config
                            && let Err(error) = store.save(&credentials)
                        {
                            app.failed = true;
                            app.status = format!(
                                "Konfiguration konnte nicht gespeichert werden: {error:#}. Keine Bankanfrage gesendet."
                            );
                            continue;
                        }
                        app.inputs.iter_mut().for_each(|input| input.zeroize());
                        worker = Some(Worker::start(
                            state,
                            credentials,
                            app.output.clone(),
                            index_path,
                            app.method,
                        ));
                        app.failed = false;
                        app.screen = Screen::Busy;
                    }
                    _ => App::edit(&mut app.inputs[app.focus], key),
                }
            }
            Screen::Challenge { sms } => {
                if key.code == KeyCode::Enter {
                    if sms && (app.tan.len() != 6 || !app.tan.bytes().all(|b| b.is_ascii_digit())) {
                        app.status =
                            "Bitte die sechsstellige TAN eingeben. Noch keine TAN gesendet.".into();
                        continue;
                    }
                    if let Some(active) = &worker {
                        let tan = Zeroizing::new(std::mem::take(&mut *app.tan));
                        let _ = active.commands.send(Command::Tan(tan));
                        app.screen = Screen::Busy;
                        app.status = "Einmalige TAN-Freigabe prüfen …".into();
                    }
                } else if sms {
                    App::edit(&mut app.tan, key);
                }
            }
            _ => {
                if app.approve_downloads(key) {
                    if let Some(active) = &worker {
                        active
                            .commands
                            .send(Command::StartDownloads)
                            .context("Downloadbestätigung konnte nicht übermittelt werden")?;
                    }
                    continue;
                }
                if !app.rows.is_empty() {
                    let selected = app.table.selected().unwrap_or(0);
                    match key.code {
                        KeyCode::Down => app
                            .table
                            .select(Some((selected + 1).min(app.rows.len() - 1))),
                        KeyCode::Up => app.table.select(Some(selected.saturating_sub(1))),
                        _ => (),
                    }
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::{Terminal, backend::TestBackend};

    #[test]
    fn overview_shows_inventory_and_requires_f5_instead_of_enter() {
        for (width, height) in [(64, 22), (100, 30)] {
            let mut app = App::new("postbox".into(), TanMethod::Push, false, true);
            let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
            terminal.draw(|frame| app.draw(frame)).unwrap();
            let text: String = terminal
                .backend()
                .buffer()
                .content
                .iter()
                .map(|cell| cell.symbol())
                .collect();
            for label in [
                "Online verfügbar:",
                "Davon offline vorhanden:",
                "Herunterzuladen:",
                "F5: Downloads starten",
            ] {
                assert!(text.contains(label));
            }
            assert!(!app.approve_downloads(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)));
            assert!(matches!(
                app.screen,
                Screen::Overview {
                    inventory: Inventory {
                        online: 3,
                        existing: 1,
                        pending: 2,
                        skipped: 0
                    }
                }
            ));
            assert!(app.approve_downloads(KeyEvent::new(KeyCode::F(5), KeyModifiers::NONE)));
            assert!(matches!(app.screen, Screen::Finished));
            assert!(!app.approve_downloads(KeyEvent::new(KeyCode::F(5), KeyModifiers::NONE)));
        }
    }

    #[test]
    fn saved_credentials_prefill_the_form_but_pin_is_always_empty() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.json");
        let credentials = Credentials {
            client_id: "SAVED_CLIENT_ID".into(),
            client_secret: "SAVED_CLIENT_SECRET".into(),
            username: "12345678".into(),
            pin: "NEVER_LOAD_PIN".into(),
        };
        ConfigStore::new(path.clone()).save(&credentials).unwrap();
        let mut app = App::new("postbox".into(), TanMethod::Push, false, false);
        app.configure(ConfigStore::new(path.clone())).unwrap();
        assert_eq!(*app.inputs[0], credentials.client_id);
        assert_eq!(*app.inputs[1], credentials.client_secret);
        assert_eq!(*app.inputs[2], credentials.username);
        assert!(app.inputs[3].is_empty());
        assert_eq!(app.focus, 3);
        let mut terminal = Terminal::new(TestBackend::new(100, 30)).unwrap();
        terminal.draw(|frame| app.draw(frame)).unwrap();
        let text: String = terminal
            .backend()
            .buffer()
            .content
            .iter()
            .map(|cell| cell.symbol())
            .collect();
        assert!(!text.contains("SAVED_CLIENT"));
        assert!(!text.contains("12345678"));
        let mut unlock = App::new("postbox".into(), TanMethod::Push, true, false);
        unlock.configure(ConfigStore::new(path)).unwrap();
        assert_eq!(*unlock.inputs[0], credentials.username);
        assert!(unlock.inputs[1].is_empty());
        assert_eq!(unlock.focus, 1);
    }

    #[test]
    fn credentials_are_masked_and_german_screens_render() {
        let mut app = App::new("postbox".into(), TanMethod::Push, false, false);
        app.inputs[1].push_str("DO_NOT_DISPLAY_SECRET");
        let mut terminal = Terminal::new(TestBackend::new(100, 30)).unwrap();
        terminal.draw(|frame| app.draw(frame)).unwrap();
        let text: String = terminal
            .backend()
            .buffer()
            .content
            .iter()
            .map(|cell| cell.symbol())
            .collect();
        assert!(text.contains("Zugangsdaten"));
        assert!(!text.contains("DO_NOT_DISPLAY_SECRET"));
        assert!(text.contains("*********************"));
        for (width, height) in [(40, 10), (64, 22), (100, 30)] {
            let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
            app.screen = Screen::Challenge { sms: false };
            terminal.draw(|frame| app.draw(frame)).unwrap();
            app.screen = Screen::Login;
            terminal.draw(|frame| app.draw(frame)).unwrap();
            if width >= 64 {
                let text: String = terminal
                    .backend()
                    .buffer()
                    .content
                    .iter()
                    .map(|cell| cell.symbol())
                    .collect();
                assert!(text.contains("PIN"));
                assert!(text.contains("F5"));
            }
        }
    }
}
