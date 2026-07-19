//! Source abstraction for the non-Postal providers.
//!
//! Postal is read straight from its database (`crate::postal`), which lets the
//! tool carry sending keys and DKIM private keys over unchanged. The four API
//! sources here (Postmark, Resend, Mailgun, SendGrid) are read over each
//! provider's HTTP API instead, and those APIs deliberately do NOT expose
//! existing sending API keys or DKIM private keys. So an API source migrates
//! what its API DOES expose (domains, suppressions, templates, routes and
//! message history) and the tool then mints a FRESH CamelMailer credential and
//! a FRESH DKIM key for each domain. The user updates their app with the new
//! key and publishes the new DKIM DNS record.
//!
//! Every API source is single-account: it produces one [`ApiSnapshot`] that
//! maps onto a single CamelMailer server, plus an on-demand history fetch.

mod mailgun;
mod postmark;
mod resend;
mod sendgrid;

use anyhow::{bail, Context, Result};
use serde_json::Value;

use crate::history::BodyMode;

/// Which source the tool reads from. `Postal` is the database path; the rest
/// are HTTP APIs handled by [`ApiClient`].
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum SourceKind {
    Postal,
    Postmark,
    Resend,
    Mailgun,
    Sendgrid,
}

impl SourceKind {
    pub fn parse(s: &str) -> Result<Self> {
        match s.to_lowercase().as_str() {
            "postal" => Ok(SourceKind::Postal),
            "postmark" => Ok(SourceKind::Postmark),
            "resend" => Ok(SourceKind::Resend),
            "mailgun" => Ok(SourceKind::Mailgun),
            "sendgrid" => Ok(SourceKind::Sendgrid),
            other => bail!(
                "--source must be one of postal, postmark, resend, mailgun, sendgrid (got {other:?})"
            ),
        }
    }

    /// Human-readable provider name, also the default server name.
    pub fn provider_name(self) -> &'static str {
        match self {
            SourceKind::Postal => "Postal",
            SourceKind::Postmark => "Postmark",
            SourceKind::Resend => "Resend",
            SourceKind::Mailgun => "Mailgun",
            SourceKind::Sendgrid => "SendGrid",
        }
    }

    pub fn is_api(self) -> bool {
        self != SourceKind::Postal
    }
}

/// A sending domain to recreate. API sources cannot read the private DKIM key,
/// so CamelMailer mints a fresh one and the user publishes a new DNS record.
#[derive(Debug, Clone)]
pub struct ApiDomain {
    pub name: String,
    /// Whether the provider reported the domain as verified/active. Purely
    /// informational: on the cloud the domain still starts unverified.
    pub verified: bool,
}

/// A recipient the provider will not send to (bounce, complaint, unsubscribe).
#[derive(Debug, Clone)]
pub struct ApiSuppression {
    pub address: String,
    /// Free-text note carried into the CamelMailer suppression `reason`.
    pub reason: String,
}

/// A stored template. Bodies are best effort: some providers only expose the
/// active version's content.
#[derive(Debug, Clone)]
pub struct ApiTemplate {
    pub name: String,
    pub permalink: String,
    pub subject: Option<String>,
    pub html_body: Option<String>,
    pub text_body: Option<String>,
}

/// An inbound/forwarding route (Mailgun only). Maps onto a CamelMailer route.
#[derive(Debug, Clone)]
pub struct ApiRoute {
    pub name: String,
    pub domain: Option<String>,
    /// CamelMailer route mode: `Endpoint`, `Accept`, `Hold`, `Bounce`, `Reject`.
    pub mode: String,
    pub endpoint_url: Option<String>,
}

/// Everything read from an API source's account, ready to map onto one
/// CamelMailer server.
#[derive(Debug, Default)]
pub struct ApiSnapshot {
    pub domains: Vec<ApiDomain>,
    pub suppressions: Vec<ApiSuppression>,
    pub templates: Vec<ApiTemplate>,
    pub routes: Vec<ApiRoute>,
    /// Provider-specific caveats to print in the plan (e.g. an endpoint that
    /// needed an add-on, or an entity the API does not expose).
    pub notes: Vec<String>,
}

/// How a request authenticates. Providers differ (bearer, HTTP basic, and
/// Postmark uses two different header tokens for account vs server scope).
pub enum Auth<'a> {
    Bearer(&'a str),
    Basic(&'a str, &'a str),
    Header(&'a str, &'a str),
}

/// A thin HTTP client for one API source. The provider modules build request
/// URLs and parse responses; this holds the shared reqwest client, resolved
/// base URL and key.
pub struct ApiClient {
    kind: SourceKind,
    http: reqwest::Client,
    base: String,
    api_key: String,
}

impl ApiClient {
    pub fn new(
        kind: SourceKind,
        api_key: &str,
        base_url: Option<&str>,
        region: Option<&str>,
    ) -> Result<Self> {
        let base = match base_url {
            Some(url) => url.trim_end_matches('/').to_string(),
            None => default_base(kind, region)?,
        };
        Ok(Self {
            kind,
            http: reqwest::Client::new(),
            base,
            api_key: api_key.to_string(),
        })
    }

