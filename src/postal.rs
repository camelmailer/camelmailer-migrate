//! Reads a Postal installation straight from its MariaDB/MySQL database.
//!
//! Postal keeps all of its configuration (servers, domains with their DKIM
//! private keys, credentials, HTTP endpoints, webhooks, routes and IP pools)
//! in one `postal` database. There is no export API that covers this, so the
//! migration reads the tables directly. Only configuration is read; message
//! history lives in separate per-server databases and is not migrated.
//!
//! Every integer column is cast to `SIGNED` so a single `i64` reader works
//! regardless of the underlying `INT`/`BIGINT`/`TINYINT` width, and booleans
//! come back as 0/1.

use std::collections::HashMap;

use anyhow::{Context, Result};
use sqlx::mysql::MySqlPoolOptions;
use sqlx::{MySql, Pool, Row};

pub struct Postal {
    pool: Pool<MySql>,
}

#[derive(Debug, Clone)]
pub struct Org {
    pub id: i64,
    pub name: String,
    pub permalink: String,
}

#[derive(Debug, Clone)]
pub struct Server {
    pub id: i64,
    pub org_id: i64,
    pub name: String,
    pub permalink: String,
    pub mode: String,
}

#[derive(Debug, Clone)]
pub struct Domain {
    pub server_id: i64,
    pub name: String,
    pub dkim_private_key: Option<String>,
    pub verified: bool,
}

#[derive(Debug, Clone)]
pub struct Credential {
    pub server_id: i64,
    pub name: String,
    /// Postal credential type: `SMTP` or `API`.
    pub kind: String,
    pub key: String,
}

#[derive(Debug, Clone)]
pub struct Webhook {
    pub server_id: i64,
    pub name: String,
    pub url: String,
    pub all_events: bool,
    pub sign: bool,
    pub enabled: bool,
    pub events: Vec<String>,
}

#[derive(Debug, Clone)]
pub struct Route {
    pub server_id: i64,
    pub name: String,
    pub domain: Option<String>,
    /// Postal route mode: `Endpoint`, `Accept`, `Hold`, `Bounce`, `Reject`.
    pub mode: String,
    pub endpoint_type: Option<String>,
    pub endpoint_id: Option<i64>,
}

#[derive(Debug, Clone)]
pub struct IpPool {
    pub id: i64,
    pub name: String,
    pub default: bool,
}

#[derive(Debug, Clone)]
pub struct IpAddress {
    pub pool_id: i64,
    pub ipv4: Option<String>,
    pub ipv6: Option<String>,
    pub hostname: Option<String>,
}

/// Everything read from a Postal database, ready to map onto CamelMailer.
pub struct Snapshot {
    pub orgs: Vec<Org>,
    pub servers: Vec<Server>,
    pub domains: Vec<Domain>,
    pub credentials: Vec<Credential>,
    pub webhooks: Vec<Webhook>,
    pub routes: Vec<Route>,
    /// HTTP endpoint id -> URL, so a route to an endpoint resolves to its URL.
    pub http_endpoints: HashMap<i64, String>,
    pub ip_pools: Vec<IpPool>,
    pub ip_addresses: Vec<IpAddress>,
}

impl Postal {
    pub async fn connect(url: &str) -> Result<Self> {
        let pool = MySqlPoolOptions::new()
            .max_connections(4)
            .connect(url)
            .await
            .context("connecting to the Postal database")?;
        Ok(Self { pool })
    }

    pub async fn read(&self) -> Result<Snapshot> {
        let orgs = self.orgs().await?;
        let servers = self.servers().await?;
        let domains = self.domains().await?;
        let credentials = self.credentials().await?;
        let webhooks = self.webhooks().await?;
        let routes = self.routes().await?;
        let http_endpoints = self.http_endpoints().await?;
        let ip_pools = self.ip_pools().await?;
        let ip_addresses = self.ip_addresses().await?;
        Ok(Snapshot {
            orgs,
            servers,
            domains,
            credentials,
            webhooks,
            routes,
            http_endpoints,
            ip_pools,
            ip_addresses,
        })
    }

