# temps-proj-otel-dash

OpenTelemetry dashboard plugin for [Temps](https://temps.sh). Services deployed
on Temps export OTLP/HTTP (JSON) traces to the plugin; the console gets a
service table, a live span feed and a per-trace waterfall.

The plugin exists in two builds that share the HTTP API, SQLite schema and the
React UI in `web/`:

| Build | Where | Install path |
|---|---|---|
| **TypeScript** (`@temps-sdk/plugin`, Bun, `bun:sqlite`) | `ts/` | *Install from GitHub* on any Temps host: `temps plugin install https://github.com/FrancisVarga/temps-proj-otel-dash --path ts` |
| **Rust** (`temps-plugin-sdk`, axum, sea-orm) | repository root | Local simulator / development hosts today; signed native-package catalog for distribution |

Temps' GitHub installer builds TypeScript only and the host refuses binaries
that are not backed by a verified install receipt, so the TypeScript build is
the one that runs on hosts right now. The Rust build is the reference for the
high-throughput path and is kept in lockstep (same routes, same wire types in
`src/types.rs` / `ts/src/types.ts` / `web/src/types.ts`).

```
exporter ─POST /api/x/otel-dash/v1/traces─▶ Temps proxy ─▶ plugin (unix socket)
                                                              │ parse + flatten
                                                              ▼
                                                 bounded queue (503 when full)
                                                              │
                                                              ▼
                                            one writer task, batched transactions
                                                              │
                                                              ▼
                                         SQLite in the plugin data dir (WAL mode)
```

Handlers never block on the database: ingest requests are parsed into flat rows
and handed to a single writer that commits up to 4 000 spans per transaction.
Reads use a separate pool so dashboards stay responsive under load.

## Layout

| Path | Purpose |
|---|---|
| `src/main.rs` | Manifest, router wiring, retention task, OpenAPI doc, entry point |
| `src/handlers.rs` | HTTP handlers, ingest-token auth, admin guards, embedded UI serving |
| `src/ingest.rs` | Bounded mpsc queue + batching writer task with counters |
| `src/db.rs` | SQLite store (sea-orm over tuned sqlx pools), DDL, queries, settings, token |
| `src/otlp.rs` | Minimal OTLP/JSON trace decoding into `SpanRow`s |
| `src/types.rs` | Wire types (`camelCase`) shared with the UI |
| `web/` | React + Vite UI, served at `/api/x/otel-dash/ui/` |
| `build.rs` | Builds `web/dist` in release (or `FORCE_WEB_BUILD=1`) and embeds it |

## Develop

Requires Rust (stable) and [Bun](https://bun.sh).

```sh
cargo check --tests          # type-check
cargo test                   # unit + router tests (SQLite in a temp dir)
cargo clippy --all-targets -- -D warnings
cargo build                  # debug binary with a placeholder UI page

cd web && bun install && bun run dev    # UI with hot reload (proxies to TEMPS_URL)
```

Release binary with the real UI embedded:

```sh
FORCE_WEB_BUILD=1 cargo build --release
```

### Local simulator (no Temps server)

```sh
bunx --bun @temps-sdk/cli plugin dev ./target/release/temps-otel-dash-plugin --session otel --port 18555
# in another shell
bunx --bun @temps-sdk/cli plugin dev status --session otel --json
```

The simulator prints a loopback preview URL. Requests from it carry an admin
identity, so `GET /settings/token` reveals the ingest token for testing. The
simulator only accepts non-GET requests that carry its preview session cookie
and a same-origin `Origin` header, so open the preview once and reuse the cookie:

```sh
B=http://127.0.0.1:18555
curl -s -c jar -o /dev/null "$B/"
TOKEN=$(curl -s -b jar "$B/api/x/otel-dash/settings/token" | jq -r .token)
curl -s -b jar -H "Origin: $B" -X POST "$B/api/x/otel-dash/v1/traces" \
  -H "X-Otel-Dash-Token: $TOKEN" -H 'Content-Type: application/json' \
  --data @fixtures/sample-traces.json
```

Note that if your global Cargo config sets `build.target-dir`, the binary lands
there instead of `./target`.

## Configure an exporter

```sh
OTEL_EXPORTER_OTLP_PROTOCOL=http/json
OTEL_EXPORTER_OTLP_TRACES_ENDPOINT=https://<temps-host>/api/x/otel-dash/v1/traces
OTEL_EXPORTER_OTLP_HEADERS="X-Otel-Dash-Token=<ingest token>"
```

The ingest token is generated on first start and shown (admins only) under
**Settings → Reveal token**. `/v1/*` is declared as a `public_path`, so Temps
does not require a console session for it; the plugin checks the token itself
with a constant-time comparison. The token travels in `X-Otel-Dash-Token`
because the Temps proxy strips `Authorization` and `Cookie` before forwarding
to any plugin.

## API

Mounted at `/api/x/otel-dash`. See `openapi_schema()` in `src/main.rs`.

| Method | Path | Auth |
|---|---|---|
| POST | `/v1/traces` | `X-Otel-Dash-Token` |
| GET | `/stats`, `/spans`, `/traces/{id}`, `/services`, `/settings` | Temps session |
| PATCH | `/settings` | Temps admin |
| GET / POST | `/settings/token` | Temps admin |

## TypeScript build (`ts/`)

```sh
cd ts
bun install --frozen-lockfile
bun test && bun run check
bun run build        # regenerates src/_embedded_ui.ts from ../web/dist and compiles dist/temps-otel-dash-plugin
bunx --bun @temps-sdk/cli plugin dev ./dist/temps-otel-dash-plugin --session otel-ts
```

`ts/src/_embedded_ui.ts` is generated and committed because the host installer
runs `bun install --ignore-scripts` and compiles `src/index.ts` offline. After
changing anything under `web/`, run `bun run embed` in `ts/` and commit the result.

Install or update on a host (admin API key configured in the Temps CLI):

```sh
bunx --bun @temps-sdk/cli plugin install https://github.com/FrancisVarga/temps-proj-otel-dash --path ts
bunx --bun @temps-sdk/cli plugin update otel-dash
```

## Distribution of the Rust build

Temps' *Install from GitHub* flow builds TypeScript plugins only. The Rust
binary is tested locally (simulator or a development host) and distributed
through the signed native-package catalog pipeline, which is a separate task
from this repository. Bump the `temps-plugin-sdk` tag in `Cargo.toml`
deliberately: the channel protocol must match the Temps host.

## License

Apache-2.0
