# comdirect-postbox-rs

> **Disclaimer:** Dieses Projekt ist mittels Vibe Coding mit KI-Unterstützung entstanden. Es kann Fehler enthalten; die Nutzung erfolgt auf eigene Verantwortung.

Die Idee zu diesem Projekt stammt von [senshi-x/ComdirectPostboxDownloader](https://github.com/senshi-x/ComdirectPostboxDownloader).

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

Mit `cargo run --locked -- --demo` kannst du die Übersicht ohne Bankverbindung, Zugangsdaten und Schreibzugriffe ansehen. `F5` zeigt darin einen simulierten Abschluss. `--help` zeigt die Optionen. Das Terminal sollte mindestens 64 × 22 Zeichen groß sein.

## Anmeldung und Bedienung

Du benötigst einen freigeschalteten comdirect-API-Zugang mit **Client-ID** und **Client-Secret**, deine achtstellige Zugangsnummer und deine PIN. API-Zugangsdaten sind separat vom Bankzugang. Alle vier Felder werden verdeckt eingegeben. Client-ID, Client-Secret und Zugangsnummer werden in einer lokalen Konfigurationsdatei gespeichert. **PIN, TAN und Tokens werden nicht gespeichert.**

1. Felder mit `Tab` wechseln; `Umschalt+Tab` wechselt zurück. `Strg+U` leert das aktuelle Feld.
2. Mit `F5` die drei Konfigurationswerte speichern, genau eine Anmeldung starten und den Bestand prüfen. Bei weiteren Programmstarts sind diese Felder bereits ausgefüllt und der Fokus liegt direkt auf der PIN. Änderungen an den drei Werten werden beim nächsten `F5` übernommen.
3. Standardmäßig die Anmeldung in der **photoTAN-App** freigeben und **erst nach der Freigabe** in der TUI `Enter` drücken. Es wird nicht automatisch auf Freigabe geprüft.
4. Nach der Anmeldung folgt die **Bestandsübersicht**: online verfügbare Dokumente, davon bereits lokal vorhandene und geprüfte PDFs sowie noch herunterzuladende PDFs. Andere Formate werden separat als übersprungen gezählt. Lokal archivierte Dokumente, die online nicht mehr vorkommen, gehören nicht zur Zahl „davon offline vorhanden“. Bis hier werden nur die Dokumentenliste und lokale Dateien geprüft, keine PDFs bei der Bank abgerufen.
5. Mit einem **weiteren `F5` in der Übersicht** die fehlenden PDFs herunterladen. `Enter` bestätigt keine Downloads. Mit `↑`/`↓` die Dokumente durchsehen; `Esc` oder `Q` beendet die Sitzung aus der Übersicht ohne PDF-Download. Gibt es nichts herunterzuladen, schließt `F5` den Vorgang ab.
6. Während des Downloads zeigt die TUI den Fortschritt. `Esc` oder `Q` bricht kontrolliert ab. Laufende HTTP-Anfragen können bis zu 60 Sekunden und die anschließende Abmeldung nochmals bis zu 60 Sekunden dauern.

Die Übersicht wartet bis kurz vor Ablauf des aktuellen Zugriffstokens auf die Bestätigung. Ohne Bestätigung werden weder PDFs abgerufen noch Tokens im Hintergrund erneuert. Läuft die Sitzung ab, beendet die App sie und meldet, dass eine neue Anmeldung erforderlich ist.

Für ein bereits bei comdirect freigeschaltetes mobileTAN-Verfahren gibt es `--tan sms`. Die sechsstellige TAN wird einmalig gesendet. Ein automatischer Wechsel des Verfahrens oder die Anzeige einer photoTAN-Scan-Grafik ist nicht implementiert.

**Der API-Download markiert ungelesene Dokumente bei comdirect als gelesen.** Die Anwendung löscht oder verschiebt keine Dokumente auf dem Bankserver. Nicht-PDF-Dokumente und HTML-Vorschaltseiten werden nicht heruntergeladen.

## Konfigurationsdatei

Die App legt `config.json` beim ersten Start mit `F5` an, bevor eine Bankanfrage gesendet wird. Das geschieht auch dann, wenn die anschließende Anmeldung oder der Download fehlschlägt. Beim nächsten Programmstart werden die Werte automatisch geladen. Ein Abbruch vor `F5` speichert keine Änderungen.

Standardmäßig liegt die Konfiguration **im selben Ordner wie die ausführbare Programmdatei**: unter Linux neben `comdirect-postbox-rs`, unter Windows neben `comdirect-postbox-rs.exe`. Der aktuelle Arbeitsordner hat darauf keinen Einfluss. Bei `cargo run --release --locked` ist der Standardpfad `target/release/config.json`, bei einem Debug-Build `target/debug/config.json`.

Eine bereits vorhandene Konfiguration aus einer älteren Version kannst du neben die Programmdatei kopieren oder weiter über `--config` auswählen. Das Programmverzeichnis muss zum Speichern beschreibbar sein.

Einen anderen Pfad kannst du mit `--config` wählen:

```bash
cargo run --release --locked -- --config ./config.json
```

Die Datei kann auch manuell als UTF-8-JSON angelegt werden. Die Schlüssel sind Englisch; alle Werte sind Zeichenketten, damit beispielsweise führende Nullen der Zugangsnummer erhalten bleiben:

```json
{
  "client_id": "DEINE_CLIENT_ID",
  "client_secret": "DEIN_CLIENT_SECRET",
  "username": "12345678"
}
```

Das Client-Secret steht **im Klartext** in dieser Datei. Unter Linux schreibt die App sie atomar mit Dateirechten `0600`; neu angelegte Konfigurationsverzeichnisse erhalten `0700`. Unter Windows gelten die Zugriffsrechte des Zielordners. `config.json` ist durch `.gitignore` ausgeschlossen. Individuell benannte Konfigurationsdateien ebenfalls außerhalb der Versionsverwaltung aufbewahren. Beim Speichern werden genau die drei genannten Werte geschrieben, keine PIN oder TAN. Ist die Datei ungültig oder nicht beschreibbar, erfolgt keine Anmeldung. Die Offline-Demo liest oder schreibt auch bei Angabe von `--config` keine Konfigurationsdatei.

Der Konfigurationspfad hat keinen Einfluss auf die lokale Anmeldesicherung. Ein Wechsel der Konfiguration hebt diese Sicherung nicht auf.

## Fehlermeldungen und HTTP 406

Fehler zeigen den HTTP-Status und den betroffenen API-Pfad an. Beim Schließen einer fehlgeschlagenen Archivierung bleibt die Meldung zusätzlich im normalen Terminal sichtbar; das Programm endet mit Fehlercode `1`. Zugangsdaten, Tokens, TANs und Antwortinhalte werden dabei nicht ausgegeben.

HTTP `406` beim Dokumentabruf bedeutet, dass das angeforderte Antwortformat nicht akzeptiert wird. Version **0.2.0** korrigiert einen Fehler, bei dem sowohl JSON als auch PDF im `Accept`-Header standen: PDF-Downloads senden jetzt ausschließlich `Accept: application/pdf`, die übrigen Anfragen ausschließlich `application/json`. Ein Formatfehler wird nicht durch zusätzliche Anmelde- oder TAN-Versuche behandelt. Vorhandene vollständige Archivdateien werden beim nächsten Start weiterhin erkannt.

## Archiv und Originaldatum

Beispiel für die Ordnerstruktur:

```text
comdirect/
  12345678/
    2026/
      Kontoauszuege/
        2026-09-30_Finanzreport September_<Dokument-ID>.pdf
      Wertpapiere/
      Steuern/
      Kreditkarte/
      Sonstiges/
```

Der Kontoordner heißt wie der **Anmeldename**, also die achtstellige Zugangsnummer aus dem Konfigurationsfeld `username`. Dadurch bleiben mehrere Zugänge getrennt. Die Kategorien werden aus dem Dokumenttitel abgeleitet; unbekannte Titel landen in `Sonstiges`.

Vorhandene Archive mit der früheren Ordnerkennung werden beim Start automatisch auf den Anmeldenamen umbenannt. PDFs, Dokument-IDs und Datumsangaben bleiben dabei erhalten und vorhandene Dokumente werden weiter erkannt. Existieren bereits beide Kontoordner, bricht die App vor einer Bankanfrage ab, damit keine Dateien überschrieben werden; die Archive müssen dann zuerst zusammengeführt werden.

Die von comdirect gelieferten PDF-Bytes bleiben unverändert. Für einen späteren Import kannst du die PDF-Dateien direkt verwenden. **In den Dokumentordnern speichert die App ausschließlich PDFs.**

Ab Version **0.4.0** speichert eine einzige Datei **`archive-index.json` neben der ausgewählten Konfigurationsdatei** die Dokument-IDs, ursprünglichen API-Metadaten einschließlich `dateCreation`, SHA-256-Prüfsummen, Größen, Archivierungszeitpunkte und PDF-Pfade. Standardmäßig liegt sie damit neben der ausführbaren Programmdatei. Auch bei `--config` folgt der Index dem Ordner der Konfiguration. Mehrere Kontoarchive und Zielordner werden in diesem gemeinsamen Index getrennt verwaltet. PIN, TAN und Tokens werden dort nicht gespeichert.

Vorhandene `.postbox.json`-Begleitdateien werden beim nächsten Start einer Archivierung **vor der Anmeldung** automatisch in den zentralen Index übernommen. Die App prüft zunächst alle Begleitdateien des ausgewählten Kontoarchivs, speichert den Index atomar und entfernt erst danach die übernommenen Begleitdateien. Auch Einträge für inzwischen gelöschte PDFs oder online nicht mehr vorhandene Dokumente bleiben erhalten. PDFs werden dabei weder neu heruntergeladen noch verändert; ihre Datumsangaben bleiben erhalten. Nach einer Unterbrechung kann die Übernahme beim nächsten Start fortgesetzt werden. Bei beschädigten oder widersprüchlichen Metadaten erfolgt ein Abbruch vor einer Bankanfrage; die Begleitdateien bleiben bestehen.

Der Index wird bei jeder Änderung über eine vollständig geschriebene und auf den Datenträger übertragene temporäre Datei atomar ersetzt. Unter Unix wird zusätzlich das Indexverzeichnis synchronisiert. Ein beschädigter oder unbekannter Index wird nicht automatisch überschrieben; die App bricht vor der Anmeldung ab. `archive-index.json` ist durch `.gitignore` ausgeschlossen.

Die API liefert `dateCreation` als **Kalenderdatum ohne Uhrzeit oder Zeitzone**. Dieses Datum wird im Dateinamen und als Dateiänderungsdatum mit **00:00 UTC** gespeichert. Das ist eine festgelegte Abbildung; die ursprüngliche Erstellungsuhrzeit lässt sich aus der API nicht rekonstruieren. Manche Programme zeigen den Zeitpunkt in lokaler Zeit an. Ein portables Setzen des Dateisystem-Erstellungsdatums wird nicht zugesagt; bei Importen das Änderungsdatum, den Dateinamen oder die Metadaten verwenden. Interne PDF-Metadaten werden nicht verändert.

Vorhandene PDFs werden auf Dateisignatur, Endemarkierung, Größe und SHA-256 geprüft. Eine fehlende PDF wird erneut heruntergeladen; eine abweichende vorhandene Datei führt zum Abbruch und wird nicht überschrieben. Eine vollständige PDF ohne Metadaten an ihrem erwarteten Pfad wird nach Prüfung wieder ins Archiv aufgenommen, etwa nach einem Abbruch zwischen PDF- und Metadatenspeicherung. Ohne frühere Prüfsumme lässt sich dabei nur die vorhandene Datei als Ausgangsstand erfassen, nicht ihre Übereinstimmung mit dem Bankoriginal beweisen.

**Für Sicherungen das vollständige PDF-Archiv und die zentrale `archive-index.json` gemeinsam sichern.** Der Index merkt sich die PDF-Pfade; PDFs deshalb nicht unabhängig davon umbenennen oder verschieben. Titeländerungen bei comdirect beeinflussen die Erkennung bereits indizierter Dokumente nicht. Bei Archivordnern unterhalb des Indexverzeichnisses werden relative Pfade gespeichert: Das gemeinsame übergeordnete Verzeichnis kann mitsamt Index und Archiv verschoben werden. Außerhalb liegende Archive werden mit absoluten Ordnerpfaden zugeordnet; bei einem Umzug müssen diese Zuordnungen im Index angepasst werden. Relative PDF-Pfade verwenden betriebssystemunabhängig `/`. PDFs sind auf 64 MiB begrenzt; symbolische Links innerhalb des Archivs werden abgelehnt.

## Schutz vor Kontosperren

Zum Schutz vor Kontosperren begrenzt die Anwendung Anmelde- und TAN-Versuche. Es gelten folgende Regeln:

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

Code und Bezeichner sind Englisch, Dokumentation und Oberfläche Deutsch. Die Architektur trennt API (`src/api.rs`), Konfiguration (`src/config.rs`), Archiv (`src/archive.rs`), Anmeldesicherung (`src/safety.rs`), Hintergrundablauf (`src/sync.rs`) und TUI (`src/tui.rs`).

```bash
cargo fmt --all --check
cargo clippy --locked --all-targets -- -D warnings
cargo test --locked
```

Tests verwenden ausschließlich temporäre Verzeichnisse und einen lokalen HTTP-Testserver mit simulierten Bankantworten. Sie prüfen unter anderem den vollständigen Authentifizierungsablauf, TAN-Fehler ohne Wiederholung, Token-Refresh-Fehler, Paginierung, inkrementelle Downloads, Prüfsummen, Datumserhalt und maskierte TUI-Felder. Weitere Tests prüfen das Speichern ohne PIN, Dateirechte, das Vorbelegen der TUI und den HTTP-406-Fall bei widersprüchlichen `Accept`-Werten. Zusätzlich wird geprüft, dass die Übersicht vor jeder Downloadbestätigung ohne PDF-Anfragen wartet, ein Abbruch keinen Download auslöst und die Umbenennung vorhandener Kontoarchive die Dateidaten erhält. Tests des zentralen Index prüfen außerdem die verlustfreie und fortsetzbare Übernahme alter Begleitdateien, fehlende PDFs mit erhaltener Originalprüfsumme, beschädigte und widersprüchliche Metadaten, unsichere Pfade, getrennte Kontoarchive sowie den gemeinsamen Umzug von Index und Archiv. Die CI führt Prüfungen und Release-Builds unter Linux und Windows aus. Ein echter Kontoabruf erfordert deine Zugangsdaten und wurde nicht automatisiert getestet.

Die Seitenparameter `paging-first` und `paging-count` folgen der comdirect-Paging-Konvention; die App prüft den zurückgegebenen Index und bricht bei widersprüchlichen oder wiederholten Seiten ab. Die allgemeine Paging-Konvention erläutert auch ein [Beitrag der comdirect-Fachabteilung](https://community.comdirect.de/t5/website-apps/rest-api-filter-parameter/td-p/108710/page/3).

Die Deaktivierung automatischer HTTP-Wiederholungen verwendet die ausdrücklich dafür vorgesehene [reqwest-Retry-Policy](https://docs.rs/reqwest/0.12.28/reqwest/retry/index.html).

Lizenz: GPL-3.0-or-later; siehe [LICENSE](LICENSE).
