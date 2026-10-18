# comdirect-postbox-rs

Eine deutsche Rust-TUI zum inkrementellen Archivieren der comdirect Postbox auf **Linux und Windows**. Die Anwendung lädt PDF-Dokumente unverändert herunter, sortiert sie nach Jahr und Dokumentart und prüft bereits archivierte Dokumente anhand ihrer Dokument-ID, Dateigröße und SHA-256-Prüfsumme.

## Starten

Zum Bauen benötigst du eine aktuelle stabile Rust-Toolchain mit Cargo, unter Windows gegebenenfalls die C++ Build Tools für das MSVC-Target. Fertige Programme werden von der GitHub-Actions-Pipeline für Linux und Windows als Artefakte bereitgestellt.

```bash
cargo build --release --locked
```

Linux:

```bash
./target/release/comdirect-postbox-rs --ziel "$HOME/Dokumente/comdirect"
```

Windows (PowerShell):

```powershell
.\target\release\comdirect-postbox-rs.exe --ziel "$HOME\Documents\comdirect"
```

Mit `cargo run --locked -- --demo` kannst du die Oberfläche ohne Bankverbindung, Zugangsdaten und Schreibzugriffe ansehen. `--help` zeigt die Optionen. Das Terminal sollte mindestens 64 × 22 Zeichen groß sein.

## Anmeldung und Bedienung

Du benötigst einen freigeschalteten comdirect-API-Zugang mit **Client-ID** und **Client-Secret**, deine achtstellige Zugangsnummer und deine PIN. API-Zugangsdaten sind separat vom Bankzugang. Alle vier Felder werden verdeckt eingegeben. Die App speichert keine Zugangsdaten oder Tokens in Dateien und führt keine Hintergrundanmeldungen aus.

1. Felder mit `Tab` wechseln; `Umschalt+Tab` wechselt zurück. `Strg+U` leert das aktuelle Feld.
2. Mit `F5` genau eine Anmeldung und anschließend die Archivierung starten.
3. Standardmäßig die Anmeldung in der **photoTAN-App** freigeben und **erst nach der Freigabe** in der TUI `Enter` drücken. Es wird nicht automatisch auf Freigabe geprüft.
4. Die TUI zeigt neue, vorhandene und nicht als PDF verfügbare Dokumente an. Mit `↑`/`↓` navigieren; `Esc` oder `Q` beendet den Vorgang kontrolliert. Laufende HTTP-Anfragen können bis zu 60 Sekunden und die anschließende Abmeldung nochmals bis zu 60 Sekunden dauern.

Für ein bereits bei comdirect freigeschaltetes mobileTAN-Verfahren gibt es `--tan sms`. Die sechsstellige TAN wird einmalig gesendet. Ein automatischer Wechsel des Verfahrens oder die Anzeige einer photoTAN-Scan-Grafik ist nicht implementiert.

**Der API-Download markiert ungelesene Dokumente bei comdirect als gelesen.** Die Anwendung löscht oder verschiebt keine Dokumente auf dem Bankserver. Nicht-PDF-Dokumente und HTML-Vorschaltseiten werden nicht heruntergeladen.

## Archiv und Originaldatum

Beispiel für die Ordnerstruktur:

```text
comdirect/
  <Kontokennung>/
    2026/
      Kontoauszuege/
        2026-09-30_Finanzreport September_<Dokument-ID>.pdf
        2026-09-30_Finanzreport September_<Dokument-ID>.pdf.postbox.json
      Wertpapiere/
      Steuern/
      Kreditkarte/
      Sonstiges/
```

Die Kontokennung besteht aus den ersten 16 Hexzeichen eines SHA-256-Hashes der Zugangsnummer und verhindert eine Vermischung mehrerer Zugänge. Sie ist keine Verschlüsselung. Die Kategorien werden aus dem Dokumenttitel abgeleitet; unbekannte Titel landen in `Sonstiges`.

Die von comdirect gelieferten PDF-Bytes bleiben unverändert. Für einen späteren Import kannst du die PDF-Dateien direkt verwenden. Die zugehörigen JSON-Dateien enthalten die ursprünglichen API-Metadaten einschließlich `dateCreation`, Prüfsumme, Größe und Archivierungszeitpunkt.

Die Spezifikation liefert `dateCreation` als **Kalenderdatum ohne Uhrzeit oder Zeitzone**. Dieses Datum wird im Dateinamen und als Dateiänderungsdatum mit **00:00 UTC** gespeichert. Das ist eine festgelegte Abbildung; die ursprüngliche Erstellungsuhrzeit lässt sich aus der API nicht rekonstruieren. Manche Programme zeigen den Zeitpunkt in lokaler Zeit an. Ein portables Setzen des Dateisystem-Erstellungsdatums wird nicht zugesagt; bei Importen das Änderungsdatum, den Dateinamen oder die Metadaten verwenden. Interne PDF-Metadaten werden nicht verändert.

Vorhandene PDFs werden auf Dateisignatur, Endemarkierung, Größe und SHA-256 geprüft. Eine fehlende PDF wird erneut heruntergeladen; eine abweichende vorhandene Datei führt zum Abbruch und wird nicht überschrieben. Eine vollständige PDF ohne Metadaten an ihrem erwarteten Pfad wird nach Prüfung wieder ins Archiv aufgenommen, etwa nach einem Abbruch zwischen PDF- und Metadatenspeicherung. Ohne frühere Prüfsumme lässt sich dabei nur die vorhandene Datei als Ausgangsstand erfassen, nicht ihre Übereinstimmung mit dem Bankoriginal beweisen.

