//! SendGrid (Twilio SendGrid) source, read over the v3 HTTP API with
//! `Authorization: Bearer`.
//!
//! Reads authenticated domains, dynamic templates (with their active version's
//! content), and the full suppression surface: global bounces, blocks, spam
//! reports and unsubscribes, plus per-group unsubscribes. Email Activity is
//! read for `--history` but requires the Email Activity add-on; the tool
//! degrades gracefully (a note, no error) when it is not enabled.
//!
//! Not portable over the API: the sending API key and DKIM private key. The
//! tool mints a fresh credential and DKIM key instead.

use std::collections::HashSet;

use anyhow::Result;
use serde_json::{json, Value};

use super::{
    permalink, synth_raw, ApiClient, ApiDomain, ApiSnapshot, ApiSuppression, ApiTemplate, Auth,
};
use crate::history::BodyMode;

const PAGE: usize = 500;

fn auth(client: &ApiClient) -> Auth<'_> {
    Auth::Bearer(client.api_key())
}

pub async fn snapshot(client: &ApiClient) -> Result<ApiSnapshot> {
    let mut snap = ApiSnapshot::default();

    // Authenticated domains: the response is a bare JSON array.
    let url = format!("{}/v3/whitelabel/domains?limit=100", client.base());
    let data = client.get_json(&url, auth(client)).await?;
    if let Some(domains) = data.as_array() {
        for d in domains {
            let name = d.get("domain").and_then(Value::as_str).unwrap_or_default();
            if name.is_empty() {
                continue;
            }
            let verified = d.get("valid").and_then(Value::as_bool).unwrap_or(false);
            snap.domains.push(ApiDomain {
                name: name.to_string(),
                verified,
            });
        }
    }

    collect_templates(client, &mut snap).await;
    collect_suppressions(client, &mut snap).await;

    snap.notes.push(
        "SendGrid API keys and DKIM private keys are not readable over the API; a fresh CamelMailer \
         credential and DKIM key are created instead"
            .to_string(),
    );
    Ok(snap)
}

async fn collect_templates(client: &ApiClient, snap: &mut ApiSnapshot) {
    let url = format!(
        "{}/v3/templates?generations=dynamic&page_size=200",
        client.base()
    );
    let data = match client.get_json(&url, auth(client)).await {
        Ok(data) => data,
        Err(error) => {
            snap.notes
                .push(format!("could not read templates: {error}"));
            return;
        }
    };
    let Some(items) = data.get("result").and_then(Value::as_array) else {
        return;
    };
    let mut seen: HashSet<String> = HashSet::new();
    for item in items {
        let Some(id) = item.get("id").and_then(Value::as_str) else {
            continue;
        };
        let name = item
            .get("name")
            .and_then(Value::as_str)
            .unwrap_or("template")
            .to_string();
        let link = permalink(&name);
        if !seen.insert(link.clone()) {
            continue;
        }
        // Fetch the template to reach the active version's content.
        let detail_url = format!("{}/v3/templates/{id}", client.base());
        let (subject, html, text) = match client.get_json(&detail_url, auth(client)).await {
            Ok(detail) => active_version(&detail),
            Err(_) => (None, None, None),
        };
        snap.templates.push(ApiTemplate {
            name,
            permalink: link,
            subject,
            html_body: html,
            text_body: text,
        });
    }
}

/// Pick the active version's subject and content, falling back to the first.
fn active_version(detail: &Value) -> (Option<String>, Option<String>, Option<String>) {
    let versions = detail.get("versions").and_then(Value::as_array);
    let Some(versions) = versions else {
        return (None, None, None);
    };
    let chosen = versions
        .iter()
        .find(|v| {
            matches!(v.get("active").and_then(Value::as_i64), Some(1))
                || v.get("active").and_then(Value::as_bool) == Some(true)
        })
        .or_else(|| versions.first());
    match chosen {
        Some(v) => (
            str_field(v, "subject"),
            str_field(v, "html_content"),
            str_field(v, "plain_content"),
        ),
        None => (None, None, None),
    }
}

