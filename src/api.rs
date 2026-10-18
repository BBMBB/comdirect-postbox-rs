use std::{
    collections::HashSet,
    io::Read,
    thread,
    time::{Duration, Instant},
};

use anyhow::{Context, Result, bail, ensure};
use reqwest::{
    Method,
    blocking::{Client, RequestBuilder, Response},
    header::{ACCEPT, CONTENT_TYPE},
};
use serde::{Deserialize, de::DeserializeOwned};
use serde_json::json;
use uuid::Uuid;
use zeroize::{Zeroize, ZeroizeOnDrop, Zeroizing};

use crate::{
    archive::MAX_PDF_BYTES,
    model::{Document, Page},
};

const ORIGIN: &str = "https://api.comdirect.de";
const MAX_JSON_BYTES: u64 = 8 * 1024 * 1024;

#[derive(Zeroize, ZeroizeOnDrop)]
pub struct Credentials {
    pub client_id: String,
    pub client_secret: String,
    pub username: String,
    pub pin: String,
}

impl Credentials {
    pub fn validate(&self) -> Result<()> {
        ensure!(
            self.username.len() == 8 && self.username.bytes().all(|b| b.is_ascii_digit()),
            "Die Zugangsnummer muss acht Ziffern enthalten"
        );
        ensure!(
            !self.pin.is_empty()
                && !self.client_id.trim().is_empty()
                && !self.client_secret.is_empty(),
            "Bitte alle Zugangsfelder ausfüllen"
        );
        Ok(())
    }
}

#[derive(Clone, Copy, clap::ValueEnum)]
pub enum TanMethod {
    Push,
    Sms,
}

impl TanMethod {
    fn api_name(self) -> &'static str {
        match self {
            Self::Push => "P_TAN_PUSH",
            Self::Sms => "M_TAN",
        }
    }
}

#[derive(Deserialize)]
pub struct Challenge {
    pub id: String,
    pub typ: String,
}

#[derive(Deserialize, Zeroize, ZeroizeOnDrop)]
struct Token {
    access_token: String,
    refresh_token: String,
    #[zeroize(skip)]
    expires_in: Lifetime,
}

#[derive(Deserialize)]
#[serde(untagged)]
enum Lifetime {
    Number(u64),
    Text(String),
}

impl Lifetime {
    fn seconds(&self) -> Result<u64> {
        match self {
            Self::Number(n) => Ok(*n),
            Self::Text(s) => s.parse().context("Ungültige Token-Laufzeit"),
        }
    }
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Session {
    identifier: String,
    session_tan_active: bool,
    #[serde(rename = "activated2FA")]
    activated_2fa: bool,
}

#[derive(Deserialize)]
#[serde(untagged)]
enum Sessions {
    List(Vec<Session>),
    Single(Session),
}

pub struct Api {
    client: Client,
    credentials: Credentials,
    origin: String,
    request_session: String,
    request_number: u32,
    last_request: Option<Instant>,
    token: Option<Token>,
    expires_at: Instant,
    login_started: bool,
}

impl Api {
    pub fn new(credentials: Credentials) -> Result<Self> {
        credentials.validate()?;
        Self::build(credentials, ORIGIN, true)
    }

    fn build(credentials: Credentials, origin: &str, https_only: bool) -> Result<Self> {
        let client = Client::builder()
            .https_only(https_only)
            .redirect(reqwest::redirect::Policy::none())
            .retry(reqwest::retry::never())
            .connect_timeout(Duration::from_secs(10))
            .timeout(Duration::from_secs(60))
            .user_agent(concat!("comdirect-postbox-rs/", env!("CARGO_PKG_VERSION")))
            .build()
            .context("HTTP-Client konnte nicht gestartet werden")?;
        Ok(Self {
            client,
            credentials,
            origin: origin.into(),
            request_session: Uuid::new_v4().simple().to_string(),
            request_number: 0,
            last_request: None,
            token: None,
            expires_at: Instant::now(),
            login_started: false,
        })
    }

    fn request(&mut self, method: Method, path: &str) -> RequestBuilder {
        self.request_number += 1;
        self.client.request(method, format!("{}{path}", self.origin))
            .header(ACCEPT, "application/json")
            .header("x-http-request-info", json!({"clientRequestId": {
                "sessionId": self.request_session, "requestId": format!("{:09}", self.request_number)
            }}).to_string())
    }