Verschiebst du Dateien innerhalb des Kontoarchivs, immer **PDF und zugehörige `.postbox.json` gemeinsam** umbenennen oder verschieben. Dann bleibt die Dokument-ID erkennbar, selbst wenn sich der Titel bei comdirect ändert. Gelöschte Metadaten oder getrennt verschobene PDFs können die Erkennung verhindern. Für Sicherungen das vollständige Archiv einschließlich JSON-Dateien kopieren. PDFs sind auf 64 MiB begrenzt; symbolische Links innerhalb des Archivs werden abgelehnt.

## Schutz vor Kontosperren

Die mitgelieferte API-Spezifikation nennt eine Sperre nach drei falschen TAN-Eingaben bzw. fünf angeforderten TAN-Challenges ohne erfolgreiche TAN-Entwertung. Deshalb gelten folgende Regeln:

- Pro Programmstart höchstens ein Passwortversuch, eine TAN-Anforderung und eine TAN-Aktivierung. HTTP-Wiederholungen und Weiterleitungen sind deaktiviert.
- Bei Netzwerk-, API-, PIN-, TAN- oder Refresh-Fehlern bricht die App ab. Sie versucht keine automatische Neuanmeldung. Dokumentanfragen laufen nacheinander mit mindestens 350 ms Abstand.
- **Vor der ersten Anmeldeanfrage** wird eine dauerhafte lokale Anmeldesicherung geschrieben. Erst ein vollständig erfolgreicher Anmeldeablauf hebt sie auf. Sie bleibt bei Fehlern, Abbruch und Absturz bestehen, auch wenn du einen anderen Archivordner wählst.
- Solange die Sicherung aktiv ist, sendet ein neuer Anmeldeversuch keine Bankanfrage. Erst selbst im **comdirect-Webbanking erfolgreich mit TAN anmelden**. Danach `comdirect-postbox-rs --entsperren` starten, Zugangsnummer und `WEBLOGIN BESTÄTIGT` eingeben und `F5` drücken. Die App prüft diesen Weblogin nicht selbst.
- Eine lokale Prozesssperre verhindert parallele Instanzen. Tokens werden bei kontrolliertem Ende widerrufen; schlägt das fehl, werden sie lokal verworfen und nicht weiter erneuert. Bei einem Absturz gibt es keine weitere Token-Erneuerung.

Eine absolute Garantie gegen eine bankseitige Kontosperre ist nicht möglich: Die App kennt insbesondere Fehlversuche anderer Programme oder Geräte nicht. Die lokale Sicherung nach einem Fehler nicht wiederholt ohne erfolgreichen Weblogin aufheben. Eine Kontoentsperrung bei comdirect selbst kann die App nicht durchführen.

Die Sicherung liegt im betriebssystemspezifischen lokalen Datenverzeichnis, unabhängig vom Archiv:

| System | Standardpfad |
| --- | --- |
| Linux | `~/.local/share/comdirect-postbox-rs/` (bzw. unter `XDG_DATA_HOME`) |
| Windows | `%LOCALAPPDATA%\comdirect-postbox-rs\comdirect-postbox-rs\data\` |

Sicherungsdateien enthalten einen Hash der Zugangsnummer, keine PIN oder Tokens. Ein geändertes Benutzerprofil oder Datenverzeichnis teilt diese Sicherung nicht. Das Archiv enthält vertrauliche Bankdokumente: einen privaten Ordner verwenden und passende Zugriffsrechte beziehungsweise Datenträgerverschlüsselung einrichten. Neue Verzeichnisse werden unter Unix mit Modus `0700`, neue PDF- und Metadatendateien mit `0600` angelegt; Windows verwendet die Zugriffsrechte des Zielordners.

## Entwicklung und Prüfungen

Code und Bezeichner sind Englisch, Dokumentation und Oberfläche Deutsch. Die Architektur trennt API (`src/api.rs`), Archiv (`src/archive.rs`), Anmeldesicherung (`src/safety.rs`), Hintergrundablauf (`src/sync.rs`) und TUI (`src/tui.rs`).

```bash
cargo fmt --all --check
cargo clippy --locked --all-targets -- -D warnings
cargo test --locked
```

Tests verwenden ausschließlich temporäre Verzeichnisse und einen lokalen HTTP-Testserver mit simulierten Bankantworten. Sie prüfen unter anderem den vollständigen Authentifizierungsablauf, TAN-Fehler ohne Wiederholung, Token-Refresh-Fehler, Paginierung, inkrementelle Downloads, Prüfsummen, Datumserhalt und maskierte TUI-Felder. Die CI führt Prüfungen und Release-Builds unter Linux und Windows aus. Ein echter Kontoabruf erfordert deine Zugangsdaten und wurde nicht automatisiert getestet.

Die lokale Datei `comdirect_REST_API_Dokumentation.pdf` ist die fachliche Grundlage (Kapitel 1–3 und 9) und wird über `.gitignore` ausgeschlossen. Sie ist kein Bestandteil des Repositories. Die Seitenparameter `paging-first` und `paging-count` folgen der comdirect-Paging-Konvention; die App prüft den zurückgegebenen Index und bricht bei widersprüchlichen oder wiederholten Seiten ab. Die bereitgestellte PDF führt diese Query-Parameter für Dokumente nicht ausdrücklich auf; dieses Verhalten muss bei einem echten Abruf bestätigt werden. Die allgemeine Paging-Konvention erläutert auch ein [Beitrag der comdirect-Fachabteilung](https://community.comdirect.de/t5/website-apps/rest-api-filter-parameter/td-p/108710/page/3).

Die Deaktivierung automatischer HTTP-Wiederholungen verwendet die ausdrücklich dafür vorgesehene [reqwest-Retry-Policy](https://docs.rs/reqwest/0.12.28/reqwest/retry/index.html).

Lizenz: GPL-3.0-or-later; siehe [LICENSE](LICENSE).
