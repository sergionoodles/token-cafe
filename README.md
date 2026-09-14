# Token Cafe (Noctalia 5)

Subscription usage and remaining tokens for popular AI coding agents, as a native Noctalia 5 plugin.

Ported and modularized from the Noctalia 4 (QML) `token-cafe-old` project into the Noctalia 5 Luau plugin model: a headless `service.luau` backend fans out to one module per provider, a compact bar `widget.luau`, and a `panel.luau` dashboard with per-provider detail views.

Credit: evolved from Noctalia's original `model-usage` plugin via the Noctalia 4 Token Cafe fork.

## Layout

| Path | Role |
| --- | --- |
| `plugin.toml` | Manifest: entries + typed settings schema |
| `service.luau` | Backend: refresh fan-out, shared `tc_snapshot` state |
| `widget.luau` | Bar widget: top meters + tooltip, click opens panel |
| `panel.luau` | Panel: overview cards + provider detail (quotas, balances, models, history) |
| `lib/schema.luau` | Normalized provider card shape + registry |
| `lib/format.luau` | Token/pct/reset-time formatting, usage colors |
| `lib/runner.luau` | One-shot `runAsync` argv runner for the bundled Rust helper |
| `lib/codex.luau` | Codex (5h + weekly via `codex app-server`) |
| `lib/claude.luau` | Claude Code (local files + OAuth usage API) |
| `lib/antigravity.luau` | Google Antigravity (language server / Cloud Code) |
| `lib/opencode.luau` | OpenCode (local SQLite activity) |
| `lib/openrouter.luau` | OpenRouter (key limits + activity) |
| `lib/grok.luau` | Grok Build (weekly/monthly credits via `grok agent stdio`) |
| `lib/venice.luau` | Venice.ai (per-key rate limits) |
| `helper/` | Rust crate building `tc-probe`: one-shot JSON-to-stdout probes for the stdio/SQLite/LS protocols Luau can't speak directly (codex, opencode, grok, antigravity). Covered by `cargo test` |
| `translations/en.json` | i18n strings + settings labels |

Every provider normalizes into the same card (`lib/schema.luau: blank()`), so the widget and panel render without per-provider branches. Provider-specific extras ride along in `details`, `topModels`, and `recentDays`.

## Providers

- **Codex** — `codex app-server` weekly quota + 5h window, reset credits, daily/lifetime tokens. Needs `codex` on `PATH` (or `codex_bin` override).
- **Claude Code** — local `stats-cache.json` / `history.jsonl` plus authoritative OAuth rate limits from `.credentials.json`. No CLI needed.
- **Google Antigravity** — local language-server probe, Cloud Code quota fallback. Optional project ID + OAuth refresh config (env `TOKEN_CAFE_ANTIGRAVITY_*`, same as the old project).
- **OpenCode** — local DB found via `opencode db path`; sessions, tokens, models, cost. Needs `opencode` on `PATH`.
- **OpenRouter** — `/key` spending limits + 7d `/activity`. `OPENROUTER_API_KEY` env var wins over the settings field.
- **Grok Build** — `x.ai/billing` snapshot via `grok agent stdio` (legacy `_x.ai/billing` fallback). Needs `grok` on `PATH` (install: `curl -fsSL https://x.ai/cli/install.sh | bash`, then `grok login`).
- **Venice.ai** — `/api_keys/rate_limits` per key (USD + DIEM balances, tier, next epoch). `VENICE_API_KEY` env var is included automatically.

CLI-backed providers resolve `PATH` from the user's interactive login shell, so version-manager shims keep working when Noctalia starts with a reduced desktop-session environment.

## Install (local dev)

```bash
# Build the probe helper once (service auto-uses release, else debug):
cargo build --release --manifest-path /home/sergio/workspace/token-cafe/helper/Cargo.toml
mkdir -p ~/.local/share/noctalia/plugins
cp -r /home/sergio/workspace/token-cafe ~/.local/share/noctalia/plugins/token-cafe
# enable in Noctalia, then add the "Token Cafe: bar" widget to a bar
```

No Python involved: the only runtime helper is the `tc-probe` Rust binary
above (override path via the advanced `probe_bin` setting if needed).

Refresh over IPC:

```bash
noctalia msg plugin codingnoodles/token-cafe:service all refresh
noctalia msg panel-toggle codingnoodles/token-cafe:panel
```

## Validation

```bash
noctalia plugins lint .
cargo test --manifest-path helper/Cargo.toml
jq . translations/en.json >/dev/null
```

## Settings

Plugin-level (Settings → Plugins): `refresh_interval`, per-provider enable toggles, binaries/paths, API keys. Widget-level: `bar_provider_limit` (1–4 meters on the bar; panel and tooltip always show all providers).