    fn base(&self) -> &str {
        &self.base
    }

    fn api_key(&self) -> &str {
        &self.api_key
    }

    /// GET a URL and return the parsed JSON body. A non-2xx response is an
    /// error carrying the status and any body text, so callers can degrade
    /// gracefully (e.g. SendGrid's Email Activity add-on returning 403).
    async fn get_json(&self, url: &str, auth: Auth<'_>) -> Result<Value> {
        let mut req = self.http.get(url);
        req = match auth {
            Auth::Bearer(token) => req.bearer_auth(token),
            Auth::Basic(user, pass) => req.basic_auth(user, Some(pass)),
            Auth::Header(name, value) => req.header(name, value),
        };
        let resp = req
            .header("Accept", "application/json")
            .send()
            .await
            .with_context(|| format!("requesting {url}"))?;
        let status = resp.status();
        let text = resp.text().await.unwrap_or_default();
        if !status.is_success() {
            let snippet: String = text.chars().take(300).collect();
            bail!("HTTP {} from {url}: {snippet}", status.as_u16());
        }
        if text.is_empty() {
            return Ok(Value::Null);
        }
        serde_json::from_str(&text).with_context(|| format!("parsing JSON from {url}"))
    }

    /// Read the account's configuration (domains, suppressions, templates,
    /// routes). History is fetched separately and only with `--history`.
    pub async fn snapshot(&self) -> Result<ApiSnapshot> {
        match self.kind {
            SourceKind::Postmark => postmark::snapshot(self).await,
            SourceKind::Resend => resend::snapshot(self).await,
            SourceKind::Mailgun => mailgun::snapshot(self).await,
            SourceKind::Sendgrid => sendgrid::snapshot(self).await,
            SourceKind::Postal => bail!("Postal is not an API source"),
        }
    }

    /// Read message history as import-ready JSON objects (the same shape the
    /// Postal path produces for `import_messages`). `domains` is the verified
    /// sending domains, needed by providers whose history is per domain
    /// (Mailgun). Returns the messages plus any note to print.
    pub async fn history(
        &self,
        domains: &[String],
        mode: BodyMode,
    ) -> Result<(Vec<Value>, Vec<String>)> {
        match self.kind {
            SourceKind::Postmark => postmark::history(self, mode).await,
            SourceKind::Resend => resend::history(self, mode).await,
            SourceKind::Mailgun => mailgun::history(self, domains, mode).await,
            SourceKind::Sendgrid => sendgrid::history(self, mode).await,
            SourceKind::Postal => bail!("Postal is not an API source"),
        }
    }
}

fn default_base(kind: SourceKind, region: Option<&str>) -> Result<String> {
    let base = match kind {
        SourceKind::Postmark => "https://api.postmarkapp.com",
        SourceKind::Resend => "https://api.resend.com",
        SourceKind::Sendgrid => "https://api.sendgrid.com",
        SourceKind::Mailgun => match region.map(str::to_lowercase).as_deref() {
            None | Some("us") => "https://api.mailgun.net",
            Some("eu") => "https://api.eu.mailgun.net",
            Some(other) => {
                bail!("--source-region for Mailgun must be 'us' or 'eu' (got {other:?})")
            }
        },
        SourceKind::Postal => bail!("Postal is not an API source"),
    };
    Ok(base.to_string())
}

/// Build a URL-safe permalink from a display name: lowercase, runs of
/// non-alphanumerics collapsed to a single dash, trimmed. Shared by the
/// provider modules for servers, domains-as-names and templates.
pub fn permalink(name: &str) -> String {
    let mut out = String::with_capacity(name.len());
    let mut prev_dash = false;
    for ch in name.chars() {
        if ch.is_ascii_alphanumeric() {
            out.push(ch.to_ascii_lowercase());
            prev_dash = false;
        } else if !prev_dash {
            out.push('-');
            prev_dash = true;
        }
    }
    let trimmed = out.trim_matches('-').to_string();
    if trimmed.is_empty() {
        "template".to_string()
    } else {
        trimmed
    }
}

/// Assemble a synthetic raw message from metadata, base64-encoded for import.
/// The API sources do not return the original raw MIME, so history messages
/// carry minimal synthesized headers (and, in `full` mode, any body text the
/// API did expose). Nothing here is ever re-sent; these are completed records.
pub fn synth_raw(
    from: &str,
    to: &str,
    subject: &str,
    message_id: &str,
    body: Option<&str>,
    mode: BodyMode,
) -> String {
    use base64::Engine;

    let mut raw = format!("From: {from}\r\nTo: {to}\r\nSubject: {subject}\r\n");
    if !message_id.is_empty() {
        raw.push_str(&format!("Message-ID: <{message_id}>\r\n"));
    }
    raw.push_str("\r\n");
    if mode == BodyMode::Full {
        if let Some(body) = body {
            raw.push_str(body);
        }
    }
    base64::engine::general_purpose::STANDARD.encode(raw.as_bytes())
}
