# AGENTS.md

Guidance for LLM harnesses (OpenCode, Claude Code, ...) working in this repo.

## What this is

modelmux is a rule-based LLM routing proxy written in Rust. It exposes one
OpenAI-compatible endpoint to coding agents and routes each request to one
of several model providers, based on policy, time windows, and failover.
Single static binary, no database, no external services.

- Behavior contract: `SPEC.md`. Read it before changing routing, windows,
  protocol translation, or the filter chain.
- User-facing documentation: `README.md`.
- Annotated configuration schema: `config.example.toml` (the source of
  truth for field names).

## Commands

`cargo` is not installed on the host. Run every toolchain command through
the Nix devshell:

| Action | Command |
|---|---|
| Build | `nix develop -c cargo build` |
| Test | `nix develop -c cargo test` (36 tests: 27 unit + 9 integration) |
| Format | `nix develop -c cargo fmt` (gate: `cargo fmt --check`) |
| Lint | `nix develop -c cargo clippy --all-targets` (gate: zero warnings) |
| Run | `target/debug/modelmux --config <path>` (the binary runs directly on the host) |
| Release build | `nix develop -c cargo build --release` |
| Nix package | `nix build .#modelmux` |

Definition of done for any change: build + all tests green + `fmt --check`
clean + clippy clean.

Commit style (no git identity is configured on this host):

```sh
git -c user.name=nicky -c user.email=nicky@local commit -m "..."
```

## Architecture

Module map (`src/`):

| Module | Responsibility |
|---|---|
| `main.rs` | CLI: config discovery, `--version`, logging init, bind listener |
| `lib.rs` | `app()`: the axum Router, the route table, the filter chain |
| `api.rs` | OpenAI surface (`/v1/chat/completions`, `/v1/models`, `/health`), the routing loop, failover, cost hook |
| `router.rs` | alias -> ordered candidate list (override -> policy -> default provider) |
| `filters.rs` | request filter chain (servlet-Filter equivalent): `request_id`, `access_log` |
| `time_windows.rs` | window evaluation: HH:MM ranges, day-of-week, per-window IANA tz, DST |
| `config.rs` | TOML schema (`Config`, `WindowCfg`, `ProviderCfg`, `PolicyCfg`, `OverrideEntry`) + validation |
| `anthropic.rs` | OpenAI <-> Anthropic translation: request, JSON response, SSE stream, tool calls |
| `upstream.rs` | provider dispatch: URL building, auth headers, `Attempt` classification |
| `state.rs` | `AppState`: mtime-based hot reload, overrides persistence, HTTP client |
| `usage.rs` | JSONL per-request log + in-memory stats |
| `sse.rs` | incremental SSE frame parser |
| `admin.rs` | `/admin/*` endpoints (routes, stats, reload, overrides) |

Request lifecycle for `POST /v1/chat/completions` (full detail in
`SPEC.md`): filter chain -> hot-reload check -> api_key gate -> parse ->
`router::resolve` -> candidate loop (translate -> `upstream::send` ->
classify) -> serve (stream or JSON) -> usage record.

Invariants — do not break:

- The agent side is OpenAI-only. Provider model ids never reach agents.
- Every response carries `x-request-id`; chat responses additionally
  carry `x-modelmux-provider` and `x-modelmux-alias`.
- OpenAI-kind upstream streams pass through byte-identical (there is a
  test for this).
- A broken config file never kills a running server: hot reload keeps the
  previous config and logs the error.
- Overrides persist atomically (tmp file + rename) to `overrides.json`.
- A started stream is never re-routed. Failover stops at the first byte.

## Hard-won facts (verified in this codebase; do not re-derive)

- axum 0.8: path captures are `{param}`, not `:param` (the router panics
  at construction otherwise).
- Layer order in `app()`: the LAST declared `.layer(...)` is the
  OUTERMOST. Pinned by `filters::tests::layer_chain_order_contract`. If
  you change the chain, keep that test green and update the comment in
  `lib.rs` and `SPEC.md` §10.
- Multi-value response headers: `HeaderValue::to_str()` returns only the
  FIRST value. Read all values with `get_all(...)`.
- Window convention: `start == end` means the full 24h. `00:00`-`23:59`
  does NOT (it excludes minute 1439). Use `00:00`/`00:00` for always-on
  windows in tests and configs.
- `window_active_at(w, tz, now_min)` reads the day-of-week from the real
  clock in `tz`, not from `now_min`. Only the minute range uses the passed
  value. Tests with day filters therefore read `now_weekday_in` themselves.
- DeepSeek peak windows are UTC. The example config pins `tz = "UTC"` on
  them. Do not convert them to local time: Brussels is CET (+1) in winter
  and CEST (+2) in summer, so a static local-time window would be wrong by
  an hour half the year.
- chrono: `num_seconds_from_midnight()` needs `Timelike` in scope.
- Admin handlers are `pub(crate)`. Making them `pub` triggers the
  `private_interfaces` lint (they use `pub(crate)` types).
- `cargo fmt` reflows multi-line expressions. Re-read a file after any fmt
  run before doing a targeted edit; the old text will not match.
- Stop the smoke server with `pkill -x modelmux`. Never `pkill -f
  <config path>`: the pattern also matches the spawning shell and kills
  your session.

## External facts (verified 2026-11-05; re-verify before trusting months later)

- DeepSeek official peak pricing: Mon-Fri 01:00-04:00 and 06:00-10:00
  UTC, exactly 2x, in force since 2026-08-16, excluding Chinese public
  holidays (NOT modeled by modelmux). Source:
  api-docs.deepseek.com/quick_start/pricing.
