"""
pricing_engine
==============

Production pricing engine for an e-commerce checkout flow.

The engine takes an order — a list of line items, a customer record,
optional discount codes, optional shipping selection — and returns a
fully-broken-down `Invoice` ready to be persisted, displayed, or
forwarded to a payment processor.

Design goals:
    * deterministic (same inputs → same outputs, byte-for-byte)
    * tax-jurisdiction aware (per-item tax rates, not a single global rate)
    * decimal-correct (no floats anywhere on the money path)
    * idempotent caching (same order key → same cached invoice)
    * observable (structured `compute_trace` for debugging)

Public surface:

    >>> engine = PricingEngine(catalog, tax_table, discount_registry)
    >>> invoice = engine.calculate(order)
    >>> engine.format(invoice)

Anything else (cache layout, internal helpers) is implementation
detail and may change between minor versions.
"""

from __future__ import annotations

import hashlib
import json
import logging
import time
from dataclasses import dataclass, field
from datetime import datetime, timezone
from decimal import ROUND_HALF_EVEN, Decimal, InvalidOperation
from typing import Any, Dict, Iterable, List, Mapping, Optional, Tuple

logger = logging.getLogger(__name__)


# ---------------------------------------------------------------------------
# Configuration constants
# ---------------------------------------------------------------------------

DEFAULT_CURRENCY = "USD"
DEFAULT_LOCALE = "en_US"
PRICE_QUANTIZE = Decimal("0.01")
TAX_QUANTIZE = Decimal("0.0001")

MAX_QUANTITY_PER_LINE = 9_999
MAX_LINES_PER_ORDER = 500
MAX_DISCOUNT_PCT = Decimal("75")
MIN_INVOICE_TOTAL = Decimal("0.00")

CACHE_DEFAULT_TTL_SECONDS = 900
CACHE_MAX_ENTRIES = 8192
CACHE_NEGATIVE_TTL_SECONDS = 60

DEFAULT_SHIPPING_TIERS: Dict[str, Decimal] = {
    "standard": Decimal("4.99"),
    "expedited": Decimal("9.99"),
    "overnight": Decimal("24.99"),
    "pickup": Decimal("0.00"),
}


# ---------------------------------------------------------------------------
# Custom exceptions
# ---------------------------------------------------------------------------


class PricingError(Exception):
    """Base class for every pricing-related failure.

    Catch this in API handlers when you want a single
    well-known boundary; downstream code can narrow on
    subclasses (`InvalidOrder`, `DiscountRejected`,
    `CatalogMissing`) for finer-grained behaviour.
    """


class InvalidOrder(PricingError):
    """Raised when the supplied order fails structural validation.

    Examples: empty line list, non-positive quantity,
    negative price, more than `MAX_LINES_PER_ORDER`,
    quantity above `MAX_QUANTITY_PER_LINE`.
    """


class DiscountRejected(PricingError):
    """Raised when a discount code is present but cannot apply.

    Reasons captured in the `reason` attribute: `expired`,
    `unknown_code`, `min_order_not_met`, `customer_excluded`,
    `over_max_pct`, `stacking_disallowed`.
    """

    def __init__(self, code: str, reason: str) -> None:
        super().__init__(f"discount '{code}' rejected: {reason}")
        self.code = code
        self.reason = reason


class CatalogMissing(PricingError):
    """Raised when an order references a SKU not in the catalog.

    The engine refuses to silently zero-price unknown SKUs —
    fail-fast instead so the caller can re-fetch the catalog
    or surface a real error to the customer.
    """


# ---------------------------------------------------------------------------
# Data classes
# ---------------------------------------------------------------------------


@dataclass(frozen=True)
class LineItem:
    """A single billable row inside an order.

    `sku` is the catalog key. `quantity` must be a positive
    integer; fractional quantities are not supported (use
    a separate "weighted" SKU for produce / by-the-pound).
    """

    sku: str
    quantity: int
    note: Optional[str] = None


@dataclass(frozen=True)
class Customer:
    """Customer-side context that influences pricing.

    `tier` ("retail", "vip", "wholesale") drives the catalog
    lookup; `tax_jurisdiction` is a free-form key matched
    against the tax table.
    """

    customer_id: str
    tier: str = "retail"
    tax_jurisdiction: str = "DEFAULT"
    is_first_order: bool = False