    fn authorized(&mut self, method: Method, path: &str) -> Result<RequestBuilder> {
        let request = self.request(method, path);
        let token = self.token.as_ref().context("Keine aktive Sitzung")?;
        Ok(request.bearer_auth(&token.access_token))
    }

    fn send(&mut self, request: RequestBuilder) -> Result<Response> {
        if let Some(last) = self.last_request {
            thread::sleep(Duration::from_millis(350).saturating_sub(last.elapsed()));
        }
        self.last_request = Some(Instant::now());
        let response = request.send().map_err(|_| anyhow::anyhow!(
            "Netzwerkfehler oder Zeitüberschreitung. Anfrage wird nicht wiederholt. Bei Anmeldefehlern zuerst das Webbanking prüfen."))?;
        let status = response.status();
        ensure!(
            status.is_success(),
            "Bankanfrage abgebrochen (HTTP {}). Keine automatische Wiederholung. Bei 401/403/422 bitte Zugang und TAN im Webbanking prüfen; bei 429 später erneut starten.",
            status.as_u16()
        );
        Ok(response)
    }

    fn exchange(&mut self, fields: &[(&str, &str)]) -> Result<()> {
        let request = self.request(Method::POST, "/oauth/token").form(fields);
        let response = self.send(request)?;
        let token: Token = read_json(response)?;
        let seconds = token.expires_in.seconds()?;
        ensure!(
            !token.access_token.is_empty()
                && !token.refresh_token.is_empty()
                && (1..=86400).contains(&seconds),
            "Unvollständige Token-Antwort"
        );
        self.expires_at = Instant::now() + Duration::from_secs(seconds.saturating_sub(30));
        self.token = Some(token);
        Ok(())
    }

    pub fn login(
        &mut self,
        method: TanMethod,
        mut confirm: impl FnMut(&Challenge) -> Result<Zeroizing<String>>,
        mut checkpoint: impl FnMut() -> Result<()>,
    ) -> Result<()> {
        ensure!(
            !self.login_started,
            "Ein zweiter Anmeldeversuch ist in dieser Instanz nicht erlaubt"
        );
        self.login_started = true;
        checkpoint()?;
        let client_id = Zeroizing::new(self.credentials.client_id.clone());
        let client_secret = Zeroizing::new(self.credentials.client_secret.clone());
        let username = Zeroizing::new(self.credentials.username.clone());
        let pin = Zeroizing::new(std::mem::take(&mut self.credentials.pin));
        self.exchange(&[
            ("grant_type", "password"),
            ("client_id", &client_id),
            ("client_secret", &client_secret),
            ("username", &username),
            ("password", &pin),
        ])?;
        checkpoint()?;
        let request = self.authorized(Method::GET, "/api/session/clients/user/v1/sessions")?;
        let sessions: Sessions = read_json(self.send(request)?)?;
        let session = match sessions {
            Sessions::Single(session) => session,
            Sessions::List(mut sessions) => {
                ensure!(sessions.len() == 1, "Keine eindeutige Banksitzung");
                sessions.remove(0)
            }
        };
        ensure!(
            !session.identifier.is_empty()
                && session.identifier.len() <= 40
                && session
                    .identifier
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_'),
            "Ungültige Sitzungs-ID"
        );
        let path = format!(
            "/api/session/clients/user/v1/sessions/{}",
            session.identifier
        );
        let body = json!({"identifier": session.identifier, "sessionTanActive": true, "activated2FA": true});
        if !(session.session_tan_active && session.activated_2fa) {
            checkpoint()?;
            let request = self
                .authorized(Method::POST, &format!("{path}/validate"))?
                .header(
                    "x-once-authentication-info",
                    json!({"typ": method.api_name()}).to_string(),
                )
                .json(&body);
            let response = self.send(request)?;
            let challenge: Challenge = serde_json::from_str(
                response
                    .headers()
                    .get("x-once-authentication-info")
                    .context("TAN-Challenge fehlt; keine neue Challenge angefordert")?
                    .to_str()?,
            )
            .context("Ungültige TAN-Challenge")?;
            ensure!(
                challenge.typ == method.api_name() && !challenge.id.is_empty(),
                "Bank lieferte ein anderes TAN-Verfahren. Abbruch ohne neue TAN-Anforderung; Verfahren im Webbanking prüfen."
            );
            let tan = confirm(&challenge)?;
            checkpoint()?;
            if matches!(method, TanMethod::Sms) {
                ensure!(
                    tan.len() == 6 && tan.bytes().all(|b| b.is_ascii_digit()),
                    "Ungültiges TAN-Format; es wurde keine TAN gesendet"
                );
            }
            let mut request = self
                .authorized(Method::PATCH, &path)?
                .header(
                    "x-once-authentication-info",
                    json!({"id": challenge.id}).to_string(),
                )
                .json(&body);
            if matches!(method, TanMethod::Sms) {
                request = request.header("x-once-authentication", tan.as_str());
            }
            let active: Session = read_json(self.send(request)?)?;
            ensure!(
                active.identifier == session.identifier
                    && active.session_tan_active
                    && active.activated_2fa,
                "Bank hat die Sitzung nicht freigegeben; keine Wiederholung"
            );
        }
        checkpoint()?;
        let token = Zeroizing::new(
            self.token
                .as_ref()
                .context("Token fehlt")?
                .access_token
                .clone(),
        );
        self.exchange(&[
            ("grant_type", "cd_secondary"),
            ("client_id", &client_id),
            ("client_secret", &client_secret),
            ("token", &token),
        ])
    }

