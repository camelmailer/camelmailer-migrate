//! Reads a Postal server's message history from its per-server message
//! database (`{prefix}-server-{id}`) and turns each message into the JSON the
//! CamelMailer bulk-import endpoint accepts. Nothing here sends mail; it only
//! reads Postal and prepares completed, historical records.
//!
//! Postal stores the raw mail split across a dynamically named `raw_table`
//! (`messages.raw_table`, with `raw_headers_id` / `raw_body_id`), and the full
//! message is `headers + "\r\n\r\n" + body`. The three body modes decide how
//! much of that comes across.

use std::collections::HashMap;

use anyhow::{Context, Result};
use base64::Engine;
use serde_json::{json, Value};
use sqlx::mysql::MySqlPoolOptions;
use sqlx::{MySql, Pool, Row};
use url::Url;

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum BodyMode {
    /// Full raw message (headers + body).
    Full,
    /// Headers only, empty body.
    Headers,
    /// No raw content: synthesize minimal headers (From/To/Subject/Message-ID)
    /// from the metadata so the message lists and is searchable.
    Index,
}

impl BodyMode {
    pub fn parse(s: &str) -> Result<Self> {
        match s {
            "full" => Ok(BodyMode::Full),
            "headers" => Ok(BodyMode::Headers),
            "index" | "none" => Ok(BodyMode::Index),
            other => {
                anyhow::bail!("--history-bodies must be full, headers or index (got {other:?})")
            }
        }
    }
}

/// Map a Postal delivery/message status onto CamelMailer's five delivery
/// statuses (`Sent`, `SoftFail`, `HardFail`, `Held`, `Bounced`).
fn map_status(postal: &str) -> &'static str {
    match postal {
        "Sent" | "Processed" | "Delivered" => "Sent",
        "SoftFail" => "SoftFail",
        "HardFail" => "HardFail",
        "Bounced" => "Bounced",
        _ => "Held",
    }
}

/// Build the connection URL for a server's message database by swapping the
/// database name in the base Postal URL.
fn message_db_url(base: &str, prefix: &str, server_id: i64) -> Result<String> {
    let mut url = Url::parse(base).context("parsing the Postal database URL")?;
    url.set_path(&format!("/{prefix}-server-{server_id}"));
    Ok(url.to_string())
}

pub async fn connect(base: &str, prefix: &str, server_id: i64) -> Result<Pool<MySql>> {
    let url = message_db_url(base, prefix, server_id)?;
    MySqlPoolOptions::new()
        .max_connections(2)
        .connect(&url)
        .await
        .with_context(|| format!("connecting to message database {prefix}-server-{server_id}"))
}

/// A Postal table name is safe to interpolate only if it is a plain
/// identifier. Postal generates these itself, but validate anyway.
fn safe_table(name: &str) -> bool {
    !name.is_empty() && name.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_')
}

async fn fetch_raw(pool: &Pool<MySql>, table: &str, id: i64) -> Vec<u8> {
    if !safe_table(table) {
        return Vec::new();
    }
    let query = format!("SELECT data FROM `{table}` WHERE id = ?");
    match sqlx::query(&query).bind(id).fetch_optional(pool).await {
        Ok(Some(row)) => row.try_get::<Vec<u8>, _>("data").unwrap_or_default(),
        _ => Vec::new(),
    }
}

fn synth_headers(mail_from: &str, rcpt_to: &str, subject: &str, message_id: &str) -> Vec<u8> {
    let mut h = format!("From: {mail_from}\r\nTo: {rcpt_to}\r\nSubject: {subject}\r\n");
    if !message_id.is_empty() {
        h.push_str(&format!("Message-ID: <{message_id}>\r\n"));
    }
    h.push_str("\r\n");
    h.into_bytes()
}

