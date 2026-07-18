# camelmailer-migrate

Move a [Postal](https://postalserver.io) installation to
[CamelMailer](https://camelmailer.com) in one command. It reads your Postal
database directly and recreates the configuration through the CamelMailer
admin API, carrying over the things that usually make a mail migration
painful: your **DKIM keys** and your **API and SMTP credential keys**.

Because the keys are preserved, your existing DNS keeps validating and your
existing integrations keep sending. No code change on your side, and for
self-hosted targets, no DKIM DNS change either.

One target URL decides where it writes:

- a `*.camelmailer.com` host is the **hosted cloud** (authenticates with a
  user token, migrates into one existing organization you name)
- any other host is a **self-hosted** install (authenticates with the
  machine admin key, and can create organizations, force-verify domains and
  set up IP pools)

## What it migrates

| Postal | CamelMailer | Notes |
| --- | --- | --- |
| Organizations | Organizations | Recreated on self-hosted; on the cloud you pick one existing org |
| Mail servers | Servers | Name, permalink and mode (Live / Development) |
| Domains | Domains | **DKIM private key imported unchanged**; verified state carried over on self-hosted |
| API and SMTP credentials | Credentials | **Key value preserved**, so existing senders keep working |
| Webhooks | Webhooks | URL, signing, and the events CamelMailer supports |
| Routes to HTTP endpoints | Routes | Endpoint URL and the accept / hold / bounce / reject modes |
| IP pools and addresses | IP pools | Self-hosted targets only (installation-level) |

Message history is **not** migrated. It lives in Postal's separate per-server
databases and is left in place; keep the old Postal instance readable for as
long as you need the archive.

Routes that forward to an SMTP or address endpoint are reported and skipped,
since CamelMailer has no equivalent for those.

## Install

Download a binary from the [releases](https://github.com/camelmailer/camelmailer-migrate/releases),
or build from source (needs a recent Rust toolchain):

```bash
cargo install --git https://github.com/camelmailer/camelmailer-migrate
```

## Use

Always start with `--dry-run`. It reads Postal and prints exactly what it
would create, and writes nothing.

```bash
camelmailer-migrate \
  --postal-db mysql://postal:password@127.0.0.1:3306/postal \
  --target https://app.camelmailer.com \
  --api-key "$CAMELMAILER_API_KEY" \
  --org acme \
  --dry-run
```

The `--target` decides the mode automatically. Drop `--dry-run` to run it.

### Cloud

The cloud host selects cloud mode. Pass the organization to migrate into and
a user token as the key:

```bash
camelmailer-migrate \
  --postal-db mysql://postal:password@db.internal/postal \
  --target https://app.camelmailer.com \
  --api-key "$CAMELMAILER_API_KEY" \
  --org acme
```

On the cloud, domains are created but start unverified: publish the
verification DNS record CamelMailer shows for each one. The DKIM key is still
imported, so your reputation carries over.

### Self-hosted

Any other host selects self-hosted mode. Use the machine admin key. Omit
`--org` to mirror Postal's own organizations, or pass one to put everything
under a single organization:

```bash
camelmailer-migrate \
  --postal-db mysql://postal:password@127.0.0.1:3306/postal \
  --target https://mail.example.com \
  --api-key "$CAMELMAILER_ADMIN_API_KEY"
```

On self-hosted, domains that were verified in Postal are force-verified, so
they are ready to send right away.

## Options

| Flag | What it does |
| --- | --- |
| `--postal-db <url>` | Postal MySQL/MariaDB URL. Also read from `POSTAL_DATABASE_URL`. |
| `--target <url>` | CamelMailer base URL. Its host selects cloud vs self-hosted. |
| `--api-key <key>` | Cloud user token or self-hosted admin key. Also `CAMELMAILER_API_KEY`. |
| `--org <permalink>` | Target organization. Required on the cloud. |
| `--server <permalink>` | Migrate only this one Postal server. |
| `--mode <cloud\|self-hosted>` | Override the URL-based mode detection. |
| `--no-dkim` | Generate fresh DKIM keys instead of importing Postal's. |
| `--dry-run` | Read and plan only, write nothing. |
| `-y`, `--yes` | Skip the confirmation prompt. |

## Safe to re-run

Every create is independent. If a run is interrupted or an item fails, run it
again: anything that already exists is reported and skipped, and the rest is
created. The tool only ever creates; it never deletes or overwrites.

## How it reads Postal

Postal keeps its configuration in one `postal` database. There is no export
API that covers domains, DKIM keys, credentials, webhooks and routes, so the
tool reads those tables directly (read-only). The database user needs `SELECT`
on the `postal` database. Message data is never read.

## License

MIT. See [LICENSE](LICENSE). CamelMailer began as a ground-up Rust rewrite of
Postal and keeps that attribution; this tool is an independent migration
helper.
