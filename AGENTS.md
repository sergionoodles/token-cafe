# AGENTS.md

Working rules for contributors and coding agents in `token-cafe` (Noctalia 5).

## Project intent

Token Cafe shows AI subscription usage and remaining tokens:

- bar-level quick glance meters (`widget.luau`)
- panel dashboard + provider detail (`panel.luau`)
- headless refresh fan-out (`service.luau` + `lib/*` + `helper/`)

Ported from the Noctalia 4 QML project (`token-cafe-old`). Noctalia 5 plugins are Luau entries (`plugin.toml` + `.luau`), not QML — do not add QML files here.

## Architecture map

- `service.luau`: backend. Fans out to `lib/<provider>.luau`, publishes `tc_snapshot`, handles `tc_command` refresh. No UI calls.
- `widget.luau`: bar UI only. Watches `tc_snapshot`, renders top meters, sends `tc_command`.
- `panel.luau`: dashboard + detail UI only. Watches `tc_snapshot`, sends `tc_command`.
- `lib/schema.luau`: normalized provider card + registry (`ORDER`, `META`, `blank()`, `fromProbe()`). All cross-entry state must be plain data (no functions).
- `lib/format.luau`: pure formatting (tokens, pct, reset labels, colors).
- `lib/runner.luau`: one-shot `runAsync` argv runner for the `tc-probe` Rust helper.
- `lib/<provider>.luau`: one module per provider exposing `isEnabled()` + `refresh(done)`. `done(providerCardOrNil)` must fire exactly once.
- `helper/` (Rust crate): synchronous one-shot probes for stdio/SQLite/LS protocols Luau can't speak directly. Keep them one-shot JSON-to-stdout; cover logic with `cargo test`.
- `plugin.toml` / `translations/en.json`: manifest + settings schema + i18n. Every `label_key`/`description_key` must exist in `en.json`.

## Safe change rules

- Keep the normalized card shape in `schema.blank()` stable; add optional fields only.
- Keep provider modules self-contained; shared logic goes in `lib/format.luau` or `lib/runner.luau`.
- `service.luau` never calls `barWidget.*`/`panel.*`; widget/panel never call `runAsync`/`http` directly (except via lib providers — prefer keeping all probing in the service path).
- Do not hardcode secrets; env vars first, settings fields second.
- Per-call time budgets apply: keep each callback fast, parallelize HTTP/probes, never block.
- `plugin_api = 24` is the minimum for `require("./lib/*.luau")` (22) + argv `runAsync` (24). Don't use newer APIs without bumping the level deliberately.

## Provider contract

`refresh(done)` resolves to `schema.blank(id)` merged with:

- `ready`, `error`
- `primaryLabel`, `primaryPct` (-1 unknown), `primaryReset` (ISO str)
- `secondaryLabel`, `secondaryPct`, `secondaryReset`
- `windows[]` ({label, pct, reset}) for multi-quota providers
- `tier`, `balanceLabel`, `balanceValue`
- `todayTokens`, `todayPrompts`, `todaySessions`
- `details[]` ({k, v}), `topModels[]` ({model, tokens}), `recentDays[]` ({date, tokens, messages})

## Validation checklist

```bash
noctalia plugins lint .
cargo test --manifest-path helper/Cargo.toml
jq . translations/en.json >/dev/null
```

`noctalia plugins lint` cross-checks declared settings against `getConfig()` calls — fix loud misses (read-but-undeclared) before committing.
