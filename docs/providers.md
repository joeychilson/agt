# Providers

agt speaks the Responses API to five providers. Each is one entry in
`src/provider.rs`, and the rest of agt reads those entries rather than naming
providers, so what differs between them, endpoints, sign-in, request dialects,
compaction and model catalogs, changes in one place.

## The five

| Provider          | `--provider` | Endpoint                        | Sign-in                                                        | Models                  |
| ----------------- | ------------ | ------------------------------- | -------------------------------------------------------------- | ----------------------- |
| OpenAI            | `openai`     | `api.openai.com/v1`             | API key, or `OPENAI_API_KEY`                                   | Known to agt            |
| ChatGPT           | `codex`      | `chatgpt.com/backend-api/codex` | Plus or Pro subscription, in the browser                       | Known to agt            |
| Grok              | `grok`       | `api.x.ai/v1`                   | SuperGrok or X Premium+ subscription, in the browser           | Known to agt            |
| OpenRouter        | `openrouter` | `openrouter.ai/api/v1`          | A key issued in the browser or pasted, or `OPENROUTER_API_KEY` | Listed at `GET /models` |
| Vercel AI Gateway | `vercel`     | `ai-gateway.vercel.sh/v1`       | API key, or `AI_GATEWAY_API_KEY`                               | Listed at `GET /models` |

## What a request carries

Every request is `POST {base}/responses` with `stream: true`, `store: false`,
the whole input, the session's instructions and the bash tool,
`include: ["reasoning.encrypted_content"]`, and `reasoning` with
`summary: "auto"` and the chosen effort. The rest differs, and each provider's
`Dialect` states it as how the provider differs from OpenAI:

|                         | OpenAI        | ChatGPT                     | Grok                 | OpenRouter              | Vercel AI Gateway                                  |
| ----------------------- | ------------- | --------------------------- | -------------------- | ----------------------- | -------------------------------------------------- |
| `prompt_cache_key`      | ✓             | ✓                           | ✓                    | —                       | ✓                                                  |
| Affinity header         | —             | `session-id`                | `x-grok-conv-id`     | `x-session-id`          | `x-session-affinity`                               |
| Asks for caching        | —             | —                           | —                    | `cache_control`, 1 h    | `caching` and `cache_ttl`, 1 h                     |
| `max_output_tokens`     | ✓             | —                           | ✓                    | ✓                       | ✓                                                  |
| Images in a tool result | In the result | In the result               | In the result        | In a user message after | In a user message after                            |
| Other headers           | —             | `openai-beta`, `originator` | —                    | —                       | —                                                  |
| Compaction              | Inline        | `/responses/compact`        | `/responses/compact` | Summaries               | `/responses/compact` for `openai/`, else summaries |

- **The session id keeps a conversation on its cache.** It is the cache key and
  the affinity header's value, so a conversation stays on the backend that holds
  its prompt cache.
- **Caching is asked for where a model needs it.** Anthropic's models cache
  only on request, so requests through the gateways always ask, with a one-hour
  lifetime that outlives long commands and pauses. Models that cache implicitly
  ignore it.
- **Images go where each provider reads them.** A tool result that shows images
  carries parts to OpenAI, ChatGPT and Grok: its text as `input_text` and each
  image as `input_image` with `detail: "high"`, as the Responses API documents.
  The gateways convert requests for other providers, and OpenRouter's
  conversion for Gemini on Vertex drops a tool output that holds an image. So to
  OpenRouter and Vercel a tool output is its text alone, and the images of a run
  of outputs follow it in one user message, as Chat Completions clients send
  them.

