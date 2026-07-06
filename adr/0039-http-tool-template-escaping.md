# HTTP tool templates: context-aware escaping

Date: 2026-07-06

Context:

arikusi's review flagged that declarative `kind: http` tools splice
LLM-controlled arguments into URLs, headers, and JSON body templates via raw
string substitution (`resolve_args`), with no escaping. Declarative HTTP tools
map to `Intention::CallCustomTool`, which the contract engine auto-approves, so
there is no policy gate to catch it either. A model-supplied
`"},"admin":true,"x":"` in an argument rewrites the JSON request body silently
unless the operator remembered `requires_approval: true`.

Shell templates get away with the same `resolve_args` because the policy engine
evaluates the *resolved* command (ADR-0014); HTTP had no equivalent gate.

Decision:

- **Context-aware escaping at the substitution site**, not a blanket approval
  gate (escaping fixes the class of bug without forcing approval friction on
  every HTTP call):
  - **URL** (`resolve_args_url`): each value is percent-encoded
    (`NON_ALPHANUMERIC`), so it cannot add path segments or query params.
  - **JSON body** (`resolve_args_json`): each value is escaped as JSON string
    content (serde escaping, outer quotes stripped) and inserted inside the
    template's existing `"..."`. A break-out payload becomes inert string
    content. Templates are expected to quote placeholders
    (`{"q":"${args.query}"}`), which is already the idiom.
  - **Headers**: left on raw `resolve_args`. `reqwest` rejects CR/LF in header
    values, so header-splitting fails closed at send; a value staying within a
    single header line is the intended use (`Bearer ${args.token}`).
- `resolve_args` itself is unchanged and still used for shell templates, whose
  safety comes from the policy engine seeing the resolved command. Its doc
  comment now warns against using it for HTTP.

Consequences:

- Pros: the reported JSON-body breakout and URL-structure injection are closed
  by construction, for every declarative HTTP tool, with no config change and
  no new approval prompts. Adds one small, already-in-tree dependency
  (`percent-encoding`, under the `runtime` feature).
- Cons / limitations:
  - `resolve_args_json` assumes placeholders sit in JSON *string* position. A
    template that interpolates a bare value (`{"n": ${args.count}}`) would get
    the value quoted-escaped and should instead use a Python tool for a
    structured body. Documented; the string-position idiom covers the declarative
    cases.
  - **Deferred cousin (not in this ADR):** tool denial/error detection in
    `run_pipeline` is string-prefix matching on the result
    (`result.starts_with("[denied…")`), which legitimate tool output can trip
    and malicious output can spoof. The robust fix threads a structured verdict
    instead of re-sniffing a string; it touches the executor return type and is
    tracked as a follow-up, not folded into this security fix.
