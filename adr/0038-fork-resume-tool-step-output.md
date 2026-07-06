# Fork-resume: reconstruct tool-step output

Date: 2026-07-06

Context:

arikusi's review found that fork-resume breaks downstream of a deterministic
tool step. When `zymi resume --from-step X` freezes the steps upstream of `X`,
it reconstructs each frozen step's output from that step's sub-stream via
`extract_step_output` (`resume_pipeline.rs`). That function only understood
`LlmCallCompleted`. A `tool:` step (ADR-0024) never emits an `LlmCallCompleted`
— its sub-stream is `ToolCallRequested` / `ToolCallCompleted` — so freezing a
step downstream of a tool step failed with "missing terminal LlmCallCompleted
event". ADR-0024 claimed fork-resume was "inherited for free"; it was not, and
the only test covered a tool step in the *re-execute* set, never in the *frozen*
set.

Decision:

- `extract_step_output` now handles both step shapes. It tries the agent path
  first — the last `LlmCallCompleted` with `has_tool_calls == false`, its
  content or `content_preview`. Only if there is **no** terminal LLM completion
  (a genuine tool step) does it fall back to the last `ToolCallCompleted`'s
  `result` (untruncated), or `result_preview` when `result` is empty
  (pre-enrichment events).
- Order matters: agent steps also emit `ToolCallCompleted` for their mid-ReAct
  tool calls, so the LLM path must win; the tool fallback fires only in its
  absence.
- Unit test added for the frozen tool-step extraction (full result and
  preview-fallback). ADR-0024 annotated with a correction for its stale event
  names and the false "inherited for free" claim.

Consequences:

- Pros: forking downstream of a tool step now works; the reconstructed output
  is the tool's real result, matching what a live run would have interpolated
  into `${steps.<id>.output}`.
- Cons / follow-up: the added coverage is a unit test on `extract_step_output`.
  A full end-to-end resume with a tool step in the frozen set (not just the
  re-execute set) is still worth adding to the integration suite; deferred, not
  blocking, since the extraction is the piece that was broken.
- Not a schema or config change — purely a reader fix, so it is compatible with
  existing stores.
