# camelmailer-migrate

Move a [Postal](https://postalserver.io) installation, or a
[Postmark](https://postmarkapp.com), [Resend](https://resend.com),
[Mailgun](https://www.mailgun.com) or [SendGrid](https://sendgrid.com)
account, to [CamelMailer](https://camelmailer.com) in one command. Pick the
source with `--source`; it defaults to `postal`, so the existing Postal
behaviour is unchanged.

For Postal it reads the database directly and recreates the configuration
through the CamelMailer admin API, carrying over the things that usually make
a mail migration painful: your **DKIM keys** and your **API and SMTP
credential keys**. Because the keys are preserved, your existing DNS keeps
validating and your existing integrations keep sending. No code change on
your side, and for self-hosted targets, no DKIM DNS change either.

The four API sources are read over each provider's HTTP API instead. Those
APIs deliberately do **not** expose existing sending API keys or DKIM private
keys, so an API source creates a **new** CamelMailer credential (update your
app) and a **fresh** per-domain DKIM key (a DNS change), and migrates what the
API does expose. See [Other sources](#other-sources) below.

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
| Message history (opt-in) | Messages | With `--history`: past messages, their delivery attempts, opens and clicks, imported as completed records, never re-sent |

Routes that forward to an SMTP or address endpoint are reported and skipped,
since CamelMailer has no equivalent for those.

### Message history

By default only configuration is migrated. Pass `--history` to also bring
across each server's past messages, read from Postal's separate per-server
message databases (`{prefix}-server-{id}`). Messages are written through a
non-sending import endpoint as completed records with their original
timestamps, delivery outcomes, opens and clicks. **Nothing is ever
re-delivered.**

You decide how much of each message comes across with `--history-bodies`:

- `full` (default): the complete raw message, headers and body, so "view
  source" works.
- `headers`: headers only, empty body. Smaller and faster.
- `index`: no raw content; minimal headers (From, To, Subject, Message-ID)
  are synthesized from the metadata so messages still list and search.

```bash
camelmailer-migrate \
  --postal-db mysql://postal:password@127.0.0.1:3306/postal \
  --target https://mail.example.com \
  --api-key "$CAMELMAILER_ADMIN_API_KEY" \
  --history --history-bodies full
```

History can be large. It is imported in batches (`--history-batch`, default
200) after each server's configuration.

### Choosing what to migrate

Everything migrates by default. Use `--skip` to leave categories out
(comma-separated): `domains`, `credentials`, `webhooks`, `routes`,
`ip-pools`. History stays off unless you pass `--history`. For example, to
move only servers and domains:

```bash
camelmailer-migrate ... --skip credentials,webhooks,routes,ip-pools
```

## Other sources

Postal is read from its database. The other four sources are read over each
provider's HTTP API with `--source` and `--source-api-key`:

```bash
camelmailer-migrate \
  --source mailgun \
  --source-api-key "$MAILGUN_API_KEY" \
  --source-region eu \
  --target https://app.camelmailer.com \
  --api-key "$CAMELMAILER_API_KEY" \
  --org acme \
  --dry-run
```

Everything runs through the same target: the URL still selects cloud vs
self-hosted, and `--dry-run`, `--yes`, `--history`, `--history-bodies`,
`--history-batch` and `--skip` all work the same way. An API source migrates
into a single CamelMailer server (`--server-name`, default the provider name).

> **API keys and DKIM are not portable over these APIs.** None of Postmark,
> Resend, Mailgun or SendGrid lets you read back an existing sending API key
> or a DKIM private key. So for an API source the tool creates a **new**
> CamelMailer API credential (set it in your app) and each domain gets a
> **fresh** CamelMailer DKIM key (publish its DNS record). Everything else is
> migrated as the provider's API exposes it.

What each API source migrates:

| Source | `--source-api-key` | Migrates | Notes |
| --- | --- | --- | --- |
| **Postmark** | Account token | Servers' domains and sender-signature domains, templates, suppressions (bounces and spam complaints), and outbound message history with `--history` | The account token discovers each server's token; templates, suppressions and history are read per server and folded into one CamelMailer server. |
| **Resend** | API key | Domains, unsubscribed audience contacts (as suppressions), and sent-email history with `--history` | Audiences and broadcasts have no admin create API, so they are reported for you to recreate as broadcast streams. No server-side templates API. |
| **Mailgun** | Private API key | Domains, routes, suppressions (bounces, unsubscribes, complaints), templates, and message history (Events API) with `--history` | `--source-region eu` for the EU host, or `--source-base-url` for a custom host. Routes map forward to endpoint, store to accept, stop to reject. |
| **SendGrid** | API key | Authenticated domains, dynamic templates, suppressions (bounces, blocks, spam reports, global and group unsubscribes), and Email Activity with `--history` | Email Activity needs the paid add-on; without it, history is skipped with a note. History carries metadata only, no bodies. |

For the API sources, `--skip` accepts `domains`, `credentials`,
`suppressions`, `templates` and `routes`. Every item that fails is reported
and the run continues, and re-running skips whatever already exists.

## Install

Pick one. None of these need you to clone this repository.

### Docker (no install)

Runs the published multi-arch image straight from GHCR. Use `--network host`
so it can reach your Postal database and the target:

```bash
docker run --rm --network host ghcr.io/camelmailer/camelmailer-migrate \
  --postal-db mysql://postal:password@127.0.0.1:3306/postal \
  --target https://app.camelmailer.com \
  --api-key "$CAMELMAILER_API_KEY" \
  --org acme \
  --dry-run
```

### Debian / Ubuntu (.deb)

Native amd64/arm64 packages from the
[releases page](https://github.com/camelmailer/camelmailer-migrate/releases):

```bash
sudo dpkg -i camelmailer-migrate_*.deb
camelmailer-migrate --help
```

### Prebuilt binary

Download and extract the archive for your platform from the
[releases page](https://github.com/camelmailer/camelmailer-migrate/releases),
then run `./camelmailer-migrate`.

### From source

Only if you want to build it yourself (needs a Rust toolchain):

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
| `--source <name>` | Source to read from: `postal` (default), `postmark`, `resend`, `mailgun`, `sendgrid`. |
| `--postal-db <url>` | Postal MySQL/MariaDB URL (for `--source postal`). Also read from `POSTAL_DATABASE_URL`. |
| `--source-api-key <key>` | API key for an API source. Also read from `SOURCE_API_KEY`. |
| `--source-region <us\|eu>` | Regional host for a provider that has one (Mailgun). |
| `--source-base-url <url>` | Override the source API base URL. Takes precedence over `--source-region`. |
| `--server-name <name>` | Name of the single CamelMailer server an API source migrates into (default: the provider name). |
| `--target <url>` | CamelMailer base URL. Its host selects cloud vs self-hosted. |
| `--api-key <key>` | Cloud user token or self-hosted admin key. Also `CAMELMAILER_API_KEY`. |
| `--org <permalink>` | Target organization. Required on the cloud. |
| `--server <permalink>` | Migrate only this one Postal server. |
| `--mode <cloud\|self-hosted>` | Override the URL-based mode detection. |
| `--no-dkim` | Generate fresh DKIM keys instead of importing Postal's. |
| `--history` | Also migrate message history (off by default). |
| `--history-bodies <full\|headers\|index>` | How message bodies come across (default `full`). |
| `--message-db-prefix <prefix>` | Postal message-DB name prefix (default `postal`). |
| `--history-batch <n>` | Messages per history import request (default 200). |
| `--skip <categories>` | Leave categories out: `domains,credentials,webhooks,routes,ip-pools`. |
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
