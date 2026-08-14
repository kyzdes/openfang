# fangrig — FANG-52 test harness

A fake OpenAI-compatible LLM provider + a fake Telegram Bot API, scripted by
JSON scenarios, running as containers inside the staging OpenFang
container's own network namespace. One command up, one command down, every
call journalled to a plain file you can `cat`.

```
fangrig up ok                    # bring up the control scenario
fangrig agent probe              # spawn a probe agent pointed at the stub
fangrig say probe "2+2"          # send it a message, see the scripted reply
fangrig journal                  # what actually hit the stub
fangrig down                     # tear everything back down
```

## Read this before you do anything else

1. **Never run a stub as a process on the host.** Traffic from the staging
   container to the host is dropped by the firewall — a timeout, not a
   refused connection — and `OpenAIDriver`'s reqwest client has no
   `.timeout()` (`openai.rs:35-38`). You will not see an error. The turn
   will hang forever. `fangrig up` always launches stubs as containers
   sharing the staging container's netns; that's the only supported way.
2. **`500` with non-empty `tools` never reaches the fallback chain.** The
   driver treats it as "this model doesn't support tools", strips `tools`,
   and retries the *same* endpoint (`openai.rs:713-731`). If you want to
   prove the fallback chain fires, use `502` (see `bad-gateway-502.json`).
   If you want to prove the tools-stripping retry, that's what
   `tools-500-absorbed.json` is for.
3. **`max_retries = 3`.** One turn can eat up to four steps off a single
   endpoint. Always set `repeat` explicitly in a scenario rather than
   relying on list length.
4. **Streaming is a separate code path with its own retries.** A scenario
   that's green on `/message` says nothing about `/message/stream`. Use
   `fangrig say --stream` and a `stream-*.json` scenario to exercise it.
5. **`cost_usd` in every run is synthetic** — the stub isn't priced in the
   real model catalog, so it falls back to the default tier.
6. **`--net share` (the default) dies with the container.** If staging
   restarts mid-run, the stub goes with it. `fangrig status` says so
   plainly; `fangrig up` again.
7. **The image has no `curl`/`wget`.** Every check that reaches into the
   container uses `python3 -c "import urllib.request..."`. Don't add a
   `curl` dependency inside the container path.
8. **A Telegram channel's `default_agent` resolves by name exactly once,
   at `POST /api/channels/reload`** (`channel_bridge.rs`, `build_bridge_manager`).
   If you wire `--tg` before the named agent exists, the reload can't find
   it and the router falls through to whatever the daemon's *persisted*
   bindings say instead — which, on a well-used staging box, is a real
   agent with a real model and real cost. `fangrig agent` reloads channels
   again after creating the agent for exactly this reason. If you ever see
   a scripted-Telegram scenario answer with prose that doesn't look like
   your `steps[]`, this is almost certainly why — check which agent
   actually answered before assuming the stub is broken.

## Layout

```
fangrig               entry point — see `fangrig --help` for the command list
lib.sh                 shared bash: prod guard, api(), container/netns helpers
llm_stub.py            fake OpenAI-compatible provider (multi-endpoint)
tg_stub.py              fake Telegram Bot API (getUpdates queue + file download)
scenarios/*.json       twelve scripted scenarios, one per defect shape
selftest.sh             thin wrapper for `fangrig selftest`
```

