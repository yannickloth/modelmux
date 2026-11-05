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
  list and a price multiplier.
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
| `[server] window_timezone` | `utc` or `local` for window evaluation. |
| `[server] default_max_tokens` | max_tokens for Anthropic providers when clients omit it. |
| `[windows.<name>]` | `start`/`end` (`HH:MM`), optional `days`, `mult` (price factor). |
| `[providers.<name>]` | `kind` (`openai`\|`anthropic`), `base_url`, `api_key_env`, `price_windows`, per-model prices. |
| `[policies.<alias>]` | Ordered `candidates`; each has `provider`, `model`, optional `avoid_windows`, `only_windows`. |

Routing order for a request:

1. Admin override for the alias (if set), then
2. policy candidates whose window conditions pass (missing API keys skip),
3. `default_provider` passthrough for aliases with no matches.

The first live candidate is used. Failures fall through the same list.

### Windows and DeepSeek pricing

The official DeepSeek rate card (checked 2026-11-05): off-peak rates are
half of peak. Peak = 01:00–04:00 and 06:00–10:00 UTC, Mon–Fri, excluding
Chinese public holidays. `config.example.toml` encodes these as two windows
with `mult = 2.0`. The cost logger multiplies configured per-model prices
by the active windows, so `usage.jsonl` shows real peak vs off-peak spend.

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

Every response carries `x-modelmux-provider` and `x-modelmux-alias` headers
so you can verify routing from any client.

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
