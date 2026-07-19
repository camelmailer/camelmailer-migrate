//! Mailgun source, read over the HTTP API with HTTP basic auth (`api:KEY`).
//!
//! Domains are `/v4`; routes, suppressions, templates and events are `/v3`.
//! Routes are account-global; suppressions, templates and message events are
//! per domain. Cursor paging follows the `paging.next` URL until a page comes
//! back empty. The base URL is US by default (`--source-region eu` or
//! `--source-base-url` selects the EU host).
//!
//! Not portable over the API: the sending API key and DKIM private key. The
//! tool mints a fresh credential and DKIM key instead.

use std::collections::HashSet;

use anyhow::Result;
use serde_json::{json, Value};

use super::{
    permalink, synth_raw, ApiClient, ApiDomain, ApiRoute, ApiSnapshot, ApiSuppression, ApiTemplate,
    Auth,
};
use crate::history::BodyMode;

fn auth(client: &ApiClient) -> Auth<'_> {
    Auth::Basic("api", client.api_key())
}

pub async fn snapshot(client: &ApiClient) -> Result<ApiSnapshot> {
    let mut snap = ApiSnapshot::default();

    // Domains (/v4).
    let url = format!("{}/v4/domains?limit=1000", client.base());
    let data = client.get_json(&url, auth(client)).await?;
    let mut domain_names = Vec::new();
    if let Some(items) = data.get("items").and_then(Value::as_array) {
        for d in items {
            let name = d.get("name").and_then(Value::as_str).unwrap_or_default();
            if name.is_empty() {
                continue;
            }
            let verified = d.get("state").and_then(Value::as_str) == Some("active");
            domain_names.push(name.to_string());
            snap.domains.push(ApiDomain {
                name: name.to_string(),
                verified,
            });
        }
    }

    // Routes (account-global, /v3).
    let url = format!("{}/v3/routes?limit=1000", client.base());
    match client.get_json(&url, auth(client)).await {
        Ok(data) => {
            if let Some(items) = data.get("items").and_then(Value::as_array) {
                for r in items {
                    snap.routes.push(map_route(r));
                }
            }
        }
        Err(error) => snap.notes.push(format!("could not read routes: {error}")),
    }

    // Per-domain suppressions and templates.
    let mut suppressed: HashSet<String> = HashSet::new();
    for domain in &domain_names {
        for kind in ["bounces", "unsubscribes", "complaints"] {
            collect_suppressions(client, domain, kind, &mut snap, &mut suppressed).await;
        }
        collect_templates(client, domain, &mut snap).await;
    }

    snap.notes.push(
        "Mailgun API keys and DKIM private keys are not readable over the API; a fresh CamelMailer \
         credential and DKIM key are created instead"
            .to_string(),
    );
    Ok(snap)
}

/// Map a Mailgun route (expression + actions) onto a CamelMailer route.
fn map_route(r: &Value) -> ApiRoute {
    let description = r
        .get("description")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty());
    let expression = r.get("expression").and_then(Value::as_str).unwrap_or("");
    let name = description
        .map(str::to_string)
        .unwrap_or_else(|| expression.to_string());
    let actions: Vec<&str> = r
        .get("actions")
        .and_then(Value::as_array)
        .map(|a| a.iter().filter_map(Value::as_str).collect())
        .unwrap_or_default();

    // A forward(...) to an http(s) URL becomes an Endpoint route; otherwise a
    // store()/stop() maps to the closest mode.
    let endpoint_url = actions.iter().find_map(|a| extract_url(a));
    let mode = if endpoint_url.is_some() {
        "Endpoint"
    } else if actions.iter().any(|a| a.starts_with("stop")) {
        "Reject"
    } else {
        "Accept"
    };
    ApiRoute {
        name: if name.is_empty() {
            "route".to_string()
        } else {
            name
        },
        domain: None,
        mode: mode.to_string(),
        endpoint_url,
    }
}

/// Pull an http(s) URL out of a Mailgun action string like `forward("...")`.
fn extract_url(action: &str) -> Option<String> {
    let start = action.find("http")?;
    let tail = &action[start..];
    let end = tail.find(['"', '\'', ')']).unwrap_or(tail.len());
    Some(tail[..end].to_string())
}

async fn collect_suppressions(
    client: &ApiClient,
    domain: &str,
    kind: &str,
    snap: &mut ApiSnapshot,
    seen: &mut HashSet<String>,
) {
    let reason = match kind {
        "bounces" => "Bounce (Mailgun)",
        "unsubscribes" => "Unsubscribe (Mailgun)",
        _ => "Complaint (Mailgun)",
    };
    let mut url = format!("{}/v3/{domain}/{kind}?limit=1000", client.base());
    loop {
        let data = match client.get_json(&url, auth(client)).await {
            Ok(data) => data,
            Err(error) => {
                snap.notes
                    .push(format!("could not read {kind} for {domain}: {error}"));
                return;
            }
        };
        let Some(items) = data.get("items").and_then(Value::as_array) else {
            return;
        };
        if items.is_empty() {
            return;
        }
        for item in items {
            let address = item
                .get("address")
                .and_then(Value::as_str)
                .unwrap_or_default();
            if address.is_empty() || !seen.insert(address.to_lowercase()) {
                continue;
            }
            snap.suppressions.push(ApiSuppression {
                address: address.to_string(),
                reason: reason.to_string(),
            });
        }
        match next_page(&data) {
            Some(next) if next != url => url = next,
            _ => return,
        }
    }
}