- Off-peak prices per 1M tokens (what the example config encodes as base,
  peak = base x window mult): deepseek-flash $0.15 in / $0.60 out (cache
  hit $0.003 in); deepseek-v4-pro $0.66 in / $1.98 out (cache hit $0.022
  in).
- Provider endpoints and model ids: see `config.example.toml` (annotated).
  Z.ai OpenAI `api.z.ai/api/coding/paas/v4`, Z.ai Anthropic
  `api.z.ai/api/anthropic`, Kimi `api.kimi.com/coding/v1`, DeepSeek
  `api.deepseek.com`. The legacy upstream model names `deepseek-chat` /
  `deepseek-reasoner` were retired (2026-07-24); do not add them.
- Kimi terms require the real client `User-Agent` to pass through to the
  upstream. `upstream::send` forwards it for all providers.
- Brussels offset: UTC+1 in winter, UTC+2 in summer. DeepSeek peak in
  Brussels local time: winter 02:00-05:00 and 07:00-11:00; summer
  03:00-06:00 and 08:00-12:00.

## Smoke test (no real keys needed)

```sh
cd ~/code/modelmux
nix develop -c cargo build
cp config.example.toml /tmp/modelmux-smoke.toml
DEEPSEEK_API_KEY=fake ./target/debug/modelmux --config /tmp/modelmux-smoke.toml &
curl -s localhost:8787/health
curl -s localhost:8787/admin/routes | jq    # window states, active candidates
curl -s -X POST localhost:8787/v1/chat/completions \
  -H 'content-type: application/json' \
  -d '{"model":"deepseek-chat","messages":[{"role":"user","content":"ping"}]}'
# Expected: DeepSeek's own 401 passes through (proves the full path)
# and a line is appended to the usage log.
pkill -x modelmux
```

## File map

| Path | What |
|---|---|
| `src/` | library + binary (module map above) |
| `tests/integration.rs` | end-to-end tests with mock upstream servers |
| `config.example.toml` | annotated configuration schema; source of truth for field names |
| `SPEC.md` | functional specification (behavior contract) |
| `README.md` | user-facing documentation |
| `env.example` | API-key env var names (never commit real keys) |
| `flake.nix`, `flake.lock` | package (rustPlatform), devShell, `nixosModules.default` |
| `nixos-module.nix` | `services.modelmux` NixOS options (systemd system service) |
| `systemd/modelmux.service` | systemd user unit template |
| `usage.jsonl`, `overrides.json` | runtime artifacts, gitignored; never commit |

## Agent plugins (opencode.json)

`opencode.json` enables 11 plugins from the sibling repo
`../llm-harness-plugins/`. The array order is a contract, not decoration —
it is documented in that repo's README (load-order table) and probed by its
`shared/canary_preflight.sh`. Hard constraints for the set in use:

- `guardrail-chain` before `session-lifecycle` before `agentmem`
- `agentmem` before `tier-router` (reads its `MEMORY.md`) and
  `agentinsights` (creates the `.agentmem/` root)
- `context-editing` after `session-lifecycle` and `datetime-inject`
- `general-skills` last among skill providers; `systemone`,
  `codify-first`, `context-editing` load after it

After any change to the array, verify (exit 0 = all loaded, in config
order; exit 1 = fail closed, do not work on):

```sh
bash ~/code/llm-harness-plugins/shared/canary_preflight.sh opencode ./opencode.json
```

Not enabled, on purpose: `ivp` (refactoring methodology, not requested for
this project), `rust-toolkit` (its `rust-check` tool runs bare `cargo`,
which does not exist on this host — `nix develop -c cargo ...` is the only
working toolchain path), `local-decide` (dormant until `laya-mcp` runs),
and the format toolkits (typst/latex/js/c/python/kotlin/graphics).

Model pins: ten general-skills audit agents declare `model: sonnet` in
their frontmatter, but `shared/agents.ts` DROPS bare model aliases at
registration (only `provider/model` survives), so they would fall back to
the session default (a 1.5B local model). The `agent` block pins them to
`deepseek/deepseek-v4-pro` (same precedent as the IVP pins in
nixos-config). A repo-declared agent REPLACES the plugin's registration
(`registerAgentDirV2` skips already-declared ids), so each pin carries the
full agent: `prompt` via `{file:...}`, `mode`, and the frontmatter's
permission block. `proof-soundness-auditor` and `translation-auditor`
keep their resolvable frontmatter models and need no pin.

`instructions` injects `agentmem/prompts/agent-prompt.md` — the memory
system's contract for the main agent (design doc: "injected into the build
agent's context"; the repo's sample config wires it the same way).
Verified live: a headless probe on the local default model answers
"12000", the token ceiling stated only in that file.

## Conventions

- Rust 2021, stable. Keep clippy + rustfmt clean on every change.
- New dependencies need justification before adding. Current stack:
  axum 0.8, tokio, reqwest (rustls), chrono + chrono-tz, serde, toml,
  tracing, uuid; dev: tower (util, for the layer-ordering test).
- Tests: unit tests live in each module's `#[cfg(test)]`; end-to-end tests
  live in `tests/integration.rs` (mock upstreams on 127.0.0.1:0 + a real
  proxy instance). Keep them fast; tests must not hit the network.
- Docs stay in sync with the code: `SPEC.md` on behavior change,
  `README.md` on user-visible change, `config.example.toml` on schema
  change.