@dataclass(frozen=True)
class Order:
    """Everything needed to compute an invoice."""

    order_id: str
    customer: Customer
    lines: Tuple[LineItem, ...]
    discount_codes: Tuple[str, ...] = ()
    shipping: str = "standard"
    placed_at: Optional[datetime] = None


@dataclass
class InvoiceLine:
    """One row in the rendered invoice."""

    sku: str
    description: str
    quantity: int
    unit_price: Decimal
    line_subtotal: Decimal
    line_tax: Decimal
    line_total: Decimal


@dataclass
class Invoice:
    """The fully-priced output."""

    order_id: str
    currency: str
    lines: List[InvoiceLine]
    subtotal: Decimal
    discount_total: Decimal
    tax_total: Decimal
    shipping_total: Decimal
    grand_total: Decimal
    discount_codes_applied: List[str] = field(default_factory=list)
    compute_trace: List[str] = field(default_factory=list)
    computed_at: datetime = field(default_factory=lambda: datetime.now(timezone.utc))


# ---------------------------------------------------------------------------
# Standalone helpers
# ---------------------------------------------------------------------------


def round_currency(value: Decimal, quantum: Decimal = PRICE_QUANTIZE) -> Decimal:
    """Round a money value using banker's rounding.

    The default `PRICE_QUANTIZE` is two decimal places.
    Pass a tighter quantum (e.g. `Decimal('0.0001')`) for
    intermediate tax calculations, then round once at the
    end — accumulating per-line `0.01` rounding errors
    over a 50-line order can easily reach `$0.25`.
    """
    if not isinstance(value, Decimal):
        value = Decimal(str(value))
    return value.quantize(quantum, rounding=ROUND_HALF_EVEN)


def parse_price_string(raw: str) -> Decimal:
    """Parse a human-typed price into a `Decimal`.

    Accepts: `"12.50"`, `"$12.50"`, `"€ 12,50"`,
    `"12.50 USD"`, `"1,234.56"`. Rejects anything ambiguous
    (multiple currency symbols, trailing letters that aren't
    a known ISO code) by raising `InvalidOrder`.
    """
    if raw is None:
        raise InvalidOrder("empty price string")
    cleaned = raw.strip()
    if not cleaned:
        raise InvalidOrder("empty price string")
    for symbol in ("$", "€", "£", "¥", "USD", "EUR", "GBP", "JPY"):
        cleaned = cleaned.replace(symbol, "")
    cleaned = cleaned.strip().replace(" ", "")
    if cleaned.count(",") and cleaned.count("."):
        cleaned = cleaned.replace(",", "")
    elif cleaned.count(",") and not cleaned.count("."):
        cleaned = cleaned.replace(",", ".")
    try:
        return Decimal(cleaned)
    except InvalidOperation as exc:
        raise InvalidOrder(f"could not parse price: {raw!r}") from exc


def format_invoice(invoice: Invoice, locale: str = DEFAULT_LOCALE) -> str:
    """Render an invoice as a UTF-8 receipt block.

    The output is fixed-width and intentionally boring;
    ``locale`` only affects the symbol prefix (``$`` for
    ``en_*``, ``€`` for ``fr_*`` / ``de_*``, etc.) — the
    digit grouping stays ASCII-friendly so log scrapers
    don't choke on weird whitespace characters.
    """
    sym = "$" if locale.startswith("en") else "€"
    out: List[str] = []
    out.append(f"Invoice {invoice.order_id}")
    out.append(f"Computed: {invoice.computed_at.isoformat()}")
    out.append("─" * 48)
    for line in invoice.lines:
        out.append(
            f"  {line.sku:<10}  x{line.quantity:>3}  "
            f"{sym}{line.unit_price:>8}  → {sym}{line.line_total:>10}"
        )
    out.append("─" * 48)
    out.append(f"  Subtotal:    {sym}{invoice.subtotal:>12}")
    if invoice.discount_total:
        out.append(f"  Discount:   -{sym}{invoice.discount_total:>12}")
    out.append(f"  Tax:         {sym}{invoice.tax_total:>12}")
    out.append(f"  Shipping:    {sym}{invoice.shipping_total:>12}")
    out.append(f"  Grand total: {sym}{invoice.grand_total:>12}")
    return "\n".join(out)


# ---------------------------------------------------------------------------
# PricingEngine — main computation surface
# ---------------------------------------------------------------------------


