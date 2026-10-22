# OpenTelemetry Dashboard (TypeScript build)

Native Temps plugin, installable from GitHub. Temps builds `src/index.ts` with
Bun on the host and embeds the UI bundle generated into `src/_embedded_ui.ts`.

- `POST /v1/traces` — OTLP/HTTP JSON trace export, authenticated by the
  `X-Otel-Dash-Token` header (shown to admins under Settings).
- `GET /stats`, `/spans`, `/traces/{id}`, `/services`, `/settings` — dashboard reads.
- `PATCH /settings`, `GET|POST /settings/token` — admin only.

Spans are parsed into flat rows, queued, and written to a WAL-mode SQLite
database in the plugin data directory by a single batching writer. Retention
pruning runs every ten minutes.

```sh
bun install --frozen-lockfile
bun test && bun run check
bun run build                      # regenerates src/_embedded_ui.ts from ../web/dist, compiles dist/temps-otel-dash-plugin
bunx --bun @temps-sdk/cli plugin dev ./dist/temps-otel-dash-plugin --session otel-ts
```

Install on a Temps host:

```sh
bunx --bun @temps-sdk/cli plugin install https://github.com/FrancisVarga/temps-proj-otel-dash --path ts
```

The Rust implementation of the same plugin lives in the repository root; see
the top-level README for how the two relate.
