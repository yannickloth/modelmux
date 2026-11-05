# modelmux

Rule-based LLM routing proxy. One OpenAI-compatible endpoint for your agents.
Many backends behind it.

modelmux sits between your coding agents and your model providers:

```
agent (.md files unchanged)
  │  POST /v1/chat/completions  (model: "glm-5.3-flash")
  ▼
modelmux (127.0.0.1:8787)
  │  policy lookup + time windows + failover
  ├──▶ Z.ai GLM Coding Plan      (subscription, OpenAI or Anthropic protocol)
  ├──▶ Kimi Code                 (subscription, OpenAI or Anthropic protocol)
  ├──▶ DeepSeek API              (pay-per-use, peak windows cost 2x)
  ├──▶ Strata                    (local, OpenAI-compatible)
  └──▶ Unsloth Studio            (local, OpenAI-compatible)
```

## What it does

- **Stable model names.** Agents keep sending `glm-5.3-flash`,
  `deepseek-chat`, `k3`, … modelmux rewrites the model id per provider.
- **Time-window rules.** DeepSeek rates are 2x during its peak windows
  (01:00–04:00 and 06:00–10:00 UTC, Mon–Fri). A policy can avoid those
  windows and fall to a subscription model. Windows carry a day-of-week
  list, a price multiplier, and an IANA timezone.
- **Failover.** On connect errors, timeouts, 429 and 5xx (before the first
  byte), modelmux tries the next candidate.
- **Local switching.** Pin any alias to a local server at runtime with one
  `curl` (persisted), or edit the config. No agent files change.
- **Protocol translation.** The proxy speaks OpenAI chat completions to
  agents. Providers may be OpenAI-compatible or Anthropic-format
  (`kind = "anthropic"`); modelmux translates requests, responses, tool
  calls and SSE streams in both directions.
- **Accounting.** Every request is logged (tokens, cost, latency) to a JSONL
  file and aggregated in memory. Costs apply the active price multiplier.

## Quickstart

```sh
cp config.example.toml ~/.config/modelmux/config.toml
cp env.example ~/.config/modelmux/env   # fill in your keys
chmod 600 ~/.config/modelmux/env
```

Run:

```sh
# foreground (dev)
nix develop -c modelmux -- ~/.config/modelmux/config.toml 2>/dev/null \
  || nix develop -c cargo run -- --config ~/.config/modelmux/config.toml

# or, after `nix build`:
$(nix build .#modelmux -o /dev/null 2>/dev/null || true)/bin/modelmux --config ~/.config/modelmux/config.toml
```

Verify:

```sh
curl -s localhost:8787/health
curl -s localhost:8787/admin/routes | jq
# check the live routing for an alias, e.g.:
curl -s -X POST localhost:8787/v1/chat/completions \
  -H 'content-type: application/json' \
  -d '{"model":"deepseek-chat","messages":[{"role":"user","content":"hi"}]}'
```

## Configuration reference

See `config.example.toml` for a complete annotated example.

| Key | Meaning |
|---|---|
| `[server] listen` | Bind address. Default `127.0.0.1:8787`. |
| `[server] api_key` | Optional. Require `Authorization: Bearer <key>`. |
| `[server] usage_log` | JSONL path for per-request accounting. |
| `[server] default_provider` | Passthrough provider for unknown model names. |
| `[server] window_timezone` | Default zone for window evaluation: `utc`, `local`, or an IANA zone (e.g. `Europe/Brussels`). |
| `[server] default_max_tokens` | max_tokens for Anthropic providers when clients omit it. |
| `[windows.<name>]` | `start`/`end` (`HH:MM`), optional `days`, `mult` (price factor), `tz` (IANA zone; default: `server.window_timezone`). |
| `[providers.<name>]` | `kind` (`openai`\|`anthropic`), `base_url`, `api_key_env`, `price_windows`, per-model prices. |
| `[policies.<alias>]` | Ordered `candidates`; each has `provider`, `model`, optional `avoid_windows`, `only_windows`. |

Routing order for a request:

1. Admin override for the alias (if set), then
2. policy candidates whose window conditions pass (missing API keys skip),
3. `default_provider` passthrough for aliases with no matches.

The first live candidate is used. Failures fall through the same list.

### Windows, timezones and DeepSeek pricing

The official DeepSeek rate card (checked 2026-11-05): off-peak rates are
half of peak. Peak = 01:00–04:00 and 06:00–10:00 UTC, Mon–Fri, excluding
Chinese public holidays. `config.example.toml` encodes these as two windows
with `mult = 2.0`. The cost logger multiplies configured per-model prices
by the active windows, so `usage.jsonl` shows real peak vs off-peak spend.

Timezones are IANA names (`Europe/Brussels`, `UTC`, …), plus the shorthands
`utc` and `local` (system clock). Each window evaluates in its own `tz`;
without one, it uses `server.window_timezone`. Two conventions:

- `start == end` means the full 24h day (e.g. `00:00`/`00:00` is always
  active; `00:00`–`23:59` would leave the last minute free).
- Windows may cross midnight (`start > end` wraps).

Why per-window timezones: a local-time window in a DST zone slides one hour
twice a year. DeepSeek's schedule is defined in UTC, so those windows pin
`tz = "UTC"` and stay exact year-round, while `server.window_timezone`
defaults to `Europe/Brussels` for your own local windows.

Brussels equivalents of the DeepSeek peak windows (computed with
`python3`/`zoneinfo`, 2026-11-05):

| Window (UTC) | Winter (CET, UTC+1) | Summer (CEST, UTC+2) |
|---|---|---|
| 01:00–04:00 | 02:00–05:00 | 03:00–06:00 |
| 06:00–10:00 | 07:00–11:00 | 08:00–12:00 |

