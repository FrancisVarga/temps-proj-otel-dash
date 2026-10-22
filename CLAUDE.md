# temps-proj-otel-dash

Temps plugin (OpenTelemetry trace ingest + dashboard) in two builds that must
stay behaviourally identical: Rust at the repo root (`src/`) and TypeScript in
`ts/` (the build Temps hosts actually install, via GitHub). `web/` is the shared
React UI. Read `README.md` for the architecture; `~/.claude/skills/temps-plugin`
has the general plugin rules.

## Commands

- `cargo check --tests` after every Rust change; `cargo test`; `cargo clippy --all-targets -- -D warnings`.
- `FORCE_WEB_BUILD=1 cargo build --release` for a binary with the embedded UI.
- Web UI lives in `web/` and uses Bun: `bun install`, `bun run build` (`tsc -b && vite build`), `bun run dev`.
- TypeScript build: `cd ts && bun test && bun run check && bun run build`. `bun run embed` regenerates the committed `ts/src/_embedded_ui.ts` from `web/dist`; run it after any `web/` change.
- Host install: `bunx --bun @temps-sdk/cli plugin install https://github.com/FrancisVarga/temps-proj-otel-dash --path ts` (GitHub mirror is remote `github`; Gitea is `origin`). Push both.
- Cargo's target dir may be redirected by the global Cargo config (currently `/mnt/pool/cargo-target`).

## Rules

- Never add a `/health` route or `#[tokio::main]`; the SDK runtime owns both.
- Async init inside `router()` goes through `tokio::task::block_in_place` + `Handle::current().block_on`.
- Ingest handlers must not await the database. Push rows into `IngestQueue`; only the writer task touches `SpanStore`'s writer pool.
- `/v1/*` is a `public_path`: every handler under it must verify the ingest token via `authorize_ingest` (header `X-Otel-Dash-Token`). The Temps proxy strips `Authorization`/`Cookie`/`x-temps-*`, so never rely on those.
- Mutating settings/token routes take `AuthenticatedCaller` and call `require_admin`.
- Wire types in `src/types.rs` are `serde(rename_all = "camelCase")` and mirrored in `ts/src/types.ts` and `web/src/types.ts`; keep all three in sync. A route or behaviour added to one build is added to the other in the same change.
- In `ts/`, handlers must call `readBody(req)` synchronously before any `await`: the SDK bridge emits the body in a microtask.
- Typed errors (`thiserror`), no `unwrap`/`expect` outside tests and startup.
- Tests live beside the code in `#[cfg(test)] mod tests` and use `tempfile` for SQLite.
- `vite.config.ts` `base` must stay `/api/x/otel-dash/ui/`.
- The `temps-plugin-sdk` git tag is a deliberate pin; do not bump it casually.

## Bun

Use `bun` (not npm/yarn/pnpm/node) for anything in `web/`.
