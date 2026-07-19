//! camelmailer-migrate: move a Postal, Postmark, Resend, Mailgun or SendGrid
//! account to CamelMailer.
//!
//! `--source postal` (the default) reads Postal's database directly and
//! recreates its configuration through the CamelMailer admin API: servers,
//! domains (carrying their DKIM private keys over unchanged), API and SMTP
//! credentials (keys preserved, so existing integrations keep working),
//! webhooks, routes and IP pools.
//!
//! The other four sources are read over each provider's HTTP API with
//! `--source-api-key`. Those APIs deliberately do NOT expose existing sending
//! API keys or DKIM private keys, so for an API source the tool creates a NEW
//! CamelMailer credential (update your app) and a fresh per-domain DKIM key (a
//! DNS change), and migrates what the API does expose: domains, suppressions,
//! templates, routes and (with `--history`) message history.
//!
//! The target URL alone decides where it writes: a `*.camelmailer.com` host
//! is the hosted cloud (bearer token, into one existing organization); any
//! other host is a self-hosted install (admin key, full access).

mod history;
mod postal;
mod sources;
mod target;

use std::collections::HashMap;
use std::io::{self, Write};

use anyhow::{bail, Context, Result};
use clap::Parser;

use history::BodyMode;
use postal::{Postal, Snapshot};
use sources::{ApiClient, ApiSnapshot, SourceKind};
use target::{field, ApiErr, Mode, Target};

/// The webhook events CamelMailer understands. Postal emits a wider set
/// (bounces, opens, clicks, DNS errors); those are dropped with a note.
const CM_EVENTS: [&str; 4] = [
    "MessageSent",
    "MessageDelayed",
    "MessageDeliveryFailed",
    "MessageHeld",
];

#[derive(Parser)]
#[command(
    name = "camelmailer-migrate",
    version,
    about = "Migrate Postal, Postmark, Resend, Mailgun or SendGrid to CamelMailer (cloud or self-hosted)."
)]
struct Cli {
    /// Where to read from: postal (default, reads the Postal database) or one
    /// of the HTTP-API sources postmark | resend | mailgun | sendgrid.
    #[arg(long, default_value = "postal")]
    source: String,

    /// Postal database URL, e.g. mysql://user:pass@host:3306/postal.
    /// Required for `--source postal`.
    #[arg(long, env = "POSTAL_DATABASE_URL")]
    postal_db: Option<String>,

    /// API key for an API source (postmark | resend | mailgun | sendgrid).
    /// For Postmark this is the account token; for Mailgun it is the private
    /// API key (used as the HTTP-basic password).
    #[arg(long, env = "SOURCE_API_KEY")]
    source_api_key: Option<String>,

    /// Region for a provider that has regional hosts. Mailgun: us (default) or
    /// eu. Ignored by the others.
    #[arg(long)]
    source_region: Option<String>,

    /// Override the source API base URL (for self-hosted or non-standard
    /// provider hosts). Takes precedence over --source-region.
    #[arg(long)]
    source_base_url: Option<String>,

    /// Name of the single CamelMailer server an API source migrates into.
    /// Defaults to the provider name (e.g. "Postmark").
    #[arg(long)]
    server_name: Option<String>,

    /// CamelMailer base URL. A *.camelmailer.com host selects the cloud;
    /// anything else is treated as a self-hosted install.
    #[arg(long)]
    target: String,

    /// CamelMailer key: a user token for the cloud, or the machine
    /// X-Admin-API-Key for a self-hosted install.
    #[arg(long, env = "CAMELMAILER_API_KEY")]
    api_key: String,

    /// Target organization permalink. Required for the cloud (organizations
    /// already exist). On self-hosted, when omitted, Postal's own
    /// organizations are recreated.
    #[arg(long)]
    org: Option<String>,

    /// Only migrate this one Postal server (by its permalink).
    #[arg(long)]
    server: Option<String>,

    /// Force the mode instead of deriving it from the URL: cloud | self-hosted.
    #[arg(long)]
    mode: Option<String>,

    /// Read and plan, but do not write anything.
    #[arg(long)]
    dry_run: bool,