"Mon–Fri" is interpreted as UTC days (the official wording and OpenRouter's
machine-readable schedule both read that way). The Chinese public-holiday
exclusion is not modeled: on a Chinese holiday the real rate is off-peak,
so your peak-day routing may over-swap to the subscription for a few hours
at most.

### Switching a workload to local

```sh
# pin one alias to a local provider until you clear it:
curl -s -X POST localhost:8787/admin/override \
  -H 'content-type: application/json' \
  -d '{"model":"deepseek-chat","provider":"strata","model_id":"your-local-model"}'

curl -s -X DELETE localhost:8787/admin/override/deepseek-chat
```

Overrides persist to `overrides.json` next to the config and survive
restarts. Editing the config file works too: modelmux checks the file mtime
on every request and reloads automatically (a broken file is rejected and
the previous config is kept).

## Endpoints

| Endpoint | Purpose |
|---|---|
| `POST /v1/chat/completions` (also `/chat/completions`) | OpenAI chat completions, streaming and not. |
| `GET /v1/models` | List of routable aliases. |
| `GET /health` | Liveness + version. |
| `GET /admin/routes` | What each alias resolves to right now, window states. |
| `GET /admin/stats` | Requests, errors, tokens, cost per (alias, provider). |
| `GET /admin/overrides` | Current pinned overrides. |
| `POST /admin/override` | Pin an alias: `{"model","provider","model_id"?}`. |
| `DELETE /admin/override/<model>` | Clear a pin. |
| `POST /admin/reload` | Force config + overrides reload. |

Every response carries `x-request-id`. Chat-completions responses also
carry `x-modelmux-provider` and `x-modelmux-alias` so you can verify
routing from any client.

## Filters (request pipeline)

Every request — OpenAI surface and admin surface — passes through a chain
of filters before it reaches the endpoint handler, and back out through
it. This is the servlet-`Filter` equivalent, built on axum layers
(`src/filters.rs`).

The current chain, outermost first:

| Filter | What it does |
|---|---|
| `access_log` | One tracing line per request: method, path, status, duration. Target `modelmux.access` (silence with `RUST_LOG=modelmux.access=warn`). |
| `request_id` | Propagate a client `x-request-id`, or generate a UUID v4. Echoed in the response; stashed in the request extensions for handlers. |
| `DefaultBodyLimit` | Reject bodies above 64 MiB with 413. |

A unit test in `filters.rs` pins the layer-ordering contract against the
axum version in use: the **last** `.layer(...)` declared in `app()` is the
**outermost** (runs first in the request direction, last in the response
direction), and every layer's request and response mutations reach the
client.

To add a filter, write an async function in `src/filters.rs`:

```rust
use axum::extract::Request;
use axum::middleware::Next;
use axum::response::Response;

async fn my_filter(req: Request, next: Next) -> Response {
    // request direction: read or mutate headers, extensions, ...
    let mut resp = next.run(req).await;
    // response direction: read or mutate the response
    resp
}
```

and install it with one line in `app()` (`src/lib.rs`), at the position
you want in the chain:

```rust
Router::new()
    // ...routes...
    .layer(middleware::from_fn(filters::my_filter))
    .layer(middleware::from_fn(filters::access_log))
    .with_state(state)
```

- Filters need shared state (config, counters)? Use
  `middleware::from_fn_with_state(state.clone(), f)`; `f` then takes
  `State<Arc<AppState>>` as its first parameter.
- Scope a filter to part of the routes (e.g. only `/admin/*`)? Use
  `.route_layer(...)` on the route instead of `.layer(...)` on the router.
- Do not buffer the request body in a filter: pass `req` straight to
  `next.run(req)` so streaming stays intact.

## Agent integration

Point your harness provider config at the proxy once. Agent markdown files
keep their model names.

OpenCode (`opencode.json`):

```json
{
  "provider": {
    "zai-coding-plan": {
      "options": { "baseURL": "http://127.0.0.1:8787/v1" }
    }
  }
}
```

Keep the same model ids in your agent `.md` files (e.g.
`model: zai-coding-plan/glm-5.3-flash`). The `baseURL` change is the only
edit needed. If your harness does not let you override the baseURL of an
existing provider, define a custom OpenAI-compatible provider entry with the
same model ids.

Note for Kimi: its terms require keeping the real client identity.
modelmux forwards the client `User-Agent` header to upstreams.

## Building

```sh
nix develop -c cargo build --release
nix develop -c cargo test
nix develop -c cargo clippy --all-targets
nix build .#modelmux
```

## Running as a service

Systemd user unit: `systemd/modelmux.service` (adjust `ExecStart` to your
binary path). NixOS module (flake output `nixosModules.default`):

```nix
{
  inputs.modelmux.url = "path:../modelmux";
  services.modelmux = {
    enable = true;
    package = inputs.modelmux.packages.${pkgs.system}.default;
    config = ./config/modelmux.toml;
    environmentFile = ./secrets/modelmux.env;
  };
}
```

## Known limitations

- OpenAI chat completions only on the client side. No OpenAI Responses API
  or `/completions` legacy endpoint.
- Cost accounting ignores cache-hit input pricing and reasoning-token
  surcharges; it uses your configured base prices x active window factors.
- Stream failover stops once the first byte is sent (a stream cannot be
  re-routed mid-flight).
- Binding is your responsibility. Keep it on `127.0.0.1` or add
  `[server] api_key`.
- Anthropic translation covers text, images (URL/base64), tool calls,
  tool_choice, stop, max_tokens, temperature, top_p, streaming and usage.
  `thinking` blocks map to `reasoning_content` deltas. Provider-specific
  fields (e.g. DeepSeek `thinking`, GLM `reasoning_effort`) pass through
  untouched on OpenAI-kind providers.