    fn refresh_if_needed(&mut self) -> Result<()> {
        if Instant::now() < self.expires_at {
            return Ok(());
        }
        let refresh = Zeroizing::new(
            self.token
                .as_ref()
                .context("Token fehlt")?
                .refresh_token
                .clone(),
        );
        let client_id = Zeroizing::new(self.credentials.client_id.clone());
        let secret = Zeroizing::new(self.credentials.client_secret.clone());
        self.exchange(&[
            ("grant_type", "refresh_token"),
            ("client_id", &client_id),
            ("client_secret", &secret),
            ("refresh_token", &refresh),
        ])
    }

    pub fn documents(
        &mut self,
        mut checkpoint: impl FnMut() -> Result<()>,
    ) -> Result<Vec<Document>> {
        let mut documents = Vec::new();
        let mut seen = HashSet::new();
        let mut index = 0;
        loop {
            checkpoint()?;
            self.refresh_if_needed()?;
            let request = self
                .authorized(Method::GET, "/api/messages/clients/user/v2/documents")?
                .query(&[("paging-first", index), ("paging-count", 100)]);
            let page: Page = read_json(self.send(request)?)?;
            ensure!(
                page.paging.index == index,
                "Bank ignoriert den Seitenindex; Abbruch statt Endlosschleife"
            );
            ensure!(page.paging.matches <= 100_000, "Unplausible Dokumentanzahl");
            if page.values.is_empty() {
                ensure!(
                    index >= page.paging.matches,
                    "Unvollständige Dokumentenliste"
                );
                break;
            }
            index += page.values.len();
            for doc in page.values {
                doc.validate()?;
                ensure!(
                    seen.insert(doc.document_id.clone()),
                    "Dokument mehrfach in Seitenantwort; Liste hat sich geändert. Bitte später erneut abrufen."
                );
                documents.push(doc);
            }
            ensure!(
                index <= page.paging.matches,
                "Widersprüchliche Dokumentanzahl"
            );
            if index == page.paging.matches {
                break;
            }
        }
        Ok(documents)
    }