    /// Generate fresh DKIM keys instead of importing Postal's (this does
    /// require a DNS change afterwards).
    #[arg(long)]
    no_dkim: bool,

    /// Do not ask for confirmation before writing.
    #[arg(long, short = 'y')]
    yes: bool,

    /// Also migrate message history (past messages and their delivery,
    /// open and click events). Off by default because it can be large.
    #[arg(long)]
    history: bool,

    /// How message bodies come across when importing history:
    /// `full` (headers + body), `headers` (headers only), or `index`
    /// (synthesize minimal headers, no body).
    #[arg(long, default_value = "full")]
    history_bodies: String,

    /// Postal message-database name prefix; per-server databases are
    /// `{prefix}-server-{id}`.
    #[arg(long, default_value = "postal")]
    message_db_prefix: String,

    /// Messages sent per history import request.
    #[arg(long, default_value_t = 200)]
    history_batch: usize,

    /// Config categories to leave out, comma-separated. Postal: any of
    /// `domains`, `credentials`, `webhooks`, `routes`, `ip-pools`. API
    /// sources: `domains`, `credentials`, `suppressions`, `templates`,
    /// `routes`.
    #[arg(long, value_delimiter = ',')]
    skip: Vec<String>,
}

impl Cli {
    fn skipped(&self, category: &str) -> bool {
        self.skip.iter().any(|s| s.eq_ignore_ascii_case(category))
    }
}

#[derive(Default)]
struct Stats {
    created: u32,
    skipped: u32,
    failed: u32,
}

impl Stats {
    /// Record one create call. Returns Ok(true) when the caller may proceed
    /// with dependent items (the entity now exists), Ok(false) when it should
    /// skip them, and Err on a fatal auth failure that aborts the run.
    fn record(&mut self, res: Result<serde_json::Value, ApiErr>, what: &str) -> Result<bool> {
        match res {
            Ok(_) => {
                println!("  \u{2713} {what}");
                self.created += 1;
                Ok(true)
            }
            Err(e) if e.is_fatal() => {
                bail!("authentication or permission error: {e}")
            }
            Err(e) if e.is_conflict() => {
                println!("  \u{29b8} {what} (already present, skipped)");
                self.skipped += 1;
                Ok(true)
            }
            Err(e) => {
                println!("  \u{2717} {what}: {e}");
                self.failed += 1;
                Ok(false)
            }
        }
    }