fn group_by_message<F>(rows: Vec<sqlx::mysql::MySqlRow>, mut build: F) -> HashMap<i64, Vec<Value>>
where
    F: FnMut(&sqlx::mysql::MySqlRow) -> Option<Value>,
{
    let mut map: HashMap<i64, Vec<Value>> = HashMap::new();
    for row in &rows {
        let mid = row.try_get::<i64, _>("mid").unwrap_or_default();
        if let Some(value) = build(row) {
            map.entry(mid).or_default().push(value);
        }
    }
    map
}

/// Read every message in this server's message database and return one
/// import-ready JSON object per message.
pub async fn read_messages(pool: &Pool<MySql>, mode: BodyMode) -> Result<Vec<Value>> {
    // Deliveries, opens and clicks, grouped by message id up front so the
    // per-message loop stays cheap.
    let deliveries = group_by_message(
        sqlx::query(
            "SELECT CAST(message_id AS SIGNED) AS mid, status, output, details, \
                    CAST(sent_with_ssl AS SIGNED) AS sent_ssl, CAST(timestamp AS DOUBLE) AS ts \
             FROM deliveries",
        )
        .fetch_all(pool)
        .await
        .context("reading deliveries")?,
        |r| {
            Some(json!({
                "status": map_status(&r.try_get::<Option<String>, _>("status").ok().flatten().unwrap_or_default()),
                "details": r.try_get::<Option<String>, _>("details").ok().flatten(),
                "output": r.try_get::<Option<String>, _>("output").ok().flatten(),
                "sent_with_ssl": r.try_get::<i64, _>("sent_ssl").unwrap_or(0) != 0,
                "timestamp": r.try_get::<f64, _>("ts").unwrap_or(0.0) as i64,
            }))
        },
    );

    let opens = group_by_message(
        sqlx::query(
            "SELECT CAST(message_id AS SIGNED) AS mid, ip_address, \
                    CAST(timestamp AS DOUBLE) AS ts FROM loads",
        )
        .fetch_all(pool)
        .await
        .context("reading opens")?,
        |r| {
            Some(json!({
                "timestamp": r.try_get::<f64, _>("ts").unwrap_or(0.0) as i64,
                "ip": r.try_get::<Option<String>, _>("ip_address").ok().flatten(),
                "user_agent": Value::Null,
            }))
        },
    );

    let clicks = group_by_message(
        sqlx::query(
            "SELECT CAST(c.message_id AS SIGNED) AS mid, l.url AS url, \
                    CAST(c.timestamp AS DOUBLE) AS ts \
             FROM clicks c LEFT JOIN links l ON l.id = c.link_id",
        )
        .fetch_all(pool)
        .await
        .context("reading clicks")?,
        |r| {
            let url = r
                .try_get::<Option<String>, _>("url")
                .ok()
                .flatten()
                .unwrap_or_default();
            if url.is_empty() {
                return None;
            }
            Some(
                json!({ "url": url, "timestamp": r.try_get::<f64, _>("ts").unwrap_or(0.0) as i64 }),
            )
        },
    );

    let rows = sqlx::query(
        "SELECT CAST(id AS SIGNED) AS id, scope, rcpt_to, mail_from, subject, message_id, \
                CAST(timestamp AS DOUBLE) AS ts, status, \
                CAST(bounce AS SIGNED) AS bounce, CAST(received_with_ssl AS SIGNED) AS sent_ssl, \
                CAST(loaded AS DOUBLE) AS loaded, tag, raw_table, \
                CAST(raw_headers_id AS SIGNED) AS rhid, CAST(raw_body_id AS SIGNED) AS rbid \
         FROM messages",
    )
    .fetch_all(pool)
    .await
    .context("reading messages")?;

    let mut out = Vec::with_capacity(rows.len());
    for row in rows {
        let id = row.try_get::<i64, _>("id").unwrap_or_default();
        let mail_from = row
            .try_get::<Option<String>, _>("mail_from")
            .ok()
            .flatten()
            .unwrap_or_default();
        let rcpt_to = row
            .try_get::<Option<String>, _>("rcpt_to")
            .ok()
            .flatten()
            .unwrap_or_default();
        let subject = row
            .try_get::<Option<String>, _>("subject")
            .ok()
            .flatten()
            .unwrap_or_default();
        let message_id = row
            .try_get::<Option<String>, _>("message_id")
            .ok()
            .flatten()
            .unwrap_or_default();
        let scope = match row
            .try_get::<Option<String>, _>("scope")
            .ok()
            .flatten()
            .as_deref()
        {
            Some("incoming") => "incoming",
            _ => "outgoing",
        };
        let ts = row.try_get::<f64, _>("ts").unwrap_or(0.0) as i64;

        // Raw content per body mode.
        let raw_table = row
            .try_get::<Option<String>, _>("raw_table")
            .ok()
            .flatten()
            .unwrap_or_default();
        let raw: Vec<u8> = match mode {
            BodyMode::Index => synth_headers(&mail_from, &rcpt_to, &subject, &message_id),
            BodyMode::Headers | BodyMode::Full if raw_table.is_empty() => {
                synth_headers(&mail_from, &rcpt_to, &subject, &message_id)
            }
            BodyMode::Headers => {
                let hid = row.try_get::<i64, _>("rhid").unwrap_or_default();
                let mut h = fetch_raw(pool, &raw_table, hid).await;
                h.extend_from_slice(b"\r\n\r\n");
                h
            }
            BodyMode::Full => {
                let hid = row.try_get::<i64, _>("rhid").unwrap_or_default();
                let bid = row.try_get::<i64, _>("rbid").unwrap_or_default();
                let mut h = fetch_raw(pool, &raw_table, hid).await;
                h.extend_from_slice(b"\r\n\r\n");
                h.extend_from_slice(&fetch_raw(pool, &raw_table, bid).await);
                h
            }
        };
        let raw = if raw.is_empty() {
            synth_headers(&mail_from, &rcpt_to, &subject, &message_id)
        } else {
            raw
        };

        // Deliveries: use Postal's rows, or synthesize one from the message
        // status so the message has a terminal state.
        let mut delivery_list = deliveries.get(&id).cloned().unwrap_or_default();
        if delivery_list.is_empty() {
            let status = map_status(
                &row.try_get::<Option<String>, _>("status")
                    .ok()
                    .flatten()
                    .unwrap_or_default(),
            );
            delivery_list.push(json!({
                "status": status,
                "details": Value::Null,
                "output": Value::Null,
                "sent_with_ssl": row.try_get::<i64, _>("sent_ssl").unwrap_or(0) != 0,
                "timestamp": ts,
            }));
        }

        // Opens: the detailed rows, or a single open from the message-level
        // `loaded` timestamp when there are none.
        let mut open_list = opens.get(&id).cloned().unwrap_or_default();
        if open_list.is_empty() {
            if let Ok(loaded) = row.try_get::<f64, _>("loaded") {
                if loaded > 0.0 {
                    open_list.push(json!({ "timestamp": loaded as i64, "ip": Value::Null, "user_agent": Value::Null }));
                }
            }
        }

        out.push(json!({
            "scope": scope,
            "mail_from": mail_from,
            "rcpt_to": rcpt_to,
            "raw_message_base64": base64::engine::general_purpose::STANDARD.encode(&raw),
            "received_with_ssl": row.try_get::<i64, _>("sent_ssl").unwrap_or(0) != 0,
            "bounce": row.try_get::<i64, _>("bounce").unwrap_or(0) != 0,
            "tag": row.try_get::<Option<String>, _>("tag").ok().flatten(),
            "timestamp": ts,
            "deliveries": delivery_list,
            "opens": open_list,
            "clicks": clicks.get(&id).cloned().unwrap_or_default(),
        }));
    }

    Ok(out)
}