async fn collect_suppressions(client: &ApiClient, snap: &mut ApiSnapshot) {
    let mut seen: HashSet<String> = HashSet::new();
    for (path, reason) in [
        ("suppression/bounces", "Bounce (SendGrid)"),
        ("suppression/blocks", "Block (SendGrid)"),
        ("suppression/spam_reports", "Spam report (SendGrid)"),
        ("suppression/unsubscribes", "Unsubscribe (SendGrid)"),
    ] {
        let mut offset = 0usize;
        loop {
            let url = format!("{}/v3/{path}?limit={PAGE}&offset={offset}", client.base());
            let data = match client.get_json(&url, auth(client)).await {
                Ok(data) => data,
                Err(error) => {
                    snap.notes.push(format!("could not read {path}: {error}"));
                    break;
                }
            };
            let Some(items) = data.as_array() else { break };
            if items.is_empty() {
                break;
            }
            for item in items {
                if let Some(email) = item.get("email").and_then(Value::as_str) {
                    if !email.is_empty() && seen.insert(email.to_lowercase()) {
                        snap.suppressions.push(ApiSuppression {
                            address: email.to_string(),
                            reason: reason.to_string(),
                        });
                    }
                }
            }
            if items.len() < PAGE {
                break;
            }
            offset += PAGE;
        }
    }

    // Per-group unsubscribes (ASM). Groups list, then emails per group.
    let url = format!("{}/v3/asm/groups", client.base());
    if let Ok(data) = client.get_json(&url, auth(client)).await {
        if let Some(groups) = data.as_array() {
            for group in groups {
                let Some(id) = group.get("id").and_then(Value::as_i64) else {
                    continue;
                };
                let name = group.get("name").and_then(Value::as_str).unwrap_or("group");
                let url = format!("{}/v3/asm/groups/{id}/suppressions", client.base());
                if let Ok(emails) = client.get_json(&url, auth(client)).await {
                    if let Some(list) = emails.as_array() {
                        for email in list.iter().filter_map(Value::as_str) {
                            if !email.is_empty() && seen.insert(email.to_lowercase()) {
                                snap.suppressions.push(ApiSuppression {
                                    address: email.to_string(),
                                    reason: format!("Group unsubscribe: {name} (SendGrid)"),
                                });
                            }
                        }
                    }
                }
            }
        }
    }
}

pub async fn history(client: &ApiClient, mode: BodyMode) -> Result<(Vec<Value>, Vec<String>)> {
    let mut messages = Vec::new();
    let mut notes = Vec::new();

    // The Email Activity feed requires a query and the add-on. Use a permissive
    // status filter; a 403 (no add-on) or any error degrades to a note.
    let query = url::form_urlencoded::byte_serialize(
        b"status=\"delivered\" OR status=\"not_delivered\" OR status=\"processed\"",
    )
    .collect::<String>();
    let url = format!("{}/v3/messages?limit=1000&query={query}", client.base());
    match client.get_json(&url, auth(client)).await {
        Ok(data) => {
            if let Some(items) = data.get("messages").and_then(Value::as_array) {
                for item in items {
                    messages.push(build_message(item, mode));
                }
            }
        }
        Err(error) => notes.push(format!(
            "SendGrid Email Activity was not read (it needs the Email Activity add-on and a \
             scoped key): {error}"
        )),
    }
    Ok((messages, notes))
}

fn build_message(item: &Value, mode: BodyMode) -> Value {
    let from = item
        .get("from_email")
        .and_then(Value::as_str)
        .unwrap_or_default();
    let to = item
        .get("to_email")
        .and_then(Value::as_str)
        .unwrap_or_default();
    let subject = item
        .get("subject")
        .and_then(Value::as_str)
        .unwrap_or_default();
    let message_id = item
        .get("msg_id")
        .and_then(Value::as_str)
        .unwrap_or_default();
    let timestamp = item
        .get("last_event_time")
        .and_then(Value::as_str)
        .unwrap_or_default();
    let status = map_status(item.get("status").and_then(Value::as_str).unwrap_or(""));

    // Opens carry over as a single synthesized event (the API exposes a count,
    // not per-open detail). Clicks have no URL in the activity feed, so they
    // are not synthesized.
    let opens = item.get("opens_count").and_then(Value::as_i64).unwrap_or(0);
    let open_list = if opens > 0 {
        json!([{ "timestamp": timestamp }])
    } else {
        json!([])
    };

    let raw = synth_raw(from, to, subject, message_id, None, mode);
    json!({
        "scope": "outgoing",
        "mail_from": from,
        "rcpt_to": to,
        "raw_message_base64": raw,
        "bounce": status == "Bounced",
        "timestamp": timestamp,
        "deliveries": [{ "status": status, "timestamp": timestamp }],
        "opens": open_list,
        "clicks": [],
    })
}

fn map_status(status: &str) -> &'static str {
    match status {
        "delivered" => "Sent",
        "not_delivered" => "HardFail",
        "processed" => "Held",
        _ => "Held",
    }
}

fn str_field(v: &Value, key: &str) -> Option<String> {
    v.get(key)
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
}