How each way of compacting works is in
[Architecture](architecture.md#context-and-compaction).

## Signing in

- **Keys come from the most specific place.** The credential is `AGT_API_KEY`,
  then the key `agt login` saved, then the provider's own variable. When
  `AGT_BASE_URL` replaces the endpoint, only `AGT_API_KEY` is sent, so a saved
  key never reaches an endpoint the environment chose.
- **Browser sign-in uses OAuth with PKCE** and a loopback redirect. ChatGPT
  signs in through `auth.openai.com` with the redirect registered for Codex
  clients, on port 1455. Grok signs in through `auth.x.ai` with the Grok CLI's
  client, and OpenRouter through `openrouter.ai/auth`, both redirecting to any
  free port; OpenRouter's code is exchanged for an ordinary key. The browser
  opens on its own, and when it runs on another machine, the address it ends on
  can be pasted instead.
- **Tokens refresh on the request's thread**, five minutes before they expire,
  or once after a 401 or the 403 xAI answers for a token it cannot validate. A
  ChatGPT refresh token works only once, and xAI issues a new one with each
  refresh, so agt processes take turns refreshing under a lock on `auth.lock`.
  A process that waited adopts the sign-in the other saved.
- **`auth.json` is private.** It maps provider ids to keys, or to access and
  refresh tokens, their expiry and ChatGPT's account. It is made readable only
  by the user before anything is written, and replaced by renaming a new file
  over it.

## Models and cost

- **Known catalogs.** agt knows OpenAI's current models, GPT-6 Astra and
  GPT-5.6 Sol, Terra and Luna, with their efforts and prices, a 1.05M window and
  a 272K pricing tier. A ChatGPT subscription serves the same ids with a 272K
  window. Grok 4.5 and 4.6 have a 500K window and a 200K tier. Subscriptions
  have no per-token price.
- **Listings.** OpenRouter and Vercel AI Gateway list their models at
  `GET /models` without a key. agt keeps the models that reason and call tools,
  apart from OpenRouter's batch variants, with their window, efforts, prices,
  whether they take images, and the input size above which a request is billed
  at long-context rates.
- **Unknown ids** get a 200K window and every effort.
- **Cost** is the provider's `usage.cost` when it reports one, as OpenRouter
  does. Otherwise it is the response's tokens at the model's prices: fresh,
  cached and cache-write input, and output, at the long-context rates for the
  whole request once its input passes the tier.
- **Tokens** are the provider's `usage` for everything it has seen, plus bytes/4
  for items appended since, with each image counted at ⌈w/28⌉ × ⌈h/28⌉ tokens:
  Claude's count, which is more than OpenAI's models use.

## What every provider gets

- **Reasoning goes back only to what produced it.** Reasoning is replayed only
  to the provider, sign-in and model that produced it. The log records which
  provider and model each span came from, so a resumed session keeps its
  reasoning when both are unchanged. Older items lose their `id`, which OpenAI
  would otherwise reject as unpaired, and the compaction item a provider's
  compaction left is replaced by a note that earlier work was compacted and
  where the transcript is. Reasoning items are kept when they carry encrypted
  content or plain reasoning text.
- **A rejection of replayed reasoning is remembered.** What was replayed is
  left out from then on, and reasoning produced later is replayed as usual; the
  history and log are never rewritten for it.
- **The stream's end is authoritative.** The terminal event's `output` is what
  a response produced. `output_item.done` items cover providers that omit it or
  send it empty, and a last event without its blank line still counts. A stream
  that ends without a terminal event is a failure. Function calls without a call
  id, name or string arguments are dropped, since they could never be sent back.
- **Failures are retried until they clearly will not pass.** Before any request
  has succeeded, three retries surface configuration mistakes quickly. After
  that, transient failures (408, 409, 425, 429, 5xx, network errors, stream
  drops, and rate-limit or overload errors inside a stream) are retried
  indefinitely, with exponential backoff capped at 60 s that honors
  `retry-after-ms` and `Retry-After`. A request that produces nothing for ten
  minutes is sent again, and response bodies time out after an hour, so a worker
  left on a dead connection eventually exits.
- **An overflow compacts.** A context overflow error, including a 413 or a
  "request too large" 429, lowers the session's window to 90% of the estimated
  context and compacts. Throttling messages that mention tokens are not mistaken
  for overflows.

## Details worth knowing

**OpenAI** compacts inline: every request carries `context_management` with a
`compact_threshold`, and the provider compacts while it responds. `/compact`
without a focus uses `/responses/compact`. The GPT-5.6 and later models bill a
request with more than 272K input tokens at higher rates for all of its tokens,
so a session's budget is 272K even with the 1.05M window.

**ChatGPT** is the backend Codex uses. It takes no `max_output_tokens`, and
whether it takes `context_management` is not known, so it compacts through
`/responses/compact`. It serves Codex clients a 272K window and counts a request
above that as heavier use. Requests name the account in `chatgpt-account-id`,
read from the access token's claims.

**Grok** documents `/responses/compact` alone. xAI lists models only to
signed-in clients and mixes in image and video models, so agt knows the two a
subscription offers for coding. It bills a request above 200K input tokens at
twice the rates, so a session's budget is 200K of the 500K window.

**OpenRouter** routes its cache by the affinity header alone, so requests carry
no `prompt_cache_key`. Its message for a failure at the model's own provider is
only "Provider returned error", so errors show what that provider said instead.
It returns plain reasoning text for many models, which is kept and replayed.
Gemini's thought signatures verify only on the Google backend that made them,
and a request OpenRouter routes to the other one is rejected like any replayed
reasoning a provider cannot use.

**Vercel AI Gateway** forwards compaction to OpenAI for OpenAI's models only, so
`openai/` models compact through `/responses/compact` and every other model
with summaries.

## Adding a provider

1. Add a variant to `Provider`, list it in `Provider::ALL`, and write its `Spec`
   in `src/provider.rs`: id, name, URL, access, dialect, compaction and catalog.
   Write the dialect as how the provider differs from OpenAI, and state only
   what the provider documents.
2. For models agt should know, add them to `src/models.rs`. For a provider that
   lists its own, write a function that reads an entry of the listing, as
   `models::openrouter` does.
3. Add its id to `tests/provider_requests.rs` and write its golden,
   `requests-<id>.json`, with `AGT_BLESS=1`. Check every field and header in
   it against the provider's documentation.
4. Name it where provider ids are listed: `--provider` in `src/cli/mod.rs`,
   `agt login`'s help in `src/cli/login.rs`, the tests that list the ids, and the
   README.