    async fn orgs(&self) -> Result<Vec<Org>> {
        let rows = sqlx::query(
            "SELECT CAST(id AS SIGNED) AS id, name, permalink \
             FROM organizations WHERE deleted_at IS NULL",
        )
        .fetch_all(&self.pool)
        .await
        .context("reading organizations")?;
        Ok(rows
            .into_iter()
            .map(|r| Org {
                id: r.get::<i64, _>("id"),
                name: r.get::<Option<String>, _>("name").unwrap_or_default(),
                permalink: r.get::<Option<String>, _>("permalink").unwrap_or_default(),
            })
            .collect())
    }

    async fn servers(&self) -> Result<Vec<Server>> {
        let rows = sqlx::query(
            "SELECT CAST(id AS SIGNED) AS id, \
                    CAST(organization_id AS SIGNED) AS org_id, \
                    name, permalink, mode \
             FROM servers WHERE deleted_at IS NULL",
        )
        .fetch_all(&self.pool)
        .await
        .context("reading servers")?;
        Ok(rows
            .into_iter()
            .map(|r| Server {
                id: r.get::<i64, _>("id"),
                org_id: r.get::<Option<i64>, _>("org_id").unwrap_or_default(),
                name: r.get::<Option<String>, _>("name").unwrap_or_default(),
                permalink: r.get::<Option<String>, _>("permalink").unwrap_or_default(),
                mode: r.get::<Option<String>, _>("mode").unwrap_or_default(),
            })
            .collect())
    }

    async fn domains(&self) -> Result<Vec<Domain>> {
        // A Postal domain belongs to a server directly (server_id) or through
        // the polymorphic owner (owner_type='Server'). Organization-owned
        // domains have no single server and are reported by the caller.
        let rows = sqlx::query(
            "SELECT name, dkim_private_key, owner_type, \
                    CAST(owner_id AS SIGNED) AS owner_id, \
                    CAST(server_id AS SIGNED) AS server_id, \
                    CAST(verified_at IS NOT NULL AS SIGNED) AS verified \
             FROM domains",
        )
        .fetch_all(&self.pool)
        .await
        .context("reading domains")?;
        Ok(rows
            .into_iter()
            .filter_map(|r| {
                let owner_type = r.get::<Option<String>, _>("owner_type").unwrap_or_default();
                let owner_id = r.get::<Option<i64>, _>("owner_id");
                let server_id = r.get::<Option<i64>, _>("server_id");
                let effective = if owner_type == "Server" {
                    owner_id.or(server_id)
                } else if owner_type.is_empty() {
                    server_id
                } else {
                    // Organization-owned or other: skip, no single server.
                    None
                }?;
                Some(Domain {
                    server_id: effective,
                    name: r.get::<Option<String>, _>("name").unwrap_or_default(),
                    dkim_private_key: r.get::<Option<String>, _>("dkim_private_key"),
                    verified: r.get::<i64, _>("verified") != 0,
                })
            })
            .filter(|d| !d.name.is_empty())
            .collect())
    }

    async fn credentials(&self) -> Result<Vec<Credential>> {
        let rows = sqlx::query(
            "SELECT CAST(server_id AS SIGNED) AS server_id, name, `type` AS kind, `key` \
             FROM credentials",
        )
        .fetch_all(&self.pool)
        .await
        .context("reading credentials")?;
        Ok(rows
            .into_iter()
            .filter_map(|r| {
                let key = r.get::<Option<String>, _>("key").unwrap_or_default();
                let server_id = r.get::<Option<i64>, _>("server_id")?;
                if key.is_empty() {
                    return None;
                }
                Some(Credential {
                    server_id,
                    name: r.get::<Option<String>, _>("name").unwrap_or_default(),
                    kind: r.get::<Option<String>, _>("kind").unwrap_or_default(),
                    key,
                })
            })
            .collect())
    }

