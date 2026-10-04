# OpenAI-compatible providers can stream (opt-in `stream: true`)

Date: 2026-10-04

## Context

The first real run of the `subbotnik` home pipeline (ADR-0044) failed with
`LLM call failed: HTTP request failed: error sending request for url (…)`.
Reproduced against `api.neuraldeep.ru`: a non-streamed chat completion is cut
by a gateway at **exactly 61s** (`Connection reset by peer`), while the same
request with `"stream": true` runs to completion (106s). A reasoning model
writing a fleet report routinely needs more than 60s, so the provider was
unusable for it. Gateways with 60–100s idle cuts (nginx `proxy_read_timeout`,
Cloudflare, corporate proxies) are common in front of self-hosted and
"OpenAI-compatible" endpoints.

ADR-0004 shipped request/response only ("streaming can be added later"). Two
more defects made the failure opaque:

- `LlmError::Http` printed reqwest's top-level message only; the cause
  (reset, timeout, TLS, DNS) lives in the error's source chain.
- A step returning `Err` (as an LLM failure does) `?`-ed out of the pipeline
  handler: no `WorkflowNodeCompleted`, no `PipelineCompleted`, no WAL
  checkpoint — `zymi runs` showed the run as "running" forever and
  `zymi events` couldn't see its tail.

Rejected alternatives:

- **Stream by default.** Most robust, but `stream_options.include_usage` and
  streamed tool-call deltas are where "OpenAI-compatible" servers diverge
  most; flipping the wire format under every existing project risks
  regressions we can't test. Opt-in now; revisit the default once it has
  mileage.
- **Raise our client timeout.** Ours is already 300s; the cut is upstream.
- **Faster/smaller model as the fix.** `-noreason` finished in 53s on a
  fixture — a margin, not a fix.

## Decision

1. `LlmConfig` gains `stream: bool` (default `false`). For OpenAI-compatible
   providers (`openai`, `ollama`, `vllm`, `together`) it sends
   `stream: true` + `stream_options: {include_usage: true}` and accumulates
   the SSE deltas (content, tool-call fragments by `index`, usage) into the
   same response the non-streamed path parses — the rest of the runtime is
   unchanged. An in-stream `{"error": …}` chunk becomes `LlmError::Api`.
   `anthropic` + `stream: true` is a config error for now.
2. Errors carry their cause: `LlmError::Http` prints the full source chain.
   A non-streamed request that dies after ≥45s without a response (and
   wasn't our own timeout) is reported as `LlmError::Dropped`, naming the
   elapsed time and suggesting `stream: true`.
3. A step that errors is sealed: `WorkflowNodeCompleted{success:false}`,
   the pipeline halts with that error, and the normal `WorkflowCompleted` /
   `PipelineCompleted` / WAL checkpoint path runs.

## Consequences

- Long generations work behind cutting gateways with one line in
  `providers.yml` / `llm:`.
- Failures say what actually happened and what to change.
- **Minus:** the streamed path is a second wire format to keep correct;
  covered by unit tests over captured SSE shapes and one live endpoint
  (vLLM-backed), not by every server out there.
- **Minus:** our 300s client timeout still bounds the *whole* streamed
  response — a generation longer than that is cut by us. Raise it if a
  real case needs it.
- Следствие: making streaming the default, and streaming for Anthropic, are
  open — decide once `stream: true` has real-world mileage.