    pub fn download(&mut self, doc: &Document) -> Result<Vec<u8>> {
        doc.validate()?;
        ensure!(doc.is_pdf(), "Nur PDF-Dokumente werden heruntergeladen");
        self.refresh_if_needed()?;
        let request = self
            .authorized(
                Method::GET,
                &format!("/api/messages/v2/documents/{}", doc.document_id),
            )?
            .header(ACCEPT, "application/pdf");
        let response = self.send(request)?;
        let content_type = response
            .headers()
            .get(CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .unwrap_or("");
        ensure!(
            content_type
                .split(';')
                .next()
                .unwrap_or("")
                .trim()
                .eq_ignore_ascii_case("application/pdf"),
            "Bank lieferte keine PDF-Datei"
        );
        let mut bytes = Vec::new();
        response
            .take(MAX_PDF_BYTES + 1)
            .read_to_end(&mut bytes)
            .context("PDF-Download unterbrochen; keine Wiederholung")?;
        crate::archive::validate_pdf(&bytes)?;
        Ok(bytes)
    }

    pub fn logout(&mut self) -> Result<()> {
        if self.token.is_none() {
            return Ok(());
        }
        let request = self.authorized(Method::DELETE, "/oauth/revoke")?;
        let result = self.send(request).map(|_| ());
        self.token = None;
        result
    }
}

fn read_json<T: DeserializeOwned>(response: Response) -> Result<T> {
    let mut bytes = Zeroizing::new(Vec::new());
    response
        .take(MAX_JSON_BYTES + 1)
        .read_to_end(&mut bytes)
        .context("API-Antwort unterbrochen")?;
    if bytes.len() as u64 > MAX_JSON_BYTES {
        bail!("API-Antwort überschreitet das Größenlimit");
    }
    serde_json::from_slice(&bytes).context("Unerwartetes Format der API-Antwort")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{
        io::Write,
        net::TcpListener,
        sync::{Arc, Mutex},
        thread::JoinHandle,
    };

    struct Step {
        method: &'static str,
        path: &'static str,
        status: u16,
        headers: Vec<(&'static str, &'static str)>,
        body: String,
    }

    fn json_step(method: &'static str, path: &'static str, value: serde_json::Value) -> Step {
        Step {
            method,
            path,
            status: 200,
            headers: vec![("Content-Type", "application/json")],
            body: value.to_string(),
        }
    }

    struct Server {
        origin: String,
        requests: Arc<Mutex<Vec<String>>>,
        handle: JoinHandle<()>,
    }

    fn serve(steps: Vec<Step>) -> Server {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let origin = format!("http://{}", listener.local_addr().unwrap());
        let requests = Arc::new(Mutex::new(Vec::new()));
        let captured = requests.clone();
        let handle = thread::spawn(move || {
            for step in steps {
                let deadline = Instant::now() + Duration::from_secs(15);
                let mut stream = loop {
                    match listener.accept() {
                        Ok((stream, _)) => break stream,
                        Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                            assert!(
                                Instant::now() < deadline,
                                "Expected mock request was never sent"
                            );
                            thread::sleep(Duration::from_millis(10));
                        }
                        Err(error) => panic!("Mock accept failed: {error}"),
                    }
                };
                stream
                    .set_read_timeout(Some(Duration::from_secs(5)))
                    .unwrap();
                let mut bytes = Vec::new();
                let mut buffer = [0; 4096];
                loop {
                    let count = stream.read(&mut buffer).unwrap();
                    assert!(count > 0);
                    bytes.extend_from_slice(&buffer[..count]);
                    if let Some(end) = bytes.windows(4).position(|w| w == b"\r\n\r\n") {
                        let header = String::from_utf8_lossy(&bytes[..end]).to_ascii_lowercase();
                        let size = header
                            .lines()
                            .find_map(|line| line.strip_prefix("content-length: "))
                            .map(|s| s.parse::<usize>().unwrap())
                            .unwrap_or(0);
                        if bytes.len() >= end + 4 + size {
                            break;
                        }
                    }
                }
                let request = String::from_utf8(bytes).unwrap();
                let first = request.lines().next().unwrap();
                assert_eq!(first, format!("{} {} HTTP/1.1", step.method, step.path));
                captured.lock().unwrap().push(request);
                write!(
                    stream,
                    "HTTP/1.1 {} Mock\r\nContent-Length: {}\r\nConnection: close\r\n",
                    step.status,
                    step.body.len()
                )
                .unwrap();
                for (key, value) in step.headers {
                    write!(stream, "{key}: {value}\r\n").unwrap();
                }
                write!(stream, "\r\n{}", step.body).unwrap();
            }
        });
        Server {
            origin,
            requests,
            handle,
        }
    }

    fn credentials() -> Credentials {
        Credentials {
            client_id: "test-client".into(),
            client_secret: "test-secret".into(),
            username: "12345678".into(),
            pin: "123456".into(),
        }
    }

    fn token() -> serde_json::Value {
        json!({"access_token":"test-access", "refresh_token":"test-refresh", "expires_in":599})
    }

    fn session(active: bool) -> serde_json::Value {
        json!({"identifier":"test-session", "sessionTanActive":active, "activated2FA":active})
    }