    fn note_skip(&mut self, what: &str, why: &str) {
        println!("  \u{29b8} {what} (skipped: {why})");
        self.skipped += 1;
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();
    let source = SourceKind::parse(&cli.source)?;

    let mode_override = match cli.mode.as_deref() {
        None => None,
        Some("cloud") => Some(Mode::Cloud),
        Some("self-hosted") | Some("selfhosted") | Some("self_hosted") => Some(Mode::SelfHosted),
        Some(other) => bail!("--mode must be 'cloud' or 'self-hosted', got {other:?}"),
    };

    let target = Target::new(&cli.target, &cli.api_key, mode_override)?;
    println!("Target: {} ({})", cli.target, target.mode);

    if target.mode == Mode::Cloud && cli.org.is_none() {
        bail!(
            "--org is required for a cloud target: cloud organizations already exist, so tell the \
             tool which one to migrate into (its permalink)."
        );
    }

    if cli.history {
        // Fail fast on a bad body mode before touching either side.
        BodyMode::parse(&cli.history_bodies)?;
    }

    if source.is_api() {
        return run_api_source(&target, &cli, source).await;
    }

    // --- Postal (database) source ---
    let postal_db = cli
        .postal_db
        .as_deref()
        .context("--postal-db is required for --source postal (or set POSTAL_DATABASE_URL)")?;

    // Read Postal first; this also validates the DB URL before we touch the
    // target.
    println!("Reading Postal database ...");
    let postal = Postal::connect(postal_db).await?;
    let mut snap = postal.read().await?;
    filter_snapshot(&mut snap, cli.server.as_deref());
    print_plan(&snap, &cli);

    if cli.dry_run {
        println!("\nDry run: nothing was written.");
        return Ok(());
    }

    // Validate the target credentials before asking to proceed.
    if let Err(e) = target.check().await {
        bail!("could not authenticate against {}: {e}", cli.target);
    }

    if !cli.yes && !confirm(&target, &cli)? {
        println!("Aborted.");
        return Ok(());
    }

    run(&target, &snap, &cli).await
}

/// Keep only the requested server (and its org) when --server is given.
fn filter_snapshot(snap: &mut Snapshot, only_server: Option<&str>) {
    let Some(permalink) = only_server else { return };
    let keep: Vec<i64> = snap
        .servers
        .iter()
        .filter(|s| s.permalink == permalink)
        .map(|s| s.id)
        .collect();
    snap.servers.retain(|s| keep.contains(&s.id));
    snap.domains.retain(|d| keep.contains(&d.server_id));
    snap.credentials.retain(|c| keep.contains(&c.server_id));
    snap.webhooks.retain(|w| keep.contains(&w.server_id));
    snap.routes.retain(|r| keep.contains(&r.server_id));
}

fn print_plan(snap: &Snapshot, cli: &Cli) {
    println!("\nFound in Postal:");
    println!("  organizations : {}", snap.orgs.len());
    println!("  servers       : {}", snap.servers.len());
    println!("  domains       : {}", snap.domains.len());
    println!("  credentials   : {}", snap.credentials.len());
    println!("  webhooks      : {}", snap.webhooks.len());
    println!("  routes        : {}", snap.routes.len());
    if cli.mode.as_deref() != Some("cloud") && cli.org.is_none() {
        println!("  ip pools      : {}", snap.ip_pools.len());
    }
    let with_dkim = snap
        .domains
        .iter()
        .filter(|d| d.dkim_private_key.is_some())
        .count();
    if cli.no_dkim {
        println!(
            "\nDKIM keys will be regenerated (--no-dkim), so update the DKIM DNS record after."
        );
    } else {
        println!(
            "\n{with_dkim} of {} domains carry a DKIM key that will be imported unchanged.",
            snap.domains.len()
        );
    }
    if cli.history {
        println!(
            "Message history: ON (bodies: {}); imported per server after its config.",
            cli.history_bodies
        );
    }
    if !cli.skip.is_empty() {
        println!("Skipping categories: {}", cli.skip.join(", "));
    }
}

fn confirm(target: &Target, cli: &Cli) -> Result<bool> {
    let where_to = match target.mode {
        Mode::Cloud => format!(
            "the cloud organization {:?}",
            cli.org.as_deref().unwrap_or("")
        ),
        Mode::SelfHosted => match &cli.org {
            Some(org) => format!("organization {org:?}"),
            None => "organizations mirrored from Postal".to_string(),
        },
    };
    print!("\nWrite this into {where_to} at {}? [y/N] ", cli.target);
    io::stdout().flush().ok();
    let mut line = String::new();
    io::stdin()
        .read_line(&mut line)
        .context("reading confirmation")?;
    Ok(matches!(line.trim(), "y" | "Y" | "yes" | "Yes"))
}

async fn run(target: &Target, snap: &Snapshot, cli: &Cli) -> Result<()> {
    let mut stats = Stats::default();

    // Group children by their Postal server id for quick lookup.
    let orgs_by_id: HashMap<i64, &postal::Org> = snap.orgs.iter().map(|o| (o.id, o)).collect();

    // Resolve, per Postal org, which target org permalink to use, creating
    // organizations on self-hosted when no single --org was given.
    let mut org_permalink: HashMap<i64, String> = HashMap::new();
    for server in &snap.servers {
        if org_permalink.contains_key(&server.org_id) {
            continue;
        }
        let permalink = match (target.mode, &cli.org) {
            (Mode::Cloud, Some(org)) => org.clone(),
            (Mode::SelfHosted, Some(org)) => org.clone(),
            (Mode::SelfHosted, None) => {
                let src = orgs_by_id.get(&server.org_id);
                let name = src.map(|o| o.name.as_str()).unwrap_or("Migrated");
                let permalink = src
                    .map(|o| o.permalink.clone())
                    .filter(|p| !p.is_empty())
                    .unwrap_or_else(|| format!("org-{}", server.org_id));
                println!("\nOrganization {name:?}");
                stats.record(
                    target.create_org(name, &permalink).await,
                    &format!("organization {permalink}"),
                )?;
                permalink
            }
            (Mode::Cloud, None) => unreachable!("cloud requires --org, checked earlier"),
        };
        org_permalink.insert(server.org_id, permalink);
    }
    // On self-hosted with an explicit --org, make sure it exists once.
    if target.mode == Mode::SelfHosted {
        if let Some(org) = &cli.org {
            stats.record(
                target.create_org(org, org).await,
                &format!("organization {org}"),
            )?;
        }
    }

    for server in &snap.servers {
        let org = org_permalink
            .get(&server.org_id)
            .cloned()
            .unwrap_or_else(|| cli.org.clone().unwrap_or_default());
        let sp = &server.permalink;
        println!("\nServer {:?} -> {org}/{sp}", server.name);

        let mode = match server.mode.as_str() {
            "Development" => "Development",
            _ => "Live",
        };
        let proceed = stats.record(
            target.create_server(&org, &server.name, sp, mode).await,
            &format!("server {sp}"),
        )?;
        if !proceed {
            println!("  (skipping this server's domains, credentials, webhooks and routes)");
            continue;
        }

        if !cli.skipped("domains") {
            migrate_domains(target, snap, cli, &mut stats, server, &org).await?;
        }
        if !cli.skipped("credentials") {
            migrate_credentials(target, snap, &mut stats, server, &org).await?;
        }
        if !cli.skipped("webhooks") {
            migrate_webhooks(target, snap, &mut stats, server, &org).await?;
        }
        if !cli.skipped("routes") {
            migrate_routes(target, snap, &mut stats, server, &org).await?;
        }
        if cli.history {
            migrate_history(target, cli, &mut stats, server, &org).await?;
        }
    }

    if target.mode == Mode::SelfHosted && cli.org.is_none() && !cli.skipped("ip-pools") {
        migrate_ip_pools(target, snap, &mut stats).await?;
    } else if !snap.ip_pools.is_empty() && !cli.skipped("ip-pools") {
        println!(
            "\nIP pools are installation-level and were left out ({} in Postal); create them on a \
             self-hosted target without --org to include them.",
            snap.ip_pools.len()
        );
    }

    println!(
        "\nDone. {} created, {} skipped, {} failed.",
        stats.created, stats.skipped, stats.failed
    );
    if stats.failed > 0 {
        println!(
            "Some items failed; the messages above say why. Re-running skips what already exists."
        );
    }
    Ok(())
}

async fn migrate_domains(
    target: &Target,
    snap: &Snapshot,
    cli: &Cli,
    stats: &mut Stats,
    server: &postal::Server,
    org: &str,
) -> Result<()> {
    for d in snap.domains.iter().filter(|d| d.server_id == server.id) {
        let dkim = if cli.no_dkim {
            None
        } else {
            d.dkim_private_key.as_deref()
        };
        let created = stats.record(
            target
                .create_domain(org, &server.permalink, &d.name, dkim)
                .await,
            &format!("domain {}", d.name),
        )?;
        // Carry the verified state over so the domain is ready to send. Only
        // the self-hosted admin key may force-verify; on the cloud the domain
        // starts unverified and its DNS challenge must be published.
        if created && d.verified && target.mode == Mode::SelfHosted {
            match target
                .force_verify_domain(org, &server.permalink, &d.name)
                .await
            {
                Ok(_) => println!("    \u{2713} verified {}", d.name),
                Err(e) => println!("    \u{2717} could not verify {}: {e}", d.name),
            }
        }
    }
    Ok(())
}

async fn migrate_credentials(
    target: &Target,
    snap: &Snapshot,
    stats: &mut Stats,
    server: &postal::Server,
    org: &str,
) -> Result<()> {
    for c in snap.credentials.iter().filter(|c| c.server_id == server.id) {
        let kind = match c.kind.to_uppercase().as_str() {
            "SMTP" => "SMTP",
            _ => "API",
        };
        let name = if c.name.is_empty() {
            format!("{kind} key")
        } else {
            c.name.clone()
        };
        stats.record(
            target
                .create_credential(org, &server.permalink, kind, &name, Some(&c.key))
                .await,
            &format!("{kind} credential {name:?} (key preserved)"),
        )?;
    }
    Ok(())
}

async fn migrate_webhooks(
    target: &Target,
    snap: &Snapshot,
    stats: &mut Stats,
    server: &postal::Server,
    org: &str,
) -> Result<()> {
    for w in snap.webhooks.iter().filter(|w| w.server_id == server.id) {
        let mapped: Vec<String> = w
            .events
            .iter()
            .filter(|e| CM_EVENTS.contains(&e.as_str()))
            .cloned()
            .collect();
        let dropped = w.events.len() - mapped.len();
        // If the source subscribed to all events, or none of its specific
        // events map, subscribe to everything CamelMailer sends so the hook
        // keeps firing.
        let all_events = w.all_events || mapped.is_empty();
        let name = if w.name.is_empty() {
            w.url.clone()
        } else {
            w.name.clone()
        };
        let created = stats.record(
            target
                .create_webhook(
                    org,
                    &server.permalink,
                    &name,
                    &w.url,
                    all_events,
                    w.sign,
                    if all_events { &[] } else { &mapped },
                )
                .await,
            &format!("webhook {name:?}"),
        )?;
        if created && dropped > 0 {
            println!(
                "    note: {dropped} Postal event(s) on this webhook have no CamelMailer \
                 equivalent and were not carried over"
            );
        }
        if created && !w.enabled {
            println!("    note: this webhook was disabled in Postal; disable it in the dashboard");
        }
    }
    Ok(())
}

async fn migrate_routes(
    target: &Target,
    snap: &Snapshot,
    stats: &mut Stats,
    server: &postal::Server,
    org: &str,
) -> Result<()> {
    for r in snap.routes.iter().filter(|r| r.server_id == server.id) {
        let name = if r.name.is_empty() { "*" } else { &r.name };
        let label = format!("route {name:?}");

        // Resolve the destination. HTTP endpoints become an endpoint URL;
        // the accept/hold/bounce/reject modes carry over directly. SMTP and
        // address endpoints have no CamelMailer equivalent.
        let (mode, endpoint_url): (&str, Option<String>) = match r.endpoint_type.as_deref() {
            Some("HTTPEndpoint") => match r.endpoint_id.and_then(|id| snap.http_endpoints.get(&id))
            {
                Some(url) => ("Endpoint", Some(url.clone())),
                None => {
                    stats.note_skip(&label, "its HTTP endpoint could not be resolved");
                    continue;
                }
            },
            Some("SMTPEndpoint") | Some("AddressEndpoint") => {
                stats.note_skip(
                    &label,
                    "CamelMailer has no SMTP/address forwarding route; recreate it by hand",
                );
                continue;
            }
            _ => match r.mode.as_str() {
                "Accept" | "Hold" | "Bounce" | "Reject" => (r.mode.as_str(), None),
                "Endpoint" => {
                    stats.note_skip(&label, "endpoint route without a resolvable endpoint");
                    continue;
                }
                other => {
                    stats.note_skip(&label, &format!("unknown Postal route mode {other:?}"));
                    continue;
                }
            },
        };

        stats.record(
            target
                .create_route(
                    org,
                    &server.permalink,
                    name,
                    r.domain.as_deref(),
                    mode,
                    endpoint_url.as_deref(),
                )
                .await,
            &label,
        )?;
    }
    Ok(())
}

async fn migrate_history(
    target: &Target,
    cli: &Cli,
    stats: &mut Stats,
    server: &postal::Server,
    org: &str,
) -> Result<()> {
    let mode = BodyMode::parse(&cli.history_bodies)?;
    let postal_db = cli
        .postal_db
        .as_deref()
        .context("--postal-db is required for history")?;
    // The message data lives in a separate `{prefix}-server-{id}` database
    // keyed by the Postal server id.
    let pool = match history::connect(postal_db, &cli.message_db_prefix, server.id).await {
        Ok(pool) => pool,
        Err(error) => {
            println!("  \u{29b8} history: no message database for this server ({error}); skipped");
            return Ok(());
        }
    };
    let messages = match history::read_messages(&pool, mode).await {
        Ok(messages) => messages,
        Err(error) => {
            println!("  \u{2717} history: could not read messages: {error}");
            return Ok(());
        }
    };
    if messages.is_empty() {
        println!("  history: no messages");
        return Ok(());
    }
    let total = messages.len();
    let mut imported = 0usize;
    let mut failed = 0usize;
    for chunk in messages.chunks(cli.history_batch.max(1)) {
        match target.import_messages(org, &server.permalink, chunk).await {
            Ok(data) => {
                imported += data.get("imported").and_then(|v| v.as_u64()).unwrap_or(0) as usize;
                if let Some(list) = data.get("failed").and_then(|v| v.as_array()) {
                    failed += list.len();
                }
            }
            Err(error) if error.is_fatal() => {
                bail!("history import authentication error: {error}")
            }
            Err(error) => {
                failed += chunk.len();
                println!("  \u{2717} history batch failed: {error}");
            }
        }
        println!("  history: {}/{total} messages", imported + failed);
    }
    stats.created += imported as u32;
    stats.failed += failed as u32;
    println!("  history: {imported} imported, {failed} failed ({total} total)");
    Ok(())
}

async fn migrate_ip_pools(target: &Target, snap: &Snapshot, stats: &mut Stats) -> Result<()> {
    if snap.ip_pools.is_empty() {
        return Ok(());
    }
    println!("\nIP pools");
    for pool in &snap.ip_pools {
        let res = target.create_ip_pool(&pool.name, pool.default).await;
        // Grab the new pool id so its addresses can be attached.
        let new_id = res.as_ref().ok().and_then(|d| {
            field(d, "ip_pool", "id")
                .and_then(|s| s.parse::<i64>().ok())
                .or_else(|| {
                    d.get("ip_pool")
                        .and_then(|p| p.get("id"))
                        .and_then(serde_json::Value::as_i64)
                })
        });
        let ok = stats.record(res, &format!("ip pool {:?}", pool.name))?;
        if !ok {
            continue;
        }
        let Some(pool_id) = new_id else {
            println!("    note: could not read the new pool id; add its IP addresses by hand");
            continue;
        };
        for addr in snap.ip_addresses.iter().filter(|a| a.pool_id == pool.id) {
            let label = addr
                .ipv4
                .clone()
                .or_else(|| addr.ipv6.clone())
                .unwrap_or_else(|| "address".to_string());
            stats.record(
                target
                    .create_ip_address(
                        pool_id,
                        addr.ipv4.as_deref(),
                        addr.ipv6.as_deref(),
                        addr.hostname.as_deref(),
                    )
                    .await,
                &format!("ip address {label}"),
            )?;
        }
    }
    Ok(())
}

// ----------------------------------------------------------- API sources

/// Migrate one HTTP-API source (Postmark, Resend, Mailgun, SendGrid) into a
/// single CamelMailer server. Unlike the Postal path, sending keys and DKIM
/// private keys cannot be read back over these APIs, so a fresh credential and
/// per-domain DKIM key are created and the user updates their app and DNS.
async fn run_api_source(target: &Target, cli: &Cli, source: SourceKind) -> Result<()> {
    let api_key = cli.source_api_key.as_deref().with_context(|| {
        format!(
            "--source-api-key is required for --source {} (or set SOURCE_API_KEY)",
            cli.source
        )
    })?;
    let client = ApiClient::new(
        source,
        api_key,
        cli.source_base_url.as_deref(),
        cli.source_region.as_deref(),
    )?;

    let provider = source.provider_name();
    println!("Reading {provider} ...");
    let snap = client.snapshot().await?;
    print_api_plan(&snap, cli, source);

    if cli.dry_run {
        if cli.history {
            println!(
                "Message history: ON (bodies: {}); read from the {provider} API after config.",
                cli.history_bodies
            );
        }
        println!("\nDry run: nothing was written.");
        return Ok(());
    }

    if let Err(e) = target.check().await {
        bail!("could not authenticate against {}: {e}", cli.target);
    }
    if !cli.yes && !confirm(target, cli)? {
        println!("Aborted.");
        return Ok(());
    }

    run_api(target, &client, cli, source, &snap).await
}

fn print_api_plan(snap: &ApiSnapshot, cli: &Cli, source: SourceKind) {
    let provider = source.provider_name();
    println!("\nFound in {provider}:");
    println!("  domains       : {}", snap.domains.len());
    println!("  suppressions  : {}", snap.suppressions.len());
    println!("  templates     : {}", snap.templates.len());
    if source == SourceKind::Mailgun {
        println!("  routes        : {}", snap.routes.len());
    }
    println!(
        "\nHeads up: {provider} does not expose existing sending API keys or DKIM private keys over \
         its API. A NEW CamelMailer credential is created (update your app with it) and each domain \
         gets a fresh CamelMailer DKIM key (publish its DNS record). Everything else above is \
         migrated as-is."
    );
    for note in &snap.notes {
        println!("  note: {note}");
    }
    if cli.history {
        println!(
            "Message history: ON (bodies: {}); read from the {provider} API after config.",
            cli.history_bodies
        );
    }
    if !cli.skip.is_empty() {
        println!("Skipping categories: {}", cli.skip.join(", "));
    }
}

async fn run_api(
    target: &Target,
    client: &ApiClient,
    cli: &Cli,
    source: SourceKind,
    snap: &ApiSnapshot,
) -> Result<()> {
    let mut stats = Stats::default();

    let server_name = cli
        .server_name
        .clone()
        .unwrap_or_else(|| source.provider_name().to_string());
    let server_permalink = sources::permalink(&server_name);

    // Resolve the organization: on the cloud it must already exist (--org); on
    // self-hosted use --org or create one from the server name.
    let org = match (target.mode, &cli.org) {
        (Mode::Cloud, Some(org)) => org.clone(),
        (Mode::SelfHosted, Some(org)) => {
            stats.record(
                target.create_org(org, org).await,
                &format!("organization {org}"),
            )?;
            org.clone()
        }
        (Mode::SelfHosted, None) => {
            let permalink = sources::permalink(&server_name);
            println!("\nOrganization {server_name:?}");
            stats.record(
                target.create_org(&server_name, &permalink).await,
                &format!("organization {permalink}"),
            )?;
            permalink
        }
        (Mode::Cloud, None) => unreachable!("cloud requires --org, checked earlier"),
    };

    println!("\nServer {server_name:?} -> {org}/{server_permalink}");
    let proceed = stats.record(
        target
            .create_server(&org, &server_name, &server_permalink, "Live")
            .await,
        &format!("server {server_permalink}"),
    )?;
    if !proceed {
        println!("  (skipping this server's domains, credentials and history)");
        return Ok(());
    }

    // Domains: a fresh DKIM key each (no key is readable over the API), so they
    // start unverified and need a DNS change.
    if !cli.skipped("domains") {
        for d in &snap.domains {
            let was = if d.verified {
                " (verified at the source; still needs a fresh CamelMailer DKIM DNS record)"
            } else {
                " (unverified at the source)"
            };
            stats.record(
                target
                    .create_domain(&org, &server_permalink, &d.name, None)
                    .await,
                &format!("domain {}{was}", d.name),
            )?;
        }
    }

    // A fresh API credential. Capture the generated key so templates can be
    // created through the server messaging API.
    let mut server_api_key: Option<String> = None;
    if !cli.skipped("credentials") {
        let name = format!("{} migration key", source.provider_name());
        match target
            .create_credential(&org, &server_permalink, "API", &name, None)
            .await
        {
            Ok(data) => {
                println!("  \u{2713} API credential {name:?} (NEW key; update your app)");
                stats.created += 1;
                server_api_key = field(&data, "credential", "key").map(str::to_string);
                if server_api_key.is_none() {
                    println!("    note: could not read the new credential key from the response");
                }
            }
            Err(e) if e.is_fatal() => bail!("authentication or permission error: {e}"),
            Err(e) if e.is_conflict() => {
                println!("  \u{29b8} API credential {name:?} (already present, skipped)");
                stats.skipped += 1;
            }
            Err(e) => {
                println!("  \u{2717} API credential {name:?}: {e}");
                stats.failed += 1;
            }
        }
    }

    // Suppressions (server-wide), honored before every send.
    if !cli.skipped("suppressions") && !snap.suppressions.is_empty() {
        println!("\nSuppressions ({})", snap.suppressions.len());
        for s in &snap.suppressions {
            stats.record(
                target
                    .create_suppression(
                        &org,
                        &server_permalink,
                        &s.address,
                        "recipient",
                        Some(&s.reason),
                    )
                    .await,
                &format!("suppression {}", s.address),
            )?;
        }
    }

    // Templates, through the server messaging API (needs the fresh key).
    if !cli.skipped("templates") && !snap.templates.is_empty() {
        println!("\nTemplates ({})", snap.templates.len());
        match &server_api_key {
            Some(key) => {
                for t in &snap.templates {
                    stats.record(
                        target
                            .create_template(
                                key,
                                &t.name,
                                &t.permalink,
                                t.subject.as_deref(),
                                t.html_body.as_deref(),
                                t.text_body.as_deref(),
                            )
                            .await,
                        &format!("template {:?}", t.name),
                    )?;
                }
            }
            None => stats.note_skip(
                &format!("{} template(s)", snap.templates.len()),
                "no server API key was available (credential step skipped or failed)",
            ),
        }
    }

    // Routes (Mailgun only).
    if !cli.skipped("routes") && !snap.routes.is_empty() {
        println!("\nRoutes ({})", snap.routes.len());
        for r in &snap.routes {
            stats.record(
                target
                    .create_route(
                        &org,
                        &server_permalink,
                        &r.name,
                        r.domain.as_deref(),
                        &r.mode,
                        r.endpoint_url.as_deref(),
                    )
                    .await,
                &format!("route {:?}", r.name),
            )?;
        }
    }

    // Message history.
    if cli.history {
        migrate_api_history(
            target,
            client,
            cli,
            &mut stats,
            &org,
            &server_permalink,
            snap,
        )
        .await?;
    }

    println!(
        "\nDone. {} created, {} skipped, {} failed.",
        stats.created, stats.skipped, stats.failed
    );
    if stats.failed > 0 {
        println!(
            "Some items failed; the messages above say why. Re-running skips what already exists."
        );
    }
    println!(
        "\nRemember to update your application with the new API credential and to publish each \
         domain's fresh DKIM DNS record before you cut traffic over."
    );
    Ok(())
}

async fn migrate_api_history(
    target: &Target,
    client: &ApiClient,
    cli: &Cli,
    stats: &mut Stats,
    org: &str,
    server_permalink: &str,
    snap: &ApiSnapshot,
) -> Result<()> {
    let mode = BodyMode::parse(&cli.history_bodies)?;
    let domain_names: Vec<String> = snap.domains.iter().map(|d| d.name.clone()).collect();
    let (messages, notes) = match client.history(&domain_names, mode).await {
        Ok(result) => result,
        Err(error) => {
            println!("  \u{2717} history: could not read messages: {error}");
            return Ok(());
        }
    };
    for note in notes {
        println!("  note: {note}");
    }
    if messages.is_empty() {
        println!("  history: no messages");
        return Ok(());
    }
    let total = messages.len();
    let mut imported = 0usize;
    let mut failed = 0usize;
    for chunk in messages.chunks(cli.history_batch.max(1)) {
        match target.import_messages(org, server_permalink, chunk).await {
            Ok(data) => {
                imported += data.get("imported").and_then(|v| v.as_u64()).unwrap_or(0) as usize;
                if let Some(list) = data.get("failed").and_then(|v| v.as_array()) {
                    failed += list.len();
                }
            }
            Err(error) if error.is_fatal() => {
                bail!("history import authentication error: {error}")
            }
            Err(error) => {
                failed += chunk.len();
                println!("  \u{2717} history batch failed: {error}");
            }
        }
        println!("  history: {}/{total} messages", imported + failed);
    }
    stats.created += imported as u32;
    stats.failed += failed as u32;
    println!("  history: {imported} imported, {failed} failed ({total} total)");
    Ok(())
}
