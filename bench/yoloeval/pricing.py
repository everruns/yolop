"""Token-based USD cost estimation.

Only a fallback: agents that report real cost (yolop's price-table estimate,
claude-code's ``total_cost_usd``, pi's ``usage.cost.total``) take precedence.
This is for agents that report token usage but no cost (e.g. codex).

Prices are supplied per-config in the matrix (``price:`` block, USD per **1M**
tokens) rather than hardcoded here, so we never ship vendor prices that silently
drift out of date. Keys: ``input``, ``output``, ``cache_read``, ``cache_write``
(the cache keys default to the ``input`` price when omitted).
"""

from __future__ import annotations

from .models import TokenUsage


def cost_from_usage(tokens: TokenUsage, price: dict | None) -> float | None:
    """Estimate USD cost from token counts and a per-1M price block.

    Returns ``None`` when no price is configured, so callers can distinguish
    "unpriced" from "$0".
    """
    if not price:
        return None
    in_p = float(price.get("input", 0.0))
    out_p = float(price.get("output", 0.0))
    cr_p = float(price.get("cache_read", in_p))
    cw_p = float(price.get("cache_write", in_p))
    cost = (
        tokens.input_tokens * in_p
        + tokens.output_tokens * out_p
        + tokens.cache_read_tokens * cr_p
        + tokens.cache_creation_tokens * cw_p
    ) / 1_000_000.0
    return round(cost, 6)
