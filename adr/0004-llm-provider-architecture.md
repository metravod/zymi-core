# LLM Provider Architecture — Unified Trait, OpenAI-Compatible + Anthropic

Date: 2026-04-04

## Context

Agents need to call LLMs at runtime. The ecosystem has many providers (OpenAI, Anthropic, Ollama, vLLM, Together) with two distinct wire formats: OpenAI chat completions API (widely adopted as de facto standard) and Anthropic Messages API. Need to support all without provider-specific code leaking into the orchestrator.

## Decision

- **`LlmProvider` trait** with a single method: `chat_completion(&self, request: &ChatRequest) -> Result<ChatResponse, LlmError>`. Object-safe, `Send + Sync + Debug`.
- **Two implementations**:
  - `OpenAiProvider` — covers OpenAI, Ollama, vLLM, Together (all OpenAI-compatible, differ only in `base_url` and auth requirements).
  - `AnthropicProvider` — Anthropic Messages API with system prompt extraction and `tool_use`/`tool_result` content blocks.
- **`create_provider(config: &LlmConfig)` factory** routes by `config.provider` string, applies default `base_url` per provider.
- Wire format types are **private** to each provider module. Public contract is only `ChatRequest`/`ChatResponse` using existing `Message`, `ToolCallInfo`, `ToolDefinition`, `TokenUsage` from `types.rs`.
- API key is **optional** for OpenAI-compatible providers (local Ollama/vLLM don't need it), **required** for Anthropic.

## Alternatives Considered

- **Separate provider per service** (OpenAiProvider, OllamaProvider, VllmProvider, TogetherProvider): Massive code duplication — all four use identical wire format, only base_url differs.
- **Single universal provider with format auto-detection**: Too fragile. OpenAI and Anthropic formats are fundamentally different (role-based vs content-block-based).
- **Using an existing crate (async-openai, anthropic-sdk)**: Adds heavy dependencies for a thin HTTP layer. Our wire types are ~100 lines each and give us full control over mapping to internal types.

## Consequences

- Adding a new OpenAI-compatible provider = one line in the factory (new match arm with default base_url).
- Adding a fundamentally different API (e.g., Google Gemini) = new provider module implementing `LlmProvider`.
- Orchestrator is provider-agnostic — receives `Box<dyn LlmProvider>`.
- No streaming yet — initial implementation is request/response only. Streaming can be added as a second trait method later.
