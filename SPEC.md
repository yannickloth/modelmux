# modelmux — Functional Specification

Version 0.1.0 · Baseline: commit `dae39eb` (2026-11-06) · Status: implemented

This document describes the behavior of modelmux. When code and this
document disagree, the tests are the authority; then update this document.

## 1. Purpose

modelmux is a reverse proxy for LLM APIs. Coding agents connect to it as
if it were an OpenAI-compatible API. modelmux decides which provider and
which model id each request actually reaches. Decisions come from a
declarative TOML config: policies (ordered candidate lists), time windows
(e.g. DeepSeek's 2x peak pricing), and runtime overrides. Agents never see
provider details.

Design constraints:

- Single static binary. No database, no external state beyond the config
  file, the API-key environment variables, and the files it writes.
- Agent files (model names in `.md`) never change. Only the agent
  harness's `baseURL` points at modelmux once.
- The agent-facing surface is OpenAI chat completions only.

## 2. Terms

| Term | Meaning |
|---|---|
| alias | the model name an agent sends (e.g. `deepseek-chat`) |
| provider | a configured upstream endpoint (name, kind, base URL, key env var) |
| candidate | one (provider, model id) pair in a policy, with optional window conditions |
| policy | the ordered candidate list bound to an alias |
| window | a named time range with a day-of-week list and a price multiplier |
| override | a runtime pin of an alias to a provider, set via the admin endpoint |
| attempt | one candidate send inside the routing loop |
| retryable | a failure that triggers the next candidate |

## 3. Configuration

### 3.1 Discovery

Config file resolution order:

1. `--config <path>` / `-c <path>`
2. `$MODELMUX_CONFIG`
3. `./modelmux.toml`
4. `./config.toml`
5. `~/.config/modelmux/config.toml`

Missing file is a startup error.

### 3.2 `[server]`

| Field | Default | Meaning |
|---|---|---|
| `listen` | `127.0.0.1:8787` | bind address |
| `api_key` | — | if set, all requests must carry `Authorization: Bearer <key>` |
| `usage_log` | — | append-only JSONL request log path |
| `overrides_path` | next to the config | where admin overrides persist |
| `default_provider` | — | passthrough provider for aliases with no policy match |
| `default_max_tokens` | `8192` | `max_tokens` for Anthropic-kind providers when the client omits it |
| `window_timezone` | `utc` | default window-evaluation timezone: `utc`, `local`, or an IANA zone |

### 3.3 `[windows.<name>]`

| Field | Default | Meaning |
|---|---|---|
| `start`, `end` | — | `HH:MM` (24h clock), in the window's timezone |
| `mult` | `1.0` | price multiplier inside the window |
| `days` | all days | subset of `Mon`..`Sun` (3-letter or full names, case-insensitive) |
| `tz` | `server.window_timezone` | IANA zone, `utc`, or `local` for this window |

Semantics:

- The range is start-inclusive, end-exclusive: `now >= start && now < end`.
- `start == end` means the full 24h (all 1440 minutes).
- `start > end` wraps midnight (e.g. 23:00 -> 01:00).
- The weekday is evaluated in the window's timezone, from the current
  clock.
- A window is ACTIVE when its range matches the current minute in its
  timezone AND (when `days` is non-empty) the current weekday is in
  `days`.

### 3.4 `[providers.<name>]`

| Field | Default | Meaning |
|---|---|---|
| `kind` | — | `openai` or `anthropic` |
| `base_url` | — | endpoint base, without trailing path |
| `api_key_env` | — | env var holding the key. Missing or empty: candidates for this provider are skipped |
| `price_windows` | — | window names whose `mult` applies to cost accounting |
| `models.<id>` | — | base prices per 1M tokens (`input_per_mtok`, `output_per_mtok`), off-peak |
| `auth_style` | `bearer` | `bearer` (`Authorization`) or `api_key` (`x-api-key`) |
| `anthropic_version` | `2023-06-01` | value of the `anthropic-version` header |

URL construction:

- `openai`: `{base_url}/chat/completions`
- `anthropic`: `{base_url}/v1/messages`

### 3.5 `[policies.<alias>]`

An ordered list of candidates. Each candidate:

| Field | Default | Meaning |
|---|---|---|
| `provider` | — | provider name |
| `model` | — | model id sent to that provider |
| `avoid_windows` | — | skip this candidate while any of these windows is active |
| `only_windows` | — | use this candidate only while at least one of these windows is active |

### 3.6 Validation

At startup and on every reload the config must satisfy:

- All `start`/`end` values are valid `HH:MM`.
- All day names are `Mon`..`Sun`.
- `window_timezone` and every window `tz` are `utc`, `local`, or a known
  IANA zone.
- Every referenced window (provider `price_windows`, candidate
  `avoid_windows`/`only_windows`) is defined.
- Every referenced provider exists. `default_provider` exists.
- Every policy has at least one candidate.

Startup failure: the process exits and lists all errors. Reload failure:
the previous config stays in effect and the error is logged.

### 3.7 Hot reload

On every request, modelmux stats the config file and the overrides file.
When either mtime advanced, it reloads. No admin call is required to pick
up a config change.

## 4. Endpoints

### 4.1 `POST /v1/chat/completions` (also `/chat/completions`)

The endpoint agents use. Body: an OpenAI chat-completions request.

Processing order:

1. Hot-reload check.
2. `api_key` gate when configured: 401 on mismatch.
3. Parse JSON (400 on failure). Read `model` (400 when missing or empty),
   `stream`, `stream_options.include_usage`, the client `User-Agent`.
4. Resolve the candidate list (section 5). Empty list: 404.
5. Routing loop, first success wins (section 5.2).
6. Serve the response (4.1.1).
7. Record usage (section 8).

#### 4.1.1 Response

- Non-stream: the provider's JSON, translated when the provider is
  Anthropic-kind (section 7).
- Stream: byte-identical passthrough for OpenAI-kind providers;
  frame-by-frame translation for Anthropic-kind (section 7.3).
- Headers on all chat responses: `x-request-id`, `x-modelmux-provider`,
  `x-modelmux-alias`.
- A started stream is never re-routed: failover stops at the first byte.

### 4.2 `GET /v1/models` (also `/models`)

The sorted list of policy aliases:
`{id, object: "model", created: 0, owned_by: "modelmux"}`.

### 4.3 `GET /health`

`{status: "ok", version}`.

### 4.4 Admin endpoints (local only)

| Endpoint | Behavior |
|---|---|
| `GET /admin/routes` | Live resolution: current minute in the default tz; every window with its `tz` and `active_now`; every alias with its candidates and their `active` state. Overrides come first, marked `via_override`. |
| `GET /admin/stats` | In-memory aggregates: totals plus per (alias, provider). |
| `POST /admin/reload` | Force reload of config + overrides. Invalid config: 400 with the errors. |
| `POST /admin/override` | Body `{model, provider, model_id?}`: pin an alias. `model_id` defaults to the alias. Unknown provider: 400. Persisted. |
| `GET /admin/overrides` | The current pin map. |
| `DELETE /admin/override/{model}` | Unpin. Unknown pin: 404. |

Overrides persist to `overrides_path` (atomic write: tmp file + rename) and
survive restarts.

## 5. Routing

### 5.1 Resolution

`resolve(alias)` builds the ordered candidate list:

1. The override for the alias (when the pinned provider is configured).
2. The policy candidates in declared order, skipping:
   - candidates whose window condition fails right now,
   - candidates whose provider's key env var is missing or empty.
3. When the list is still empty: the `default_provider` passthrough
   (model id unchanged), when configured with a key present.

Window condition: a candidate is ACTIVE when none of its `avoid_windows`
is active AND (when `only_windows` is non-empty) at least one of them is
active. Each window evaluates in its own timezone (section 6).

### 5.2 Attempt loop

For each candidate, in order:

1. Copy the request body; set `model` to the candidate's model id.
2. When the provider is Anthropic-kind, translate the body (section 7.1).
   A translation error skips the candidate (recorded in the attempt list).
3. Send (section 5.3).
4. Classify the result:
   - **Ok** (2xx): serve it. Done.
   - **Retryable**: transport error (connect failure, timeout) or status
     408, 425, 429, 500, 502, 503, 504, before the first response byte:
     try the next candidate.
   - **Fatal**: any other non-2xx status (in practice 4xx): surface to
     the client immediately (translated when Anthropic-kind, section
     7.4).
5. When no candidate succeeds: 502, with a message listing all attempts.

### 5.3 Send

- URL, auth header, and `anthropic-version` (Anthropic-kind) per section
  3.4.
- The client `User-Agent` is forwarded to the upstream (Kimi terms
  require the real client identity).
- Timeouts: connect 5s, read 300s. Connection pool: 8 idle per host.

## 6. Time windows and timezones

- Timezones are IANA names (`Europe/Brussels`, `UTC`) plus the shorthands
  `utc` and `local` (system clock). Unknown names are validation errors at
  load time.
- Each window evaluates in its own timezone: `window.tz` when set, else
  `server.window_timezone`.
- Why per-window timezones: a local-time window in a DST zone drifts one
  hour twice a year. DeepSeek's schedule is defined in UTC, so those
  windows pin `tz = "UTC"` and stay exact year-round.
- Weekday: ISO (Mon = 1 .. Sun = 7), evaluated in the window's timezone.
- DeepSeek peak (external fact, see AGENTS.md): Mon-Fri 01:00-04:00 and
  06:00-10:00 UTC, 2x.

## 7. OpenAI <-> Anthropic translation

Applies only to providers with `kind = "anthropic"`. OpenAI-kind providers
pass the body through unchanged.

### 7.1 Request (OpenAI -> Anthropic)

| OpenAI | Anthropic |
|---|---|
| `messages[role=system\|developer]` | `system` (joined with blank lines) |
| `messages[role=user]` (string or parts) | user message: `text` blocks; `image_url` parts become `image` blocks (data-URI base64 or URL) |
| `messages[role=assistant]` | assistant message: `text` block + one `tool_use` block per `tool_calls` entry (JSON-string `arguments` parsed to an object) |
| `messages[role=tool]` | user message with a `tool_result` block (`tool_call_id` -> `tool_use_id`) |
| `tools[]` (`type = "function"`) | `{name, description, input_schema: parameters}` |
| `tool_choice` | `none` -> `{type:none}`; `required` -> `{type:any}`; other strings -> `{type:auto}`; `{type:function, function:{name}}` -> `{type:tool, name}` |
| `stop` (string or array) | `stop_sequences` |
| `temperature`, `top_p` | passed through |
| `max_tokens` (else `server.default_max_tokens`) | `max_tokens` (minimum 1) |
| `stream: true` | `stream: true` |
| `model` | the candidate's model id, set before translation |

A request with no usable message after translation is an error; the
candidate is skipped.

### 7.2 JSON response (Anthropic -> OpenAI)

- `content[]` `text` blocks -> `message.content` (concatenated).
- `content[]` `tool_use` blocks -> `message.tool_calls[]` (`arguments`
  re-serialized as a JSON string).
- `message.content` is null when only tool calls are present.
- `stop_reason` mapping: `max_tokens` -> `length`; `tool_use` ->
  `tool_calls`; `refusal` -> `content_filter`; anything else -> `stop`.
- `usage`: `prompt_tokens` = `input_tokens` + `cache_read_input_tokens` +
  `cache_creation_input_tokens`; `completion_tokens` = `output_tokens`.
- `model` is set to the alias, not the upstream id.

### 7.3 SSE stream (Anthropic -> OpenAI)

| Anthropic event | OpenAI output |
|---|---|
| `message_start` | capture usage (input incl. cache); emit the first chunk if needed |
| `content_block_start` (tool_use) | `tool_calls` delta: index, id, name, empty arguments |
| `content_block_delta` (text_delta) | `content` delta |
| `content_block_delta` (input_json_delta) | `tool_calls` delta: arguments fragment |
| `content_block_delta` (thinking_delta) | `reasoning_content` delta |
| `message_delta` | capture output tokens + finish reason |
| `message_stop` | finish chunk (empty delta + finish_reason), optional usage chunk when `stream_options.include_usage`, then `data: [DONE]` |
| `error` | one error frame + `data: [DONE]` |
| `ping` | ignored |

The finish chunk is always emitted (finish_reason defaults to `stop`) even
when the upstream stream ends without `message_stop`.

### 7.4 Error mapping (Anthropic -> OpenAI)

`{error: {message, type}}` maps to the OpenAI error object
`{error: {message, type, code: <http status>}}`, with the original status
preserved.

## 8. Usage accounting

Every request produces exactly one usage record, success or failure:
`ts` (RFC3339, ms, UTC), `alias`, `provider`, `model`, `status`, `stream`,
`prompt_tokens`, `completion_tokens`, `cost_usd`, `duration_ms`,
`attempts`, `error`.

- Written as one line to the JSONL file (append-only, best effort: a write
  failure does not affect the response).
- Aggregated in memory per (alias, provider); exposed at
  `GET /admin/stats`.

Cost formula (only when both token counts and the model's base prices are
known):

```
cost = (input_per_mtok * prompt/1e6 + output_per_mtok * completion/1e6)
       * product(mult of every ACTIVE price_window)
```

Each window is checked in its own timezone. Cache-hit input pricing is not
modeled.

## 9. Error model

| Status | Condition |
|---|---|
| 400 | invalid JSON body; missing or empty `model`; (admin) unknown provider in an override |
| 401 | `server.api_key` set and the request's key does not match |
| 404 | no candidate for the alias (no policy match, no usable default provider); (admin) no such override |
| 413 | request body over 64 MiB (body-limit filter) |
| passthrough | Fatal upstream status (in practice 4xx), translated when Anthropic-kind |
| 502 | every candidate failed (the message lists the attempts) |

All error responses are JSON: `{error: {message, type, code?}}`.

## 10. Filter chain

Every request passes through the layer chain in `app()` (see `lib.rs`) on
the way in and on the way out. Current chain, outermost first:

1. `access_log` — one tracing line per request (method, path, status,
   duration), target `modelmux.access`.
2. `request_id` — propagate the client's `x-request-id` or generate a
   UUID v4; echoed in the response; stashed in the request extensions.
3. `DefaultBodyLimit` — 64 MiB.

Contract (pinned by `filters::tests::layer_chain_order_contract`): the
last declared `.layer(...)` is the outermost; every layer's request and
response mutations reach the client.

Rules for filters:

- Do not buffer the request body (streaming must stay intact).
- Keep per-request work minimal; the chain runs on every endpoint.
- When the chain changes, update the comment in `lib.rs`, this section,
  and the README.

## 11. Security

- Default bind is loopback. Binding elsewhere is the operator's decision;
  set `server.api_key` when doing so.
- The admin surface has no dedicated auth; it relies on bind discipline.
  Do not expose it on a network.
- API keys come only from environment variables, named in the config. Keys
  are never written to config, logs, or responses.
- The client `User-Agent` is forwarded to upstreams (required by Kimi).

## 12. Test baseline

- 27 unit tests, per module: windows and timezones, tz resolution,
  routing, translation (request, JSON, SSE), SSE parsing, upstream
  classification, filter-chain ordering contract.
- 9 integration tests: a real proxy instance against mock upstreams on
  127.0.0.1:0 — failover + model rewrite, window avoidance, Anthropic
  JSON + SSE translation (tool-call reassembly), byte-identical OpenAI SSE
  passthrough, override lifecycle, default provider, model listing,
  request-id on all surfaces.
- Gate: `cargo test` green + `cargo fmt --check` clean +
  `cargo clippy --all-targets` without warnings.

## 13. Limitations (known, accepted)

- OpenAI chat completions only on the agent side (no Responses API, no
  legacy `/completions`).
- Cost accounting ignores cache-hit input pricing and reasoning-token
  surcharges.
- Stream failover stops once the first byte is sent.
- The Chinese public-holiday exclusion of DeepSeek's peak schedule is not
  modeled.
- Anthropic translation: `thinking` blocks map to `reasoning_content`
  deltas. Provider-specific fields on OpenAI-kind providers pass through
  untouched.
