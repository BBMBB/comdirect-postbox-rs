mod api;
mod archive;
mod model;
mod safety;
mod sync;
mod tui;

use std::{
    io::{self, IsTerminal},
    path::PathBuf,
    sync::Arc,
};

use anyhow::{Context, Result, ensure};
use clap::Parser;
use directories::ProjectDirs;

use api::TanMethod;

#[derive(Parser)]
#[command(
    version,
    disable_help_flag = true,
    disable_version_flag = true,
    about = "comdirect Postbox inkrementell als PDF archivieren",
    long_about = "Deutsche Terminal-Oberfläche zur comdirect Postbox.\nZugangsdaten werden interaktiv abgefragt und nicht gespeichert.\nDownloads markieren Dokumente bei comdirect als gelesen.",
    help_template = "{about-with-newline}\nVerwendung: comdirect-postbox-rs [OPTIONEN]\n\nOptionen:\n{options}"
)]
struct Args {
    /// Zielordner für das lokale Archiv (Standard: postbox)
    #[arg(
        long = "ziel",
        default_value = "postbox",
        value_name = "ORDNER",
        hide_default_value = true
    )]
    output: PathBuf,
    /// TAN-Verfahren: push (Standard, photoTAN-App) oder sms (mobileTAN, falls freigeschaltet)
    #[arg(
        long,
        value_enum,
        default_value = "push",
        value_name = "VERFAHREN",
        hide_default_value = true,
        hide_possible_values = true
    )]
    tan: TanMethod,
    /// Lokale Anmeldesicherung nach erfolgreichem Webbanking-Login mit TAN aufheben
    #[arg(long = "entsperren", conflicts_with = "demo")]
    unlock: bool,
    /// Oberfläche mit Beispieldaten ohne Bankverbindung oder Schreibzugriffe anzeigen
    #[arg(long)]
    demo: bool,
    /// Hilfe anzeigen
    #[arg(short = 'h', long, action = clap::ArgAction::Help)]
    help: Option<bool>,
    /// Version anzeigen
    #[arg(short = 'V', long, action = clap::ArgAction::Version)]
    version: Option<bool>,
}

fn main() -> std::process::ExitCode {
    match run() {
        Ok(()) => std::process::ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("Fehler: {error:#}");
            std::process::ExitCode::FAILURE
        }
    }
}

fn run() -> Result<()> {
    let args = Args::parse();
    ensure!(
        io::stdin().is_terminal() && io::stdout().is_terminal(),
        "Bitte in einem interaktiven Terminal starten (Hilfe: --help)"
    );
    let state = if args.demo {
        None
    } else {
        let directories = ProjectDirs::from("de", "comdirect-postbox-rs", "comdirect-postbox-rs")
            .context("Lokales Anwendungsverzeichnis nicht verfügbar")?;
        Some(Arc::new(safety::Safety::open(
            directories.data_local_dir(),
        )?))
    };
    tui::run(args.output, args.tan, args.unlock, args.demo, state)
}
