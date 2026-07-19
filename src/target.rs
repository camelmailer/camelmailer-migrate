//! The CamelMailer side: a thin admin-API client that decides, from the
//! target URL, whether it is talking to the hosted cloud or a self-hosted
//! installation, and authenticates accordingly.
//!
//! * Cloud (`*.camelmailer.com`): a user token in `Authorization: Bearer`,
//!   scoped to the caller's organization. Organizations already exist, so
//!   the migration targets one with `--org`.
//! * Self-hosted (any other host): the machine `X-Admin-API-Key`, which has
//!   full access and can create organizations, IP pools and force-verify
//!   domains.

use std::fmt;

use anyhow::{Context, Result};
use serde_json::{json, Value};
use url::Url;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Mode {
    Cloud,
    SelfHosted,
}

impl fmt::Display for Mode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Mode::Cloud => write!(f, "cloud"),
            Mode::SelfHosted => write!(f, "self-hosted"),
        }
    }
}

/// A structured API error so the caller can tell fatal auth failures apart
/// from per-item problems (a duplicate name, a validation error) that should
/// only warn and let the rest of the migration continue.
#[derive(Debug)]
pub struct ApiErr {
    pub http: u16,
    pub code: String,
    pub message: String,
}

impl fmt::Display for ApiErr {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.code.is_empty() {
            write!(f, "HTTP {}: {}", self.http, self.message)
        } else {
            write!(f, "{} ({})", self.message, self.code)
        }
    }
}

impl ApiErr {
    /// Auth or permission failures are fatal: nothing else will work either.
    pub fn is_fatal(&self) -> bool {
        self.http == 401 || self.http == 403
    }

    /// A name/permalink that already exists, so a re-run can treat it as done.
    pub fn is_conflict(&self) -> bool {
        self.http == 409
            || self.message.to_lowercase().contains("already")
            || self.message.to_lowercase().contains("taken")
            || self.message.to_lowercase().contains("exists")
    }
}

pub struct Target {
    http: reqwest::Client,
    base: String,
    api_key: String,
    pub mode: Mode,
}

impl Target {
    pub fn new(target_url: &str, api_key: &str, mode_override: Option<Mode>) -> Result<Self> {
        let url =
            Url::parse(target_url).with_context(|| format!("parsing target URL {target_url:?}"))?;
        let host = url.host_str().unwrap_or_default().to_lowercase();
        let detected = if host == "camelmailer.com" || host.ends_with(".camelmailer.com") {
            Mode::Cloud
        } else {
            Mode::SelfHosted
        };
        let mode = mode_override.unwrap_or(detected);
        // Normalize base: scheme://host[:port], no trailing slash or path.
        let base = format!("{}://{}", url.scheme(), url.host_str().unwrap_or_default(),);
        let base = match url.port() {
            Some(port) => format!("{base}:{port}"),
            None => base,
        };
        Ok(Self {
            http: reqwest::Client::new(),
            base,
            api_key: api_key.to_string(),
            mode,
        })
    }

    fn url(&self, path: &str) -> String {
        format!("{}/api/v2/admin{}", self.base, path)
    }

    async fn send(
        &self,
        method: reqwest::Method,
        path: &str,
        body: Value,
    ) -> Result<Value, ApiErr> {
        let mut req = self.http.request(method, self.url(path));
        req = match self.mode {
            Mode::Cloud => req.bearer_auth(&self.api_key),
            Mode::SelfHosted => req.header("X-Admin-API-Key", &self.api_key),
        };
        if !body.is_null() {
            req = req.json(&body);
        }
        let resp = req.send().await.map_err(|e| ApiErr {
            http: 0,
            code: String::new(),
            message: format!("request failed: {e}"),
        })?;
        let http = resp.status().as_u16();
        let value: Value = resp.json().await.unwrap_or(Value::Null);
        let status = value.get("status").and_then(Value::as_str).unwrap_or("");
        if (200..300).contains(&http) && status != "error" {
            return Ok(value.get("data").cloned().unwrap_or(Value::Null));
        }
        let err = value.get("error");
        Err(ApiErr {
            http,
            code: err
                .and_then(|e| e.get("code"))
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string(),
            message: err
                .and_then(|e| e.get("message"))
                .and_then(Value::as_str)
                .unwrap_or("request was not successful")
                .to_string(),
        })
    }

    async fn post(&self, path: &str, body: Value) -> Result<Value, ApiErr> {
        self.send(reqwest::Method::POST, path, body).await
    }