    fn login_steps(method: &'static str) -> Vec<Step> {
        let mut challenge = json_step(
            "POST",
            "/api/session/clients/user/v1/sessions/test-session/validate",
            session(false),
        );
        challenge.status = 201;
        challenge.headers.push((
            "x-once-authentication-info",
            if method == "M_TAN" {
                r#"{"id":"challenge-1","typ":"M_TAN"}"#
            } else {
                r#"{"id":"challenge-1","typ":"P_TAN_PUSH"}"#
            },
        ));
        vec![
            json_step("POST", "/oauth/token", token()),
            json_step(
                "GET",
                "/api/session/clients/user/v1/sessions",
                json!([session(false)]),
            ),
            challenge,
            json_step(
                "PATCH",
                "/api/session/clients/user/v1/sessions/test-session",
                session(true),
            ),
            json_step("POST", "/oauth/token", token()),
        ]
    }

    fn doc(id: &str) -> serde_json::Value {
        json!({"documentId":id, "name":"Finanzreport", "dateCreation":"2024-01-02", "mimeType":"application/pdf"})
    }

    #[test]
    fn push_login_pagination_download_and_second_run_skip() {
        let mut steps = login_steps("P_TAN_PUSH");
        steps.extend([
            json_step(
                "GET",
                "/api/messages/clients/user/v2/documents?paging-first=0&paging-count=100",
                json!({"paging":{"index":0,"matches":2},"values":[doc("id-1")]}),
            ),
            json_step(
                "GET",
                "/api/messages/clients/user/v2/documents?paging-first=1&paging-count=100",
                json!({"paging":{"index":1,"matches":2},"values":[doc("id-2")]}),
            ),
            Step {
                method: "GET",
                path: "/api/messages/v2/documents/id-1",
                status: 200,
                headers: vec![("Content-Type", "application/pdf")],
                body: "%PDF-1.4\noriginal\n%%EOF\n".into(),
            },
            Step {
                method: "GET",
                path: "/api/messages/v2/documents/id-2",
                status: 200,
                headers: vec![("Content-Type", "application/pdf")],
                body: "%PDF-1.4\nsecond\n%%EOF\n".into(),
            },
            Step {
                method: "DELETE",
                path: "/oauth/revoke",
                status: 204,
                headers: vec![],
                body: String::new(),
            },
        ]);
        steps.extend(login_steps("P_TAN_PUSH"));
        steps.push(json_step(
            "GET",
            "/api/messages/clients/user/v2/documents?paging-first=0&paging-count=100",
            json!({"paging":{"index":0,"matches":2},"values":[doc("id-1"), doc("id-2")]}),
        ));
        steps.push(Step {
            method: "DELETE",
            path: "/oauth/revoke",
            status: 204,
            headers: vec![],
            body: String::new(),
        });
        let server = serve(steps);
        let dir = tempfile::tempdir().unwrap();
        for run in 0..2 {
            let mut api = Api::build(credentials(), &server.origin, false).unwrap();
            api.login(
                TanMethod::Push,
                |_| Ok(Zeroizing::new(String::new())),
                || Ok(()),
            )
            .unwrap();
            let documents = api.documents(|| Ok(())).unwrap();
            let mut archive = crate::archive::Archive::open(dir.path()).unwrap();
            let mut downloaded = 0;
            for doc in documents {
                if !archive.exists_verified(&doc).unwrap() {
                    archive.store(&doc, &api.download(&doc).unwrap()).unwrap();
                    downloaded += 1;
                }
            }
            assert_eq!(downloaded, if run == 0 { 2 } else { 0 });
            api.logout().unwrap();
            assert!(api.token.is_none());
        }
        server.handle.join().unwrap();
        let requests = server.requests.lock().unwrap();
        assert_eq!(
            requests
                .iter()
                .filter(|r| r.starts_with("GET /api/messages/v2/documents/"))
                .count(),
            2
        );
        for request in requests.iter() {
            let header = request
                .lines()
                .find_map(|line| line.strip_prefix("x-http-request-info: "))
                .unwrap();
            let info: serde_json::Value = serde_json::from_str(header).unwrap();
            let session_id = info["clientRequestId"]["sessionId"].as_str().unwrap();
            assert_eq!(session_id.len(), 32);
            assert!(session_id.bytes().all(|b| b.is_ascii_hexdigit()));
            assert_eq!(
                info["clientRequestId"]["requestId"].as_str().unwrap().len(),
                9
            );
        }
        let activation = requests.iter().find(|r| r.starts_with("PATCH")).unwrap();
        assert!(!activation.contains("x-once-authentication:"));
        assert!(activation.contains("challenge-1"));
        assert!(
            requests
                .iter()
                .any(|r| r.contains("grant_type=cd_secondary"))
        );
        assert!(
            requests
                .iter()
                .filter(|r| r.starts_with("GET /api/messages/v2/"))
                .all(|r| r.contains("accept: application/pdf"))
        );
    }

