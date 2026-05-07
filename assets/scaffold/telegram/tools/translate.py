"""Async Python tool — `async def` is supported as-is.

The runtime awaits the coroutine on a worker thread, so you can use
`httpx`, `aiohttp`, or any other async I/O library here without blocking
the agent loop. The stub below just sleeps to prove async dispatch works.
"""
import asyncio

from zymi import tool


@tool
async def translate(text: str, target: str = "en") -> str:
    """Translate text into the target language (ISO 639-1 code)."""
    await asyncio.sleep(0)  # placeholder for a real async HTTP call
    return f"[{target}] {text}"
