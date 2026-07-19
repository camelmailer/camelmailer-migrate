//! Resend source, read over the HTTP API with `Authorization: Bearer`.
//!
//! Resend exposes domains, audiences with their contacts, and broadcasts.
//! CamelMailer has no create-broadcast API on the admin surface, so audiences
//! and broadcasts are reported as a manual follow-up (recreate them as
//! broadcast streams); the one piece that maps cleanly is an unsubscribed
//! contact, which becomes a CamelMailer suppression. Email history is read
//! from the List Sent Emails endpoint, which returns metadata only.
//!
//! Not portable over the API: the sending API key and DKIM private key. The
//! tool mints a fresh credential and DKIM key instead.

use std::collections::HashSet;

use anyhow::Result;
use serde_json::{json, Value};

use super::{synth_raw, ApiClient, ApiDomain, ApiSnapshot, ApiSuppression, Auth};
use crate::history::BodyMode;

fn auth(client: &ApiClient) -> Auth<'_> {
    Auth::Bearer(client.api_key())
}

pub async fn snapshot(client: &ApiClient) -> Result<ApiSnapshot> {
    let mut snap = ApiSnapshot::default();

    let url = format!("{}/domains", client.base());
    let data = client.get_json(&url, auth(client)).await?;
    if let Some(domains) = data.get("data").and_then(Value::as_array) {
        for d in domains {
            let name = d.get("name").and_then(Value::as_str).unwrap_or_default();
            if name.is_empty() {
                continue;
            }
            let verified = d.get("status").and_then(Value::as_str) == Some("verified");
            snap.domains.push(ApiDomain {
                name: name.to_string(),
                verified,
            });
        }
    }

    // Audiences and their contacts. Unsubscribed contacts become suppressions;
    // the audiences themselves are a manual follow-up (broadcast streams).
    let mut audience_count = 0usize;
    let mut contact_count = 0usize;
    let mut suppressed: HashSet<String> = HashSet::new();
    let url = format!("{}/audiences", client.base());
    match client.get_json(&url, auth(client)).await {
        Ok(data) => {
            if let Some(audiences) = data.get("data").and_then(Value::as_array) {
                for audience in audiences {
                    audience_count += 1;
                    let Some(id) = audience.get("id").and_then(Value::as_str) else {
                        continue;
                    };
                    let url = format!("{}/audiences/{id}/contacts", client.base());
                    if let Ok(contacts) = client.get_json(&url, auth(client)).await {
                        if let Some(list) = contacts.get("data").and_then(Value::as_array) {
                            for contact in list {
                                contact_count += 1;
                                let email = contact
                                    .get("email")
                                    .and_then(Value::as_str)
                                    .unwrap_or_default();
                                let unsubscribed = contact
                                    .get("unsubscribed")
                                    .and_then(Value::as_bool)
                                    .unwrap_or(false);
                                if unsubscribed
                                    && !email.is_empty()
                                    && suppressed.insert(email.to_lowercase())
                                {
                                    snap.suppressions.push(ApiSuppression {
                                        address: email.to_string(),
                                        reason: "Unsubscribed audience contact (Resend)"
                                            .to_string(),
                                    });
                                }
                            }
                        }
                    }
                }
            }
        }
        Err(error) => snap
            .notes
            .push(format!("could not read audiences/contacts: {error}")),
    }
    if audience_count > 0 {
        snap.notes.push(format!(
            "{audience_count} audience(s) with {contact_count} contact(s): recreate them as \
             CamelMailer broadcast streams and import the opted-in contacts as subscribers \
             (unsubscribed contacts are carried over as suppressions)"
        ));
    }

    // Broadcasts are informational: no admin create-broadcast API.
    let url = format!("{}/broadcasts", client.base());
    if let Ok(data) = client.get_json(&url, auth(client)).await {
        if let Some(count) = data.get("data").and_then(Value::as_array).map(Vec::len) {
            if count > 0 {
                snap.notes.push(format!(
                    "{count} broadcast(s) exist in Resend; recreate the content in CamelMailer \
                     broadcast streams (the API does not expose a portable broadcast object)"
                ));
            }
        }
    }

    snap.notes.push(
        "Resend has no server-side stored templates API; if you use React Email, keep rendering in \
         your app. The sending API key and DKIM private key are not readable over the API; a fresh \
         CamelMailer credential and DKIM key are created instead"
            .to_string(),
    );
    Ok(snap)
}

pub async fn history(client: &ApiClient, mode: BodyMode) -> Result<(Vec<Value>, Vec<String>)> {
    let mut messages = Vec::new();
    let mut notes = Vec::new();
    let mut after: Option<String> = None;
    loop {
        let mut url = format!("{}/emails?limit=100", client.base());
        if let Some(cursor) = &after {
            url.push_str(&format!("&after={cursor}"));
        }
        let data = match client.get_json(&url, auth(client)).await {
            Ok(data) => data,
            Err(error) => {
                notes.push(format!(
                    "email history is limited on Resend and could not be read: {error}"
                ));
                break;
            }
        };
        let Some(items) = data.get("data").and_then(Value::as_array) else {
            break;
        };
        if items.is_empty() {
            break;
        }
        for item in items {
            messages.push(build_message(item, mode));
        }
        let has_more = data
            .get("has_more")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        after = items
            .last()
            .and_then(|m| m.get("id"))
            .and_then(Value::as_str)
            .map(str::to_string);
        if !has_more || after.is_none() {
            break;
        }
    }
    Ok((messages, notes))
}

fn build_message(item: &Value, mode: BodyMode) -> Value {
    let from = item.get("from").and_then(Value::as_str).unwrap_or_default();
    let to = item
        .get("to")
        .and_then(Value::as_array)
        .and_then(|a| a.first())
        .and_then(Value::as_str)
        .or_else(|| item.get("to").and_then(Value::as_str))
        .unwrap_or_default();
    let subject = item
        .get("subject")
        .and_then(Value::as_str)
        .unwrap_or_default();
    let message_id = item
        .get("message_id")
        .and_then(Value::as_str)
        .or_else(|| item.get("id").and_then(Value::as_str))
        .unwrap_or_default();
    let timestamp = item
        .get("created_at")
        .and_then(Value::as_str)
        .unwrap_or_default();
    let status = map_status(item.get("last_event").and_then(Value::as_str).unwrap_or(""));

    let raw = synth_raw(from, to, subject, message_id, None, mode);
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

fn map_status(event: &str) -> &'static str {
    match event {
        "delivered" | "sent" | "opened" | "clicked" => "Sent",
        "bounced" => "Bounced",
        "complained" => "HardFail",
        "delivery_delayed" => "SoftFail",
        _ => "Held",
    }
}