    #[test]
    fn failed_password_is_not_retried_and_remains_locally_latched() {
        let mut step = json_step("POST", "/oauth/token", json!({"error":"invalid_grant"}));
        step.status = 401;
        let server = serve(vec![step]);
        let state_dir = tempfile::tempdir().unwrap();
        let state = crate::safety::Safety::open(state_dir.path()).unwrap();
        state.begin("12345678").unwrap();
        let mut api = Api::build(credentials(), &server.origin, false).unwrap();
        assert!(
            api.login(
                TanMethod::Push,
                |_| panic!("No challenge expected"),
                || Ok(())
            )
            .is_err()
        );
        assert!(
            api.login(
                TanMethod::Push,
                |_| panic!("No challenge expected"),
                || Ok(())
            )
            .is_err()
        );
        assert!(state.begin("12345678").is_err());
        server.handle.join().unwrap();
        assert_eq!(server.requests.lock().unwrap().len(), 1);
    }

    #[test]
    fn failed_sms_tan_is_sent_once_without_secondary_flow() {
        let mut steps = login_steps("M_TAN");
        steps.pop();
        steps.last_mut().unwrap().status = 422;
        let server = serve(steps);
        let mut api = Api::build(credentials(), &server.origin, false).unwrap();
        assert!(
            api.login(
                TanMethod::Sms,
                |_| Ok(Zeroizing::new("654321".into())),
                || Ok(())
            )
            .is_err()
        );
        server.handle.join().unwrap();
        let requests = server.requests.lock().unwrap();
        assert_eq!(requests.len(), 4);
        assert!(requests[3].contains("x-once-authentication: 654321"));
    }

    #[test]
    fn cancellation_before_login_sends_nothing() {
        let server = serve(Vec::new());
        let mut api = Api::build(credentials(), &server.origin, false).unwrap();
        assert!(
            api.login(
                TanMethod::Push,
                |_| panic!("Unexpected challenge"),
                || bail!("Cancelled")
            )
            .is_err()
        );
        server.handle.join().unwrap();
        assert!(server.requests.lock().unwrap().is_empty());
    }

    #[test]
    fn refresh_failure_does_not_retry_or_reauthenticate() {
        let mut step = json_step("POST", "/oauth/token", json!({"error":"invalid_grant"}));
        step.status = 401;
        let server = serve(vec![step]);
        let mut api = Api::build(credentials(), &server.origin, false).unwrap();
        api.token = Some(serde_json::from_value(token()).unwrap());
        assert!(api.documents(|| Ok(())).is_err());
        server.handle.join().unwrap();
        let requests = server.requests.lock().unwrap();
        assert_eq!(requests.len(), 1);
        assert!(requests[0].contains("grant_type=refresh_token"));
        assert!(!requests[0].contains("grant_type=password"));
    }

    #[test]
    fn ignored_pagination_is_detected() {
        let server = serve(vec![
            json_step(
                "GET",
                "/api/messages/clients/user/v2/documents?paging-first=0&paging-count=100",
                json!({"paging":{"index":0,"matches":2},"values":[doc("id-1")]}),
            ),
            json_step(
                "GET",
                "/api/messages/clients/user/v2/documents?paging-first=1&paging-count=100",
                json!({"paging":{"index":0,"matches":2},"values":[doc("id-1")]}),
            ),
        ]);
        let mut api = Api::build(credentials(), &server.origin, false).unwrap();
        api.token = Some(serde_json::from_value(token()).unwrap());
        api.expires_at = Instant::now() + Duration::from_secs(599);
        assert!(api.documents(|| Ok(())).is_err());
        server.handle.join().unwrap();
        assert_eq!(server.requests.lock().unwrap().len(), 2);
    }
}