    /// Confirm the URL and key work before doing anything, and report the
    /// resolved mode. Listing organizations is a harmless authenticated read.
    pub async fn check(&self) -> Result<(), ApiErr> {
        self.send(reqwest::Method::GET, "/organizations", Value::Null)
            .await
            .map(|_| ())
    }

    pub async fn create_org(&self, name: &str, permalink: &str) -> Result<Value, ApiErr> {
        self.post(
            "/organizations",
            json!({ "name": name, "permalink": permalink }),
        )
        .await
    }

    pub async fn create_server(
        &self,
        org: &str,
        name: &str,
        permalink: &str,
        mode: &str,
    ) -> Result<Value, ApiErr> {
        self.post(
            &format!("/organizations/{org}/servers"),
            json!({ "name": name, "permalink": permalink, "mode": mode }),
        )
        .await
    }

    pub async fn create_domain(
        &self,
        org: &str,
        server: &str,
        name: &str,
        dkim_private_key: Option<&str>,
    ) -> Result<Value, ApiErr> {
        let mut body = json!({ "name": name });
        if let Some(pem) = dkim_private_key {
            body["dkim_private_key"] = json!(pem);
        }
        self.post(
            &format!("/organizations/{org}/servers/{server}/domains"),
            body,
        )
        .await
    }

    /// Self-hosted only: mark a domain verified without waiting for DNS,
    /// because the source install already proved ownership.
    pub async fn force_verify_domain(
        &self,
        org: &str,
        server: &str,
        name: &str,
    ) -> Result<Value, ApiErr> {
        self.post(
            &format!("/organizations/{org}/servers/{server}/domains/{name}/verify"),
            json!({ "force": true }),
        )
        .await
    }

    pub async fn create_credential(
        &self,
        org: &str,
        server: &str,
        kind: &str,
        name: &str,
        key: &str,
    ) -> Result<Value, ApiErr> {
        self.post(
            &format!("/organizations/{org}/servers/{server}/credentials"),
            json!({ "type": kind, "name": name, "key": key }),
        )
        .await
    }

    #[allow(clippy::too_many_arguments)]
    pub async fn create_webhook(
        &self,
        org: &str,
        server: &str,
        name: &str,
        url: &str,
        all_events: bool,
        sign: bool,
        events: &[String],
    ) -> Result<Value, ApiErr> {
        self.post(
            &format!("/organizations/{org}/servers/{server}/webhooks"),
            json!({
                "name": name,
                "url": url,
                "all_events": all_events,
                "sign": sign,
                "events": events,
            }),
        )
        .await
    }

    pub async fn create_route(
        &self,
        org: &str,
        server: &str,
        name: &str,
        domain: Option<&str>,
        mode: &str,
        endpoint_url: Option<&str>,
    ) -> Result<Value, ApiErr> {
        let mut body = json!({ "name": name, "mode": mode });
        if let Some(d) = domain {
            body["domain"] = json!(d);
        }
        if let Some(u) = endpoint_url {
            body["endpoint_url"] = json!(u);
        }
        self.post(
            &format!("/organizations/{org}/servers/{server}/routes"),
            body,
        )
        .await
    }

    /// Bulk-import historical messages (does not send). `messages` is a slice
    /// of the import JSON objects prepared from the Postal message database.
    pub async fn import_messages(
        &self,
        org: &str,
        server: &str,
        messages: &[Value],
    ) -> Result<Value, ApiErr> {
        self.post(
            &format!("/organizations/{org}/servers/{server}/messages/import"),
            json!({ "messages": messages }),
        )
        .await
    }

    pub async fn create_ip_pool(&self, name: &str, default: bool) -> Result<Value, ApiErr> {
        self.post("/ip_pools", json!({ "name": name, "default": default }))
            .await
    }

    pub async fn create_ip_address(
        &self,
        pool_id: i64,
        ipv4: Option<&str>,
        ipv6: Option<&str>,
        hostname: Option<&str>,
    ) -> Result<Value, ApiErr> {
        self.post(
            &format!("/ip_pools/{pool_id}/ip_addresses"),
            json!({ "ipv4": ipv4, "ipv6": ipv6, "hostname": hostname }),
        )
        .await
    }
}

/// Pull a string field out of a created-entity payload, trying the entity
/// wrapper first (`data.<entity>.<field>`) then the bare field.
pub fn field<'a>(data: &'a Value, entity: &str, field: &str) -> Option<&'a str> {
    data.get(entity)
        .and_then(|e| e.get(field))
        .or_else(|| data.get(field))
        .and_then(Value::as_str)
}
