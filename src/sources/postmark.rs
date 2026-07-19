//! Postmark source, read over the Account API and per-server Server API.
//!
//! `--source-api-key` is the Postmark **account** token
//! (`X-Postmark-Account-Token`). It lists the account's servers (each row
//! carries that server's Server API token) and the account's sending domains
//! and sender signatures. Server-scoped data (templates, bounces/suppressions
//! and outbound message history) is then read per server with each server's
//! Server API token and folded into the one CamelMailer server.
//!
//! Not portable over the API: existing sending API keys and DKIM private keys.
//! The tool mints a fresh credential and a fresh DKIM key instead.

use std::collections::HashSet;

use anyhow::{Context, Result};
use serde_json::{json, Value};

use super::{
    permalink, synth_raw, ApiClient, ApiDomain, ApiSnapshot, ApiSuppression, ApiTemplate, Auth,
};
use crate::history::BodyMode;

const ACCOUNT_HEADER: &str = "X-Postmark-Account-Token";
const SERVER_HEADER: &str = "X-Postmark-Server-Token";
const PAGE: usize = 500;
/// Postmark caps `offset + count` at 10,000 on paged list endpoints.
const MAX_OFFSET: usize = 10_000;

fn account_auth(client: &ApiClient) -> Auth<'_> {
    Auth::Header(ACCOUNT_HEADER, client.api_key())
}

/// The Server API tokens of every server on the account.
async fn server_tokens(client: &ApiClient) -> Result<Vec<String>> {
    let url = format!("{}/servers?count={PAGE}&offset=0", client.base());
    let data = client.get_json(&url, account_auth(client)).await?;
    let mut tokens = Vec::new();
    if let Some(servers) = data.get("Servers").and_then(Value::as_array) {
        for server in servers {
            if let Some(token) = server
                .get("ApiTokens")
                .and_then(Value::as_array)
                .and_then(|a| a.first())
                .and_then(Value::as_str)
            {
                tokens.push(token.to_string());
            }
        }
    }
    Ok(tokens)
}

pub async fn snapshot(client: &ApiClient) -> Result<ApiSnapshot> {
    let mut snap = ApiSnapshot::default();
    let mut domain_names: HashSet<String> = HashSet::new();

    // Account-level sending domains.
    let url = format!("{}/domains?count={PAGE}&offset=0", client.base());
    let data = client.get_json(&url, account_auth(client)).await?;
    if let Some(domains) = data.get("Domains").and_then(Value::as_array) {
        for d in domains {
            let name = d.get("Name").and_then(Value::as_str).unwrap_or_default();
            if name.is_empty() || !domain_names.insert(name.to_lowercase()) {
                continue;
            }
            let verified = d
                .get("DKIMVerified")
                .and_then(Value::as_bool)
                .unwrap_or(false);
            snap.domains.push(ApiDomain {
                name: name.to_string(),
                verified,
            });
        }
    }

    // Sender signatures: each confirmed single-sender address contributes its
    // domain as a sending domain if not already covered.
    let url = format!("{}/senders?count={PAGE}&offset=0", client.base());
    match client.get_json(&url, account_auth(client)).await {
        Ok(data) => {
            if let Some(sigs) = data.get("SenderSignatures").and_then(Value::as_array) {
                for s in sigs {
                    let domain = s.get("Domain").and_then(Value::as_str).unwrap_or_default();
                    if domain.is_empty() || !domain_names.insert(domain.to_lowercase()) {
                        continue;
                    }
                    let verified = s.get("Confirmed").and_then(Value::as_bool).unwrap_or(false);
                    snap.domains.push(ApiDomain {
                        name: domain.to_string(),
                        verified,
                    });
                }
            }
        }
        Err(error) => snap
            .notes
            .push(format!("could not read sender signatures: {error}")),
    }

    // Per-server templates and suppressions, folded together.
    let tokens = server_tokens(client).await.unwrap_or_default();
    if tokens.is_empty() {
        snap.notes.push(
            "no Postmark servers were visible with this account token; templates, suppressions and \
             history need the account token that owns the servers"
                .to_string(),
        );
    }
    let mut template_permalinks: HashSet<String> = HashSet::new();
    let mut suppressed: HashSet<String> = HashSet::new();
    for token in &tokens {
        collect_templates(client, token, &mut snap, &mut template_permalinks).await;
        collect_bounces(client, token, &mut snap, &mut suppressed).await;
    }

    snap.notes.push(
        "Postmark API keys and DKIM private keys are not readable over the API; a fresh CamelMailer \
         credential and DKIM key are created instead"
            .to_string(),
    );
    Ok(snap)
}