async fn collect_templates(client: &ApiClient, domain: &str, snap: &mut ApiSnapshot) {
    let url = format!("{}/v3/{domain}/templates?limit=100", client.base());
    let data = match client.get_json(&url, auth(client)).await {
        Ok(data) => data,
        Err(error) => {
            snap.notes
                .push(format!("could not read templates for {domain}: {error}"));
            return;
        }
    };
    let Some(items) = data.get("items").and_then(Value::as_array) else {
        return;
    };
    for item in items {
        let name = item.get("name").and_then(Value::as_str).unwrap_or_default();
        if name.is_empty() {
            continue;
        }
        // Fetch the active version's content.
        let content_url = format!("{}/v3/{domain}/templates/{name}?active=yes", client.base());
        let html = client
            .get_json(&content_url, auth(client))
            .await
            .ok()
            .and_then(|d| {
                d.get("template")
                    .and_then(|t| t.get("version"))
                    .and_then(|v| v.get("template"))
                    .and_then(Value::as_str)
                    .map(str::to_string)
            });
        snap.templates.push(ApiTemplate {
            name: name.to_string(),
            permalink: permalink(name),
            subject: None,
            html_body: html,
            text_body: None,
        });
    }
}

pub async fn history(
    client: &ApiClient,
    domains: &[String],
    mode: BodyMode,
) -> Result<(Vec<Value>, Vec<String>)> {
    let mut messages = Vec::new();
    let mut notes = Vec::new();
    for domain in domains {
        // event -> CamelMailer status; accepted seeds the record, delivered and
        // failed upgrade it. Grouped by message-id to avoid duplicate records.
        let mut order: Vec<String> = Vec::new();
        let mut by_id: std::collections::HashMap<String, Value> = std::collections::HashMap::new();
        for (event, status) in [
            ("accepted", "Held"),
            ("delivered", "Sent"),
            ("failed", "HardFail"),
        ] {
            let mut url = format!(
                "{}/v3/{domain}/events?limit=300&ascending=yes&event={event}",
                client.base()
            );
            loop {
                let data = match client.get_json(&url, auth(client)).await {
                    Ok(data) => data,
                    Err(error) => {
                        notes.push(format!("could not read events for {domain}: {error}"));
                        break;
                    }
                };
                let Some(items) = data.get("items").and_then(Value::as_array) else {
                    break;
                };
                if items.is_empty() {
                    break;
                }
                for item in items {
                    accumulate_event(item, status, mode, &mut order, &mut by_id);
                }
                match next_page(&data) {
                    Some(next) if next != url => url = next,
                    _ => break,
                }
            }
        }
        for id in order {
            if let Some(msg) = by_id.remove(&id) {
                messages.push(msg);
            }
        }
    }
    Ok((messages, notes))
}

fn accumulate_event(
    item: &Value,
    status: &str,
    mode: BodyMode,
    order: &mut Vec<String>,
    by_id: &mut std::collections::HashMap<String, Value>,
) {
    let headers = item.get("message").and_then(|m| m.get("headers"));
    let message_id = headers
        .and_then(|h| h.get("message-id"))
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string();
    if message_id.is_empty() {
        return;
    }
    // Refine the failed status by severity when present.
    let status = if status == "HardFail" {
        match item.get("severity").and_then(Value::as_str) {
            Some("temporary") => "SoftFail",
            _ => "HardFail",
        }
    } else {
        status
    };
    let timestamp = item
        .get("timestamp")
        .and_then(Value::as_f64)
        .map(|t| t as i64)
        .unwrap_or(0);

    if let Some(existing) = by_id.get_mut(&message_id) {
        // Upgrade the delivery status; accepted (Held) is the weakest.
        existing["deliveries"] = json!([{ "status": status, "timestamp": timestamp }]);
        existing["bounce"] = json!(status == "Bounced");
        return;
    }

    let from = headers
        .and_then(|h| h.get("from"))
        .and_then(Value::as_str)
        .unwrap_or_default();
    let to = headers
        .and_then(|h| h.get("to"))
        .and_then(Value::as_str)
        .or_else(|| item.get("recipient").and_then(Value::as_str))
        .unwrap_or_default();
    let subject = headers
        .and_then(|h| h.get("subject"))
        .and_then(Value::as_str)
        .unwrap_or_default();
    let raw = synth_raw(from, to, subject, &message_id, None, mode);
    order.push(message_id.clone());
    by_id.insert(
        message_id,
        json!({
            "scope": "outgoing",
            "mail_from": from,
            "rcpt_to": to,
            "raw_message_base64": raw,
            "bounce": false,
            "timestamp": timestamp,
            "deliveries": [{ "status": status, "timestamp": timestamp }],
            "opens": [],
            "clicks": [],
        }),
    );
}

/// The `paging.next` cursor URL, if the page reported one.
fn next_page(data: &Value) -> Option<String> {
    data.get("paging")
        .and_then(|p| p.get("next"))
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
}
