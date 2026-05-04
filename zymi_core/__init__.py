"""zymi-core — Event-sourced agent engine (Python bindings).

Public API re-exported from the native ``_zymi_core`` extension module,
plus the ``@tool`` decorator used for auto-discovery of Python tools
under ``tools/*.py`` (ADR-0014 slice 3).
"""

from zymi_core._zymi_core import (
    Event,
    EventBus,
    EventStore,
    RunPipelineResult,
    Runtime,
    StepResult,
    Subscription,
    ToolRegistry,
)


def tool(
    func=None,
    *,
    name=None,
    description=None,
    intention=None,
    requires_approval=None,
):
    """Mark a function as a zymi tool for auto-discovery.

    Drop a file in ``<project>/tools/<anything>.py`` containing one or
    more ``@tool``-decorated functions and zymi will pick them up at
    runtime startup, register them in the tool catalog, and make them
    available to your agents — no YAML stub required.

    Usage::

        from zymi import tool

        @tool
        def get_weather(city: str) -> str:
            \"\"\"Look up the current weather for a city.\"\"\"
            return f"sunny in {city}"

        @tool(intention="web_search", requires_approval=True)
        async def deep_search(query: str, limit: int = 10) -> str:
            \"\"\"Search the web with deep retrieval.\"\"\"
            ...

    The decorator is a marker only — it sets ``_zymi_tool`` and a
    handful of optional ``_zymi_tool_*`` attributes on the function and
    returns the function unchanged. The Rust loader walks the imported
    module, picks up every callable whose ``_zymi_tool`` is truthy, and
    introspects its signature + docstring for the LLM-facing schema.

    Args:
        name: Override the registered tool name. Defaults to ``func.__name__``.
        description: Override the description. Defaults to the first line
            of the function's docstring.
        intention: Optional ESAA intention tag. Falls back to
            ``CallCustomTool`` when unset.
        requires_approval: Force human-approval gating regardless of the
            project's defaults. Useful for shell-equivalent power tools.
    """

    def _wrap(f):
        f._zymi_tool = True
        if name is not None:
            f._zymi_tool_name = name
        if description is not None:
            f._zymi_tool_description = description
        if intention is not None:
            f._zymi_tool_intention = intention
        if requires_approval is not None:
            f._zymi_tool_requires_approval = bool(requires_approval)
        return f

    if func is None:
        return _wrap
    return _wrap(func)


__all__ = [
    "Event",
    "EventBus",
    "EventStore",
    "RunPipelineResult",
    "Runtime",
    "StepResult",
    "Subscription",
    "ToolRegistry",
    "tool",
]