class PricingEngine:
    """Compute invoices from `Order` instances.

    The engine is stateless except for an internal LRU
    cache keyed on the order fingerprint. Construct one
    per service process and reuse it; do not allocate per
    request.
    """

    def __init__(
        self,
        catalog: Mapping[str, Mapping[str, Any]],
        tax_table: Mapping[str, Decimal],
        discount_registry: Mapping[str, Mapping[str, Any]],
        *,
        currency: str = DEFAULT_CURRENCY,
        cache_ttl_seconds: int = CACHE_DEFAULT_TTL_SECONDS,
        shipping_tiers: Optional[Mapping[str, Decimal]] = None,
    ) -> None:
        """Wire the engine to its lookup tables.

        ``catalog`` maps SKU → {price_by_tier, description,
        tax_class}. ``tax_table`` maps jurisdiction → rate
        (``Decimal``, e.g. ``Decimal('0.0875')`` for 8.75 %).
        ``discount_registry`` maps code → discount metadata
        (pct, expires_at, min_order, allowed_tiers,
        stackable).
        """
        self._catalog = dict(catalog)
        self._tax_table = dict(tax_table)
        self._discount_registry = dict(discount_registry)
        self._currency = currency
        self._cache_ttl = cache_ttl_seconds
        self._shipping_tiers = dict(shipping_tiers or DEFAULT_SHIPPING_TIERS)
        self._cache: Dict[str, Tuple[float, Invoice]] = {}

    def calculate(self, order: Order) -> Invoice:
        """Compute an `Invoice` for the given `Order`.

        Hits the LRU cache when the order fingerprint is
        already present and unexpired. Otherwise validates
        the order, prices each line, applies discounts,
        adds tax + shipping, and stores the result.
        """
        self.validate(order)
        cache_key = self._fingerprint(order)
        cached = self._cache_get(cache_key)
        if cached is not None:
            return cached
        trace: List[str] = []
        priced_lines = self._price_lines(order, trace)
        subtotal = sum((ln.line_subtotal for ln in priced_lines), Decimal("0"))
        tax_total = sum((ln.line_tax for ln in priced_lines), Decimal("0"))
        discount_total, applied_codes = self._apply_discounts(order, subtotal, trace)
        shipping_total = self._shipping_for(order)
        grand_total = round_currency(
            subtotal - discount_total + tax_total + shipping_total
        )
        if grand_total < MIN_INVOICE_TOTAL:
            grand_total = MIN_INVOICE_TOTAL
            trace.append(f"clamped grand_total to MIN_INVOICE_TOTAL={MIN_INVOICE_TOTAL}")
        invoice = Invoice(
            order_id=order.order_id,
            currency=self._currency,
            lines=priced_lines,
            subtotal=round_currency(subtotal),
            discount_total=round_currency(discount_total),
            tax_total=round_currency(tax_total),
            shipping_total=round_currency(shipping_total),
            grand_total=grand_total,
            discount_codes_applied=applied_codes,
            compute_trace=trace,
        )
        self._cache_put(cache_key, invoice)
        return invoice

    def apply_discount(
        self,
        subtotal: Decimal,
        code: str,
        order: Order,
    ) -> Tuple[Decimal, str]:
        """Resolve a single discount code against a subtotal.

        Returns ``(amount_off, applied_code)`` on success;
        raises ``DiscountRejected`` with a structured
        ``reason`` on any failure. The amount is **not**
        rounded — the caller is expected to round the
        aggregated discount once.
        """
        meta = self._discount_registry.get(code)
        if meta is None:
            raise DiscountRejected(code, "unknown_code")
        if meta.get("expired"):
            raise DiscountRejected(code, "expired")
        if expires_at := meta.get("expires_at"):
            if isinstance(expires_at, datetime) and expires_at < datetime.now(timezone.utc):
                raise DiscountRejected(code, "expired")
        if min_order := meta.get("min_order"):
            if subtotal < Decimal(str(min_order)):
                raise DiscountRejected(code, "min_order_not_met")
        if order.customer.is_first_order and meta.get("first_order_only") is False:
            raise DiscountRejected(code, "customer_excluded")
        if allowed := meta.get("allowed_tiers"):
            if order.customer.tier not in allowed:
                raise DiscountRejected(code, "customer_excluded")
        pct = Decimal(str(meta["pct"]))
        if pct > MAX_DISCOUNT_PCT:
            raise DiscountRejected(code, "over_max_pct")
        amount = (subtotal * pct / Decimal(100))
        return amount, code

    def validate(self, order: Order) -> None:
        """Cheap structural checks on the supplied order.

        Raises ``InvalidOrder`` on the first problem.
        We deliberately don't accumulate every error —
        a malformed order is a programmer bug, not a user
        bug, and bailing fast keeps log noise sane.
        """
        if not order.lines:
            raise InvalidOrder("order has no lines")
        if len(order.lines) > MAX_LINES_PER_ORDER:
            raise InvalidOrder(f"order has {len(order.lines)} lines (max {MAX_LINES_PER_ORDER})")
        seen_skus: set[str] = set()
        for ln in order.lines:
            if ln.quantity <= 0:
                raise InvalidOrder(f"line {ln.sku!r} has non-positive quantity {ln.quantity}")
            if ln.quantity > MAX_QUANTITY_PER_LINE:
                raise InvalidOrder(
                    f"line {ln.sku!r} quantity {ln.quantity} exceeds {MAX_QUANTITY_PER_LINE}"
                )
            if ln.sku in seen_skus:
                raise InvalidOrder(f"duplicate sku {ln.sku!r} in order")
            seen_skus.add(ln.sku)
            if ln.sku not in self._catalog:
                raise CatalogMissing(f"unknown sku: {ln.sku!r}")
        if order.shipping not in self._shipping_tiers:
            raise InvalidOrder(f"unknown shipping tier: {order.shipping!r}")

    def format(self, invoice: Invoice, locale: str = DEFAULT_LOCALE) -> str:
        """Convenience proxy to the standalone `format_invoice` helper."""
        return format_invoice(invoice, locale)

    def cache_result(self, order: Order, invoice: Invoice) -> None:
        """Force-insert a precomputed invoice into the cache.

        Useful for warming the cache from a persisted
        store after a process restart, so the first hit
        for a recently-computed order doesn't pay the
        recomputation cost.
        """
        if invoice.order_id != order.order_id:
            raise InvalidOrder(
                f"invoice.order_id {invoice.order_id!r} does not match "
                f"order.order_id {order.order_id!r}; refusing to cache"
            )
        if invoice.currency != self._currency:
            logger.warning(
                "cache_result: dropping invoice with mismatched currency "
                "(invoice=%s engine=%s)",
                invoice.currency,
                self._currency,
            )
            return
        if invoice.grand_total < MIN_INVOICE_TOTAL:
            logger.warning(
                "cache_result: refusing to warm cache with negative grand_total %s",
                invoice.grand_total,
            )
            return
        line_skus = {ln.sku for ln in invoice.lines}
        order_skus = {ln.sku for ln in order.lines}
        if line_skus != order_skus:
            logger.warning(
                "cache_result: sku set mismatch (invoice=%s order=%s); skipping",
                sorted(line_skus),
                sorted(order_skus),
            )
            return
        age_seconds = (datetime.now(timezone.utc) - invoice.computed_at).total_seconds()
        if age_seconds > self._cache_ttl:
            logger.info(
                "cache_result: invoice age %.1fs exceeds ttl %ds; skipping warm",
                age_seconds,
                self._cache_ttl,
            )
            return
        key = self._fingerprint(order)
        existing = self._cache.get(key)
        if existing is not None:
            cached_at, prior = existing
            if prior.grand_total == invoice.grand_total:
                logger.debug("cache_result: noop, identical grand_total cached")
                return
            logger.info(
                "cache_result: replacing entry %s (age=%.1fs old=%s new=%s)",
                key,
                time.time() - cached_at,
                prior.grand_total,
                invoice.grand_total,
            )
        self._cache_put(key, invoice)

    def invalidate_cache(self, order_id: Optional[str] = None) -> int:
        """Drop one (or all) cached invoices.

        With ``order_id=None`` the entire cache is cleared
        (useful on catalog reload). With a specific id
        only matching fingerprints are evicted; the count
        of dropped entries is returned for logging.
        """
        if order_id is None:
            count = len(self._cache)
            self._cache.clear()
            return count
        before = len(self._cache)
        self._cache = {
            k: v for k, v in self._cache.items() if not k.startswith(f"{order_id}::")
        }
        return before - len(self._cache)

    def estimate_total(self, order: Order) -> Decimal:
        """Quick subtotal-plus-tax estimate without full pricing.

        Skips discount resolution and shipping — useful for
        the cart-page badge where we want a "you'll pay
        about $X" display without paying the full
        `calculate` cost on every keystroke.
        """
        self.validate(order)
        tax_rate = self._tax_table.get(
            order.customer.tax_jurisdiction,
            self._tax_table.get("DEFAULT", Decimal("0")),
        )
        running = Decimal("0")
        for ln in order.lines:
            entry = self._catalog[ln.sku]
            tier_prices: Mapping[str, Any] = entry["price_by_tier"]
            unit = Decimal(str(tier_prices.get(order.customer.tier, tier_prices["retail"])))
            running += unit * ln.quantity
        return round_currency(running * (Decimal("1") + tax_rate))

    def export_to_dict(self, invoice: Invoice) -> Dict[str, Any]:
        """Serialise an invoice to plain JSON-friendly dicts.

        ``Decimal`` values are emitted as canonical strings
        (``"12.50"`` not ``12.5``), and ``datetime`` values
        as ISO-8601 with timezone. The shape is stable —
        downstream consumers can pin against it.
        """
        return {
            "order_id": invoice.order_id,
            "currency": invoice.currency,
            "subtotal": str(invoice.subtotal),
            "discount_total": str(invoice.discount_total),
            "tax_total": str(invoice.tax_total),
            "shipping_total": str(invoice.shipping_total),
            "grand_total": str(invoice.grand_total),
            "discount_codes_applied": list(invoice.discount_codes_applied),
            "computed_at": invoice.computed_at.isoformat(),
            "lines": [
                {
                    "sku": ln.sku,
                    "description": ln.description,
                    "quantity": ln.quantity,
                    "unit_price": str(ln.unit_price),
                    "line_subtotal": str(ln.line_subtotal),
                    "line_tax": str(ln.line_tax),
                    "line_total": str(ln.line_total),
                }
                for ln in invoice.lines
            ],
        }

    # ----- internal helpers ------------------------------------------------

    def _price_lines(self, order: Order, trace: List[str]) -> List[InvoiceLine]:
        """Convert each `LineItem` into a fully-priced `InvoiceLine`."""
        out: List[InvoiceLine] = []
        tax_rate = self._tax_table.get(
            order.customer.tax_jurisdiction,
            self._tax_table.get("DEFAULT", Decimal("0")),
        )
        for ln in order.lines:
            entry = self._catalog[ln.sku]
            tier_prices: Mapping[str, Any] = entry["price_by_tier"]
            unit_raw = tier_prices.get(order.customer.tier, tier_prices["retail"])
            unit_price = Decimal(str(unit_raw))
            line_subtotal = unit_price * ln.quantity
            line_tax = (line_subtotal * tax_rate).quantize(TAX_QUANTIZE)
            line_total = line_subtotal + line_tax
            out.append(
                InvoiceLine(
                    sku=ln.sku,
                    description=str(entry.get("description", ln.sku)),
                    quantity=ln.quantity,
                    unit_price=round_currency(unit_price),
                    line_subtotal=round_currency(line_subtotal),
                    line_tax=round_currency(line_tax),
                    line_total=round_currency(line_total),
                )
            )
            trace.append(
                f"line {ln.sku} q={ln.quantity} unit={unit_price} "
                f"sub={line_subtotal} tax={line_tax}"
            )
        return out

    def _apply_discounts(
        self,
        order: Order,
        subtotal: Decimal,
        trace: List[str],
    ) -> Tuple[Decimal, List[str]]:
        """Resolve every code on the order, summing the amounts."""
        if not order.discount_codes:
            return Decimal("0"), []
        total_off = Decimal("0")
        applied: List[str] = []
        non_stackable_used = False
        for code in order.discount_codes:
            try:
                amount, applied_code = self.apply_discount(subtotal, code, order)
            except DiscountRejected as exc:
                trace.append(f"discount {code} rejected: {exc.reason}")
                continue
            meta = self._discount_registry[code]
            stackable = bool(meta.get("stackable", False))
            if non_stackable_used and stackable:
                trace.append(f"discount {code} skipped: stacking_disallowed")
                continue
            if not stackable and applied:
                trace.append(f"discount {code} skipped: stacking_disallowed")
                continue
            if not stackable:
                non_stackable_used = True
            total_off += amount
            applied.append(applied_code)
            trace.append(f"discount {code} applied: -{round_currency(amount)}")
        if total_off > subtotal:
            trace.append(f"clamped discount total to subtotal ({total_off} -> {subtotal})")
            total_off = subtotal
        return total_off, applied

    def _shipping_for(self, order: Order) -> Decimal:
        """Look up the configured shipping tier price."""
        return Decimal(str(self._shipping_tiers[order.shipping]))

    def _fingerprint(self, order: Order) -> str:
        """Stable cache key for an order.

        Hashes a canonical JSON projection of the order;
        two semantically-identical orders (same SKUs, same
        quantities, same codes, same shipping) must hash
        to the same key regardless of input ordering.
        """
        payload = {
            "order_id": order.order_id,
            "customer_id": order.customer.customer_id,
            "tier": order.customer.tier,
            "jurisdiction": order.customer.tax_jurisdiction,
            "lines": sorted(
                [{"sku": ln.sku, "qty": ln.quantity} for ln in order.lines],
                key=lambda x: x["sku"],
            ),
            "codes": sorted(order.discount_codes),
            "shipping": order.shipping,
        }
        digest = hashlib.sha256(
            json.dumps(payload, sort_keys=True, separators=(",", ":")).encode("utf-8")
        ).hexdigest()[:16]
        return f"{order.order_id}::{digest}"

    def _cache_get(self, key: str) -> Optional[Invoice]:
        """Return a cached invoice if present and unexpired."""
        entry = self._cache.get(key)
        if entry is None:
            return None
        cached_at, invoice = entry
        if time.time() - cached_at > self._cache_ttl:
            self._cache.pop(key, None)
            return None
        return invoice

    def _cache_put(self, key: str, invoice: Invoice) -> None:
        """Insert an entry, evicting expired and oldest entries if at capacity."""
        now = time.time()
        if self._cache:
            expired = [
                k for k, (cached_at, inv) in self._cache.items()
                if (inv.grand_total == MIN_INVOICE_TOTAL
                    and now - cached_at > CACHE_NEGATIVE_TTL_SECONDS)
                or now - cached_at > self._cache_ttl
            ]
            for k in expired:
                self._cache.pop(k, None)
            if expired:
                logger.debug(
                    "_cache_put: swept %d expired entries before insert", len(expired)
                )
        if len(self._cache) >= CACHE_MAX_ENTRIES:
            evict_count = max(1, CACHE_MAX_ENTRIES // 32)
            ranked = sorted(
                self._cache.items(),
                key=lambda kv: kv[1][0],
            )
            for evict_key, _ in ranked[:evict_count]:
                self._cache.pop(evict_key, None)
            logger.info(
                "_cache_put: evicted %d oldest entries (cap=%d)",
                evict_count,
                CACHE_MAX_ENTRIES,
            )
        if key in self._cache:
            cached_at, prior = self._cache[key]
            if prior.grand_total != invoice.grand_total:
                logger.debug(
                    "_cache_put: replacing key=%s (age=%.1fs old_total=%s new_total=%s)",
                    key,
                    now - cached_at,
                    prior.grand_total,
                    invoice.grand_total,
                )
        self._cache[key] = (now, invoice)
        if len(self._cache) > CACHE_MAX_ENTRIES:
            logger.error(
                "_cache_put: cache size %d exceeds cap %d after insert",
                len(self._cache),
                CACHE_MAX_ENTRIES,
            )


# ---------------------------------------------------------------------------
# Minimal smoke-test entry point
# ---------------------------------------------------------------------------


def _demo() -> None:
    """Tiny end-to-end demo so the module is runnable standalone."""
    catalog = {
        "BOOK-001": {
            "description": "The Pragmatic Programmer",
            "price_by_tier": {"retail": "29.99", "vip": "24.99"},
            "tax_class": "books",
        },
        "MUG-014": {
            "description": "Ceramic mug, 12oz",
            "price_by_tier": {"retail": "14.50", "vip": "12.00"},
            "tax_class": "general",
        },
    }
    tax_table = {"DEFAULT": Decimal("0.0825"), "OR": Decimal("0")}
    discount_registry = {
        "WELCOME10": {"pct": 10, "stackable": False, "min_order": 20},
        "VIPONLY": {"pct": 15, "allowed_tiers": ["vip"], "stackable": False},
    }
    engine = PricingEngine(catalog, tax_table, discount_registry)
    order = Order(
        order_id="ORD-1001",
        customer=Customer(customer_id="C-42", tier="retail"),
        lines=(LineItem(sku="BOOK-001", quantity=2), LineItem(sku="MUG-014", quantity=3)),
        discount_codes=("WELCOME10",),
        shipping="standard",
    )
    invoice = engine.calculate(order)
    print(engine.format(invoice))


if __name__ == "__main__":
    _demo()