A run's evidence lives outside this directory, under `/tmp/fang-harness/<run_id>/`
(symlinked as `/tmp/fang-harness/current`): `scenario.json` (the copy actually
used, stamped with `run_id`), `journal.jsonl` / `tg-journal.jsonl` (every
call, one JSON object per line, `flush()`-ed immediately), `stub.log`, and
`state.json` (what's up, so `down` knows what to tear down). Nothing here
gets deleted by `down` — only the containers, the network, the provider key,
and `config.toml`.

## What's from where

`tg_stub.py` supersedes two prior probes, kept exactly as they are:

- `tests/fang/fake_telegram_api.py` — the single-reader `getUpdates` queue
  semantics ("newcomer wins", displaced poller gets 409 after
  `CONFLICT_DELAY`). That's `queue_policy: "single_reader"` here, off by
  default (`"permissive"`) because most scenarios don't need it.
- `tests/fang/FANG-43.sh`'s inline stub — `getFile` + the raw
  `/file/bot<token>/<path>` download route, matched *before* the
  `/bot<token>/<method>` route so a loose regex doesn't swallow the file
  path as a bogus method name.

Neither original file is modified or imported by this harness; they remain
what they always were — standalone, self-contained proof scripts for
FANG-31 and FANG-43 respectively.

## Scenarios

| File | Proves |
|---|---|
| `ok.json` | Control. If this isn't green, the harness is broken, not the product. |
| `auth-401-echo.json` | A LiteLLM-style 401 puts the caller's key back in its own error body, which the driver carries verbatim into `LlmError::Api{message: body}` and from there into the API response / session. |
| `bad-gateway-502.json` | `502` (not `500` — see gotcha #2) triggers the fallback chain. |
| `tools-500-absorbed.json` | `500` while `tools` is non-empty is absorbed as "no tool support", tools get stripped, the *same* endpoint is retried — fallback never sees it. |
| `hang.json` | No response, ever. The turn hangs forever (no client-side timeout — gotcha #1), it doesn't error out. |
| `no-choices.json` | Empty `choices` → `LlmError::Parse("No choices in response")`. |
| `toolcall-then-text.json` | Ordinary two-iteration tool-call turn, one endpoint. |
| `split-turn.json` | fallback answers iteration 1 (tool_call) and, via `then`, opens primary; primary answers iteration 2. Rollup after one turn must read exactly `{"primary":1,"fallback":1}` — that's the split-turn/mixed-iteration shape from sprint 2. |
| `stream-ok.json` | The `/message/stream` path answers at all (gotcha #4). |
| `stream-toolcall.json` | Streamed tool-call delta accumulation: first chunk must carry both `id` and `function.name` or the driver silently drops the call. |
| `stream-no-stream-options.json` | Provider 400s on `stream_options`; driver must retry without it. The rejected request doesn't consume a step. |
| `tg-file.json` | FANG-43 shape end to end: `getFile`, then the bot token landing verbatim inside the LLM prompt text (never an actual byte download for `document`/`voice` — that's the defect). Needs `--tg`; see gotcha #8 for the ordering that makes routing actually work. |

## Scenario schema, short version

```json
{
  "name": "split-turn",
  "endpoints": [ {
    "role": "fallback",           // journal/rollup key
    "port": 8971,                  // 8971-8979 only; 8981 reserved for telegram
    "prefix": "/fallback/v1",      // OpenFang calls {prefix}/chat/completions
    "open": true,                  // false = port not bound = instant ECONNREFUSED
    "require_auth": false,         // true => no Authorization header => 401
    "chunk_chars": 8, "chunk_delay_ms": 0,
    "stream_quirks": [],           // ["reject_stream_options"]
    "steps": [ /* ... */ ],
    "default_step": { "type": "text", "content": "exhausted" }
  } ],
  "telegram": { "port": 8981, "token": "...", "default_agent": "...",
                "queue_policy": "permissive", "updates": [] }
}
```

Each **step** has a `type` (`text` | `tool_call` | `status` | `empty_choices` |
`malformed` | `hang` | `close`), an optional `repeat` (default 1 —
deliberately *not* infinite, because `max_retries=3` means a `tool_call` at
the end of an unbounded list would loop the agent forever), and an optional
`then: {"open"|"close": "<role>"}` fired after the response is sent — the
mechanism `split-turn.json` uses to open `primary` right after `fallback`
answers. Calls past the end of `steps` always get `default_step`; give it
`type: "text"` if any step in the list is `tool_call`, or the loop never
terminates. `fangrig selftest` checks exactly this before every `up`.

`{{run_id}}`, `{{canary}}`, `{{role}}`, `{{seq}}`, `{{key_echo}}` are
substituted into `content`/`body_template`/`raw` at request time.
`{{key_echo}}` is **not** a template convenience — it's the reason
`auth-401-echo.json` is safe to run against a box with a real provider key
configured: it echoes back the received bearer token only if it equals this
run's canary (`of-fangrig-canary-<run_id>`); anything else — including a
real key that leaked in by operator error — is replaced with
`sk-...REDACTED-BY-FANGRIG` before it can appear in a response body or a
journal line. `fangrig selftest` checks this by sending a decoy token and
grepping for it in both places.

## Commands

```
fangrig up <scenario> [--net share|bridge] [--with-key] [--tg] [--no-selftest]
fangrig down [run_id]
fangrig agent <name> [--fallback] [--tools t1,t2]
fangrig say <agent> "<text>" [--stream]
fangrig tg-send <index|kind> [--file <name>]
fangrig journal [--json|--raw|--rollup]
fangrig selftest [run_id]
fangrig status
```

`--net share` (default) shares the staging container's network namespace —
addresses are always `127.0.0.1:PORT`, nothing about the staging container's
own networking changes, and it's what `FANG-31.sh`/`FANG-43.sh` already
used. `--net bridge` publishes ports to the host too (useful if you want to
`curl` the stub directly instead of going through `journal`/`tg-send`), at
the cost of actually mutating staging's network (a bridge network gets
created and connected to it, then disconnected and removed on `down`). Reach
for `bridge` only if you specifically need host access to the stub; it's had
far less mileage than `share`.

`--with-key` sets a synthetic key (`of-fangrig-canary-<run_id>`) on a
provider named `fangrig` via `POST /api/providers/fangrig/key` — no restart,
no `secrets.env` hand-editing (an unrecognized provider name with an
explicit `base_url` gets an OpenAI-compatible driver whose key may be empty
with no error, `drivers/mod.rs:555-565`; without `--with-key` no
`Authorization` header goes out at all, and the stub doesn't check for one
unless a scenario sets `require_auth`). `down` removes it and verifies it's
gone from both the live process and `/api/providers`.

`fangrig agent` always names the agent `fangrig-<name>` — that prefix is
what makes `down`'s safety-net sweep (by name, independent of `state.json`)
safe to run unconditionally.

## `selftest`

Runs automatically as the last step of `up` (skip with `--no-selftest`) and
is available standalone. Seven checks, any failure exits 6:

1. A request with model `SELFTEST-MUST-APPEAR-<run_id>` shows up in
   `journal.jsonl`. If it doesn't, the journal is lying, not the product.
2. A string that was never sent (`SELFTEST-MUST-NOT-APPEAR`) does **not**
   show up — same `grep -E` invocation as check 1, so a syntax mismatch
   between "must find" and "must not find" can't silently pass both.
3. A decoy bearer token never appears in the journal or in the stub's own
   response body (see `{{key_echo}}` above).
4. The same probe, requested with `stream:true`, comes back as
   `text/event-stream` with ≥3 `data:` events and a terminal `[DONE]`.
5. The stub answers `/__health` from inside the staging container within
   10s (the exact gate `up` itself uses before doing anything else).
6. No external domain string in either journal, and neither stub's source
   contains `urlopen`/`requests.*`/`socket.connect`/`http.client` — so a
   future edit can't quietly add an outbound call.
7. Every file in `scenarios/*.json` parses, has unique role names, ports in
   8971-8979, and any trailing `tool_call` step is backed by a
   `default_step` of type `text`.

**Selftest probes never touch a scenario's own step cursor or rollup
counts** — they're served off a synthetic step and logged to
`journal.jsonl` only, never through `rollup_note()`. Getting this wrong
was the harness's own first bug: an early version let `up`'s automatic
selftest silently eat the first N steps of whatever scenario it was
checking, so a real agent turn right after `up` landed on `default_step`
instead of `steps[0]`. If you ever see a rollup or a response that doesn't
match `steps[0]`, check for exactly this before assuming the product is at
fault.

A green scenario run is not proof of anything by itself — read what the
scenario actually asserts (the `doc` field) before trusting a result, and
run `ok.json` as your control if in doubt.

## Verified against a live defect

This harness was proven against staging, not just started:

- **`split-turn`**: one real turn through a `--fallback` agent produced
  rollup `{"primary": 1, "fallback": 1}` and an API response served by two
  different (fake) models in one turn — the split-turn/mixed-iteration
  shape from sprint 2, reproduced end to end.
- **`auth-401-echo` with `--with-key`**: the API's error response contained
  `of-fangrig-canary-<run_id>` verbatim, while the box's real
  `HYPERFUSION_API_KEY` was confirmed absent (0 occurrences) from both that
  response and the journal — the A-7 key-in-body shape, reproduced without
  any risk to the real key.
- **`tg-file` with `--tg`**: `getFile` and `full_token_seen:true` showed up
  in `tg-journal.jsonl`, and the LLM-facing journal recorded the bot token
  landing unredacted inside the prompt text — the FANG-43 shape, reproduced
  live.