async fn collect_templates(
    client: &ApiClient,
    token: &str,
    snap: &mut ApiSnapshot,
    seen: &mut HashSet<String>,
) {
    let url = format!("{}/templates?count=300&offset=0", client.base());
    let list = match client
        .get_json(&url, Auth::Header(SERVER_HEADER, token))
        .await
    {
        Ok(list) => list,
        Err(error) => {
            snap.notes
                .push(format!("could not read templates: {error}"));
            return;
        }
    };
    let Some(items) = list.get("Templates").and_then(Value::as_array) else {
        return;
    };
    for item in items {
        let Some(id) = item.get("TemplateId").and_then(Value::as_i64) else {
            continue;
        };
        let name = item
            .get("Name")
            .and_then(Value::as_str)
            .unwrap_or("template")
            .to_string();
        let alias = item.get("Alias").and_then(Value::as_str);
        let link = alias
            .filter(|a| !a.is_empty())
            .map(permalink)
            .unwrap_or_else(|| permalink(&name));
        if !seen.insert(link.clone()) {
            continue;
        }
        // The list omits bodies; fetch the full template for subject/bodies.
        let detail_url = format!("{}/templates/{id}", client.base());
        let subject;
        let html;
        let text;
        match client
            .get_json(&detail_url, Auth::Header(SERVER_HEADER, token))
            .await
        {
            Ok(detail) => {
                subject = str_field(&detail, "Subject");
                html = str_field(&detail, "HtmlBody");
                text = str_field(&detail, "TextBody");
            }
            Err(_) => {
                subject = None;
                html = None;
                text = None;
            }
        }
        snap.templates.push(ApiTemplate {
            name,
            permalink: link,
            subject,
            html_body: html,
            text_body: text,
        });
    }
}

async fn collect_bounces(
    client: &ApiClient,
    token: &str,
    snap: &mut ApiSnapshot,
    seen: &mut HashSet<String>,
) {
    let mut offset = 0usize;
    loop {
        let url = format!("{}/bounces?count={PAGE}&offset={offset}", client.base());
        let data = match client
            .get_json(&url, Auth::Header(SERVER_HEADER, token))
            .await
        {
            Ok(data) => data,
            Err(error) => {
                snap.notes.push(format!("could not read bounces: {error}"));
                return;
            }
        };
        let Some(items) = data.get("Bounces").and_then(Value::as_array) else {
            return;
        };
        if items.is_empty() {
            return;
        }
        for item in items {
            let email = item
                .get("Email")
                .and_then(Value::as_str)
                .unwrap_or_default();
            if email.is_empty() || !seen.insert(email.to_lowercase()) {
                continue;
            }
            let kind = item.get("Type").and_then(Value::as_str).unwrap_or("Bounce");
            snap.suppressions.push(ApiSuppression {
                address: email.to_string(),
                reason: format!("{kind} (Postmark)"),
            });
        }
        offset += PAGE;
        if offset >= MAX_OFFSET {
            return;
        }
    }
}

pub async fn history(client: &ApiClient, mode: BodyMode) -> Result<(Vec<Value>, Vec<String>)> {
    let tokens = server_tokens(client)
        .await
        .context("listing Postmark servers for history")?;
    let mut messages = Vec::new();
    let mut notes = Vec::new();
    for token in &tokens {
        let mut offset = 0usize;
        loop {
            let url = format!(
                "{}/messages/outbound?count={PAGE}&offset={offset}",
                client.base()
            );
            let data = match client
                .get_json(&url, Auth::Header(SERVER_HEADER, token))
                .await
            {
                Ok(data) => data,
                Err(error) => {
                    notes.push(format!("history batch failed: {error}"));
                    break;
                }
            };
            let Some(items) = data.get("Messages").and_then(Value::as_array) else {
                break;
            };
            if items.is_empty() {
                break;
            }
            for item in items {
                messages.push(build_message(client, token, item, mode).await);
            }
            offset += PAGE;
            if offset >= MAX_OFFSET {
                notes.push(
                    "Postmark limits paging to 10,000 messages per server; older messages were not \
                     read"
                        .to_string(),
                );
                break;
            }
        }
    }
    Ok((messages, notes))
}

async fn build_message(client: &ApiClient, token: &str, item: &Value, mode: BodyMode) -> Value {
    let from = str_field(item, "From").unwrap_or_default();
    let to = item
        .get("Recipients")
        .and_then(Value::as_array)
        .and_then(|a| a.first())
        .and_then(Value::as_str)
        .map(str::to_string)
        .or_else(|| str_field(item, "To"))
        .unwrap_or_default();
    let subject = str_field(item, "Subject").unwrap_or_default();
    let message_id = str_field(item, "MessageID").unwrap_or_default();
    let timestamp = str_field(item, "ReceivedAt").unwrap_or_default();
    let status = map_status(item.get("Status").and_then(Value::as_str).unwrap_or(""));

    // Only fetch bodies in full mode, and best effort.
    let body = if mode == BodyMode::Full {
        let id = item.get("MessageID").and_then(Value::as_str).unwrap_or("");
        let url = format!("{}/messages/outbound/{id}/details", client.base());
        client
            .get_json(&url, Auth::Header(SERVER_HEADER, token))
            .await
            .ok()
            .and_then(|d| str_field(&d, "HtmlBody").or_else(|| str_field(&d, "TextBody")))
    } else {
        None
    };

    let raw = synth_raw(&from, &to, &subject, &message_id, body.as_deref(), mode);
    json!({
        "scope": "outgoing",
        "mail_from": from,
        "rcpt_to": to,
        "raw_message_base64": raw,
        "bounce": status == "Bounced",
        "timestamp": timestamp,
        "deliveries": [{ "status": status, "timestamp": timestamp }],
        "opens": [],
        "clicks": [],
    })
}

/// Map a Postmark outbound status onto CamelMailer's delivery statuses.
fn map_status(status: &str) -> &'static str {
    match status {
        "Sent" | "Delivered" | "Processed" | "Transient" => "Sent",
        "Bounced" => "Bounced",
        "Queued" | "Scheduled" => "Held",
        _ => "Held",
    }
}

fn str_field(v: &Value, key: &str) -> Option<String> {
    v.get(key)
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
}
