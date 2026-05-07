"""Sync Python tool — current weather for a city.

This is the simplest possible @tool: a regular Python function whose
arguments and docstring are introspected to build the LLM-facing schema.
Replace the stub body with a real API call (OpenWeatherMap, weatherapi.com,
…) when you want live data.
"""
from zymi import tool


@tool
def get_weather(city: str) -> str:
    """Return the current weather for a city."""
    return f"It's mild and partly cloudy in {city} today (stub — wire a real API)."
