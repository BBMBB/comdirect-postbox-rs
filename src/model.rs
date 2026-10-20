use anyhow::{Context, Result, ensure};
use chrono::{DateTime, NaiveDate, Utc};
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, PartialEq, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Document {
    pub document_id: String,
    pub name: String,
    pub date_creation: String,
    pub mime_type: String,
    #[serde(flatten)]
    pub extra: serde_json::Map<String, serde_json::Value>,
}

impl Document {
    pub fn date(&self) -> Result<NaiveDate> {
        // The API defines DateString as a date without time or timezone.
        ensure!(
            self.date_creation.len() == 10,
            "Dokumentdatum muss das Format JJJJ-MM-TT haben"
        );
        NaiveDate::parse_from_str(&self.date_creation, "%Y-%m-%d")
            .context("Ungültiges Dokumentdatum; kein Ersatz durch das heutige Datum")
    }

    pub fn timestamp(&self) -> Result<DateTime<Utc>> {
        Ok(self.date()?.and_hms_opt(0, 0, 0).unwrap().and_utc())
    }

    pub fn validate(&self) -> Result<()> {
        ensure!(
            !self.document_id.is_empty()
                && self.document_id.len() <= 128
                && self
                    .document_id
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_'),
            "Ungültige Dokument-ID"
        );
        self.date()?;
        Ok(())
    }

    pub fn is_pdf(&self) -> bool {
        self.mime_type
            .split(';')
            .next()
            .unwrap_or("")
            .trim()
            .eq_ignore_ascii_case("application/pdf")
    }

    pub fn category(&self) -> &'static str {
        let name = self.name.to_lowercase();
        if name.contains("steuer") || name.contains("verlustbescheinigung") {
            "Steuern"
        } else if name.contains("kontoauszug") || name.contains("finanzreport") {
            "Kontoauszuege"
        } else if name.contains("kreditkarte") || name.contains("visa") {
            "Kreditkarte"
        } else if [
            "wertpapier",
            "abrechnung",
            "dividende",
            "ertrag",
            "depot",
            "zinsgutschrift",
        ]
        .iter()
        .any(|word| name.contains(word))
        {
            "Wertpapiere"
        } else {
            "Sonstiges"
        }
    }
}

#[derive(Deserialize)]
pub struct Page {
    pub paging: Paging,
    pub values: Vec<Document>,
}

#[derive(Deserialize)]
pub struct Paging {
    pub index: usize,
    pub matches: usize,
}

pub fn safe_component(value: &str, max_bytes: usize) -> String {
    let mut cleaned = String::new();
    for c in value.chars() {
        let c = if c.is_control() || r#"<>:"/\|?*"#.contains(c) {
            '_'
        } else {
            c
        };
        if cleaned.len() + c.len_utf8() > max_bytes {
            break;
        }
        cleaned.push(c);
    }
    let cleaned = cleaned.trim_matches([' ', '.']);
    if cleaned.is_empty() {
        "Dokument".into()
    } else {
        cleaned.into()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    pub fn document(id: &str) -> Document {
        Document {
            document_id: id.into(),
            name: "Finanzreport / Oktober".into(),
            date_creation: "2024-10-09".into(),
            mime_type: "application/pdf".into(),
            extra: serde_json::Map::new(),
        }
    }

    #[test]
    fn dates_are_never_replaced_or_guessed() {
        let mut doc = document("abc");
        assert_eq!(
            doc.timestamp().unwrap().to_rfc3339(),
            "2024-10-09T00:00:00+00:00"
        );
        doc.date_creation = "2024-02-30".into();
        assert!(doc.validate().is_err());
    }

    #[test]
    fn remote_data_cannot_inject_paths() {
        assert_eq!(safe_component("../a\\b:<c>?\n", 100), "_a_b__c___");
        assert!(document("../../bad").validate().is_err());
        assert_eq!(document("abc").category(), "Kontoauszuege");
        assert!(safe_component(&"🦀".repeat(100), 48).len() <= 48);
    }
}