    async fn webhooks(&self) -> Result<Vec<Webhook>> {
        let events_rows = sqlx::query(
            "SELECT CAST(webhook_id AS SIGNED) AS webhook_id, event FROM webhook_events",
        )
        .fetch_all(&self.pool)
        .await
        .context("reading webhook events")?;
        let mut by_webhook: HashMap<i64, Vec<String>> = HashMap::new();
        for r in events_rows {
            if let (Some(id), Some(event)) = (
                r.get::<Option<i64>, _>("webhook_id"),
                r.get::<Option<String>, _>("event"),
            ) {
                by_webhook.entry(id).or_default().push(event);
            }
        }

        let rows = sqlx::query(
            "SELECT CAST(id AS SIGNED) AS id, CAST(server_id AS SIGNED) AS server_id, \
                    name, url, \
                    CAST(all_events AS SIGNED) AS all_events, \
                    CAST(enabled AS SIGNED) AS enabled, \
                    CAST(sign AS SIGNED) AS sign_ \
             FROM webhooks",
        )
        .fetch_all(&self.pool)
        .await
        .context("reading webhooks")?;
        Ok(rows
            .into_iter()
            .filter_map(|r| {
                let id = r.get::<i64, _>("id");
                let url = r.get::<Option<String>, _>("url").unwrap_or_default();
                let server_id = r.get::<Option<i64>, _>("server_id")?;
                if url.is_empty() {
                    return None;
                }
                Some(Webhook {
                    server_id,
                    name: r.get::<Option<String>, _>("name").unwrap_or_default(),
                    url,
                    all_events: r.get::<i64, _>("all_events") != 0,
                    sign: r.get::<i64, _>("sign_") != 0,
                    enabled: r.get::<i64, _>("enabled") != 0,
                    events: by_webhook.remove(&id).unwrap_or_default(),
                })
            })
            .collect())
    }

    async fn routes(&self) -> Result<Vec<Route>> {
        // Join the route's domain name in directly so we do not need a second
        // lookup table. domain_id may be null for catch-all routes.
        let rows = sqlx::query(
            "SELECT CAST(r.server_id AS SIGNED) AS server_id, r.name, d.name AS domain, \
                    r.mode, r.endpoint_type, CAST(r.endpoint_id AS SIGNED) AS endpoint_id \
             FROM routes r LEFT JOIN domains d ON d.id = r.domain_id",
        )
        .fetch_all(&self.pool)
        .await
        .context("reading routes")?;
        Ok(rows
            .into_iter()
            .filter_map(|r| {
                let server_id = r.get::<Option<i64>, _>("server_id")?;
                Some(Route {
                    server_id,
                    name: r.get::<Option<String>, _>("name").unwrap_or_default(),
                    domain: r.get::<Option<String>, _>("domain"),
                    mode: r.get::<Option<String>, _>("mode").unwrap_or_default(),
                    endpoint_type: r.get::<Option<String>, _>("endpoint_type"),
                    endpoint_id: r.get::<Option<i64>, _>("endpoint_id"),
                })
            })
            .collect())
    }

    async fn http_endpoints(&self) -> Result<HashMap<i64, String>> {
        let rows = sqlx::query("SELECT CAST(id AS SIGNED) AS id, url FROM http_endpoints")
            .fetch_all(&self.pool)
            .await
            .context("reading HTTP endpoints")?;
        Ok(rows
            .into_iter()
            .filter_map(|r| {
                let url = r.get::<Option<String>, _>("url").unwrap_or_default();
                if url.is_empty() {
                    return None;
                }
                Some((r.get::<i64, _>("id"), url))
            })
            .collect())
    }

    async fn ip_pools(&self) -> Result<Vec<IpPool>> {
        let rows = sqlx::query(
            "SELECT CAST(id AS SIGNED) AS id, name, CAST(`default` AS SIGNED) AS is_default \
             FROM ip_pools",
        )
        .fetch_all(&self.pool)
        .await
        .context("reading IP pools")?;
        Ok(rows
            .into_iter()
            .map(|r| IpPool {
                id: r.get::<i64, _>("id"),
                name: r.get::<Option<String>, _>("name").unwrap_or_default(),
                default: r.get::<i64, _>("is_default") != 0,
            })
            .collect())
    }

    async fn ip_addresses(&self) -> Result<Vec<IpAddress>> {
        let rows = sqlx::query(
            "SELECT CAST(ip_pool_id AS SIGNED) AS pool_id, ipv4, ipv6, hostname FROM ip_addresses",
        )
        .fetch_all(&self.pool)
        .await
        .context("reading IP addresses")?;
        Ok(rows
            .into_iter()
            .filter_map(|r| {
                let pool_id = r.get::<Option<i64>, _>("pool_id")?;
                Some(IpAddress {
                    pool_id,
                    ipv4: r.get::<Option<String>, _>("ipv4"),
                    ipv6: r.get::<Option<String>, _>("ipv6"),
                    hostname: r.get::<Option<String>, _>("hostname"),
                })
            })
            .collect())
    }
}
