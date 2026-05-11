using System;
using System.Collections.Concurrent;
using System.Collections.Generic;
using System.Linq;
using System.Threading;
using System.Threading.Tasks;
using Microsoft.EntityFrameworkCore;
using Microsoft.Extensions.Logging;

namespace ECommerce.Orders.Services;

/// <summary>
/// Status lifecycle for an <see cref="Order"/>. Transitions are validated
/// in <see cref="OrderService.ValidateAsync"/> against the matrix defined
/// in the domain model.
/// </summary>
public enum OrderStatus
{
    Pending = 0,
    Paid = 1,
    Shipped = 2,
    Delivered = 3,
    Cancelled = 4,
    Refunded = 5,
}

/// <summary>
/// Aggregate root representing a customer order. The aggregate owns its
/// <see cref="OrderItem"/> collection and is the only entry point for
/// mutating order state.
/// </summary>
public class Order
{
    /// <summary>Unique identifier (v7 UUID generated at insertion time).</summary>
    public required Guid Id { get; init; }

    /// <summary>Customer that placed the order. Indexed in the database.</summary>
    public required Guid CustomerId { get; init; }

    /// <summary>Current status. Mutated only via <see cref="OrderService"/>.</summary>
    public OrderStatus Status { get; set; } = OrderStatus.Pending;

    /// <summary>UTC timestamp of the original placement.</summary>
    public required DateTimeOffset CreatedAt { get; init; }

    /// <summary>UTC timestamp of the last mutation. Updated by interceptor.</summary>
    public DateTimeOffset UpdatedAt { get; set; }

    /// <summary>UTC timestamp of when the order shipped; null until carrier handoff.</summary>
    public DateTimeOffset? ShippedAt { get; set; }

    /// <summary>Three-letter ISO 4217 currency code.</summary>
    public required string Currency { get; init; }

    /// <summary>Sum of line totals plus tax minus discount. Computed, persisted.</summary>
    public decimal TotalAmount { get; set; }

    /// <summary>Optional cancellation reason; populated when transitioning to Cancelled.</summary>
    public string? CancellationReason { get; set; }

    /// <summary>Owned collection of line items. Loaded eagerly with the order.</summary>
    public List<OrderItem> Items { get; init; } = new();

    /// <summary>Concurrency token populated by EF Core (xmin / rowversion).</summary>
    public uint Xmin { get; set; }
}

/// <summary>
/// Single line item within an <see cref="Order"/>. Owned entity in EF Core
/// terms — has no identity outside its parent order.
/// </summary>
public sealed class OrderItem
{
    /// <summary>Surrogate identifier for the line.</summary>
    public required Guid Id { get; init; }

    /// <summary>SKU of the catalog product. Snapshotted at order time.</summary>
    public required string Sku { get; init; }

    /// <summary>Quantity ordered. Must be strictly positive.</summary>
    public required int Quantity { get; init; }

    /// <summary>Unit price in the order currency at the time of capture.</summary>
    public required decimal UnitPrice { get; init; }

    /// <summary>Line discount applied (absolute amount, not percentage).</summary>
    public decimal Discount { get; init; }
}

/// <summary>
/// Filter object for <see cref="IOrderService.ListAsync"/>. All fields are
/// optional and combine with AND semantics.
/// </summary>
public record OrderFilter(
    Guid? CustomerId = null,
    OrderStatus? Status = null,
    DateTimeOffset? From = null,
    DateTimeOffset? To = null,
    decimal? MinTotal = null,
    decimal? MaxTotal = null,
    int Skip = 0,
    int Take = 50);

/// <summary>
/// Request payload for creating a new order. Validated structurally by
/// model binding and semantically by <see cref="OrderService.ValidateAsync"/>.
/// </summary>
public record CreateOrderRequest(
    Guid CustomerId,
    string Currency,
    IReadOnlyList<CreateOrderLine> Items);

/// <summary>Single line in a <see cref="CreateOrderRequest"/>.</summary>
public record CreateOrderLine(string Sku, int Quantity, decimal UnitPrice, decimal Discount = 0m);

/// <summary>
/// Patch-style request for updating order metadata. Only non-null fields
/// are applied; status transitions are checked separately.
/// </summary>
public record UpdateOrderRequest(
    OrderStatus? Status = null,
    string? CancellationReason = null,
    IReadOnlyList<CreateOrderLine>? ReplaceItems = null);

/// <summary>
/// Aggregated statistics returned by <see cref="IOrderService.GetStatsAsync"/>.
/// Used by the merchant dashboard.
/// </summary>
public record OrderStats(
    int TotalOrders,
    decimal GrossRevenue,
    decimal AverageOrderValue,
    IReadOnlyDictionary<OrderStatus, int> CountsByStatus);

/// <summary>
/// Thrown when an order cannot be located by id. Maps to HTTP 404 in the
/// API layer via the global exception filter.
/// </summary>
public sealed class OrderNotFoundException : Exception
{
    /// <summary>The id that was looked up.</summary>
    public Guid OrderId { get; }

    /// <summary>Constructs the exception with a contextual message.</summary>
    /// <param name="orderId">The id that could not be resolved.</param>
    public OrderNotFoundException(Guid orderId)
        : base($"Order with id '{orderId}' was not found.")
    {
        OrderId = orderId;
    }
}

/// <summary>
/// Thrown when a status transition is rejected by the domain rules.
/// Maps to HTTP 409 Conflict.
/// </summary>
public sealed class InvalidOrderTransitionException : Exception
{
    /// <summary>Current status of the aggregate.</summary>
    public OrderStatus From { get; }

    /// <summary>Requested target status.</summary>
    public OrderStatus To { get; }

    /// <summary>Constructs the exception with both endpoints of the rejected transition.</summary>
    /// <param name="from">The status the order is currently in.</param>
    /// <param name="to">The status the caller attempted to transition to.</param>
    public InvalidOrderTransitionException(OrderStatus from, OrderStatus to)
        : base($"Cannot transition order from {from} to {to}.")
    {
        From = from;
        To = to;
    }
}

/// <summary>
/// Abstraction over the application event bus. Implementations include
/// the in-memory test double and the RabbitMQ-backed adapter.
/// </summary>
public interface IEventBus
{
    /// <summary>Publishes a domain event to all subscribed handlers.</summary>
    /// <typeparam name="T">Event payload type.</typeparam>
    /// <param name="event">The event instance.</param>
    /// <param name="ct">Cancellation token propagated from the request scope.</param>
    Task PublishAsync<T>(T @event, CancellationToken ct) where T : class;
}

/// <summary>
/// EF Core context exposing the <see cref="Order"/> aggregate.
/// </summary>
public class OrderDbContext : DbContext
{
    /// <summary>Standard ctor for DI registration.</summary>
    /// <param name="options">EF Core options injected by the host.</param>
    public OrderDbContext(DbContextOptions<OrderDbContext> options) : base(options) { }

    /// <summary>Top-level orders set. Items are owned and loaded with .Include().</summary>
    public DbSet<Order> Orders => Set<Order>();

    /// <summary>Outbox table for the transactional outbox pattern (v2).</summary>
    public DbSet<OutboxMessage> Outbox => Set<OutboxMessage>();
}

/// <summary>
/// Persisted record of a domain event awaiting publication. Written in the
/// same transaction as the aggregate change; drained by a background relay.
/// </summary>
public sealed class OutboxMessage
{
    /// <summary>Surrogate primary key.</summary>
    public required Guid Id { get; init; }

    /// <summary>Fully qualified type name of the payload for deserialization.</summary>
    public required string Type { get; init; }

    /// <summary>JSON-serialized payload.</summary>
    public required string Payload { get; init; }

    /// <summary>UTC timestamp of enqueue.</summary>
    public required DateTimeOffset OccurredAt { get; init; }

    /// <summary>Null until the relay successfully publishes the message.</summary>
    public DateTimeOffset? DispatchedAt { get; set; }
}

/// <summary>
/// Application-level service contract for the orders bounded context.
/// Consumed by the API layer, by the GraphQL gateway, and by the
/// integration tests.
/// </summary>
public interface IOrderService
{
    /// <summary>Loads a single order by primary key, including its line items.</summary>
    /// <param name="id">Primary key of the order.</param>
    /// <param name="ct">Cancellation token.</param>
    /// <returns>The order, or <c>null</c> if not found.</returns>
    Task<Order?> GetByIdAsync(Guid id, CancellationToken ct);

    /// <summary>Lists orders matching the supplied filter.</summary>
    /// <param name="filter">Combinable filter criteria; pagination via Skip/Take.</param>
    /// <param name="ct">Cancellation token.</param>
    /// <returns>Materialized read-only list of orders.</returns>
    Task<IReadOnlyList<Order>> ListAsync(OrderFilter filter, CancellationToken ct);

    /// <summary>Creates a new order in <see cref="OrderStatus.Pending"/> state.</summary>
    /// <param name="request">Validated create payload.</param>
    /// <param name="ct">Cancellation token.</param>
    /// <returns>The persisted order with generated id and totals.</returns>
    /// <exception cref="ArgumentException">When the request fails domain validation.</exception>
    Task<Order> CreateAsync(CreateOrderRequest request, CancellationToken ct);

    /// <summary>Applies a partial update to the order.</summary>
    /// <param name="id">Order id.</param>
    /// <param name="request">Patch document; null fields are ignored.</param>
    /// <param name="ct">Cancellation token.</param>
    /// <returns>The mutated order.</returns>
    /// <exception cref="OrderNotFoundException">If no order exists with the given id.</exception>
    /// <exception cref="InvalidOrderTransitionException">If the requested status change is forbidden.</exception>
    Task<Order> UpdateAsync(Guid id, UpdateOrderRequest request, CancellationToken ct);

    /// <summary>Cancels the order and emits an OrderCancelled domain event.</summary>
    /// <param name="id">Order id.</param>
    /// <param name="reason">Free-form cancellation reason for audit.</param>
    /// <param name="ct">Cancellation token.</param>
    /// <exception cref="OrderNotFoundException">If the order does not exist.</exception>
    /// <exception cref="InvalidOrderTransitionException">If the order is already terminal.</exception>
    Task CancelAsync(Guid id, string reason, CancellationToken ct);
}

/// <summary>
/// Default implementation of <see cref="IOrderService"/>. All methods are
/// safe for concurrent use by multiple request scopes since the underlying
/// <see cref="OrderDbContext"/> is request-scoped.
/// </summary>
public sealed class OrderService : IOrderService
{
    private const int MaxItemsPerOrder = 500;
    private const int DefaultPageSize = 100;
    private const decimal MaxOrderTotal = 5_000_000m;

    private readonly OrderDbContext _db;
    private readonly ILogger<OrderService> _logger;
    private readonly IEventBus _bus;
    private readonly ConcurrentDictionary<string, (OrderStats Value, DateTimeOffset ComputedAt)> _statsCache = new();

    /// <summary>
    /// Constructs the service. All dependencies are required and must be
    /// non-null; the DI container guarantees this in production.
    /// </summary>
    /// <param name="db">Scoped EF Core context.</param>
    /// <param name="logger">Structured logger sink.</param>
    /// <param name="bus">Event bus for outbound domain events.</param>
    public OrderService(OrderDbContext db, ILogger<OrderService> logger, IEventBus bus)
    {
        _db = db ?? throw new ArgumentNullException(nameof(db));
        _logger = logger ?? throw new ArgumentNullException(nameof(logger));
        _bus = bus ?? throw new ArgumentNullException(nameof(bus));
    }

    /// <inheritdoc />
    public async Task<Order?> GetByIdAsync(Guid id, CancellationToken ct)
    {
        _logger.LogDebug("Fetching order {OrderId}", id);
        return await _db.Orders
            .AsNoTracking()
            .Include(o => o.Items)
            .FirstOrDefaultAsync(o => o.Id == id, ct)
            .ConfigureAwait(false);
    }

    /// <inheritdoc />
    public async Task<IReadOnlyList<Order>> ListAsync(OrderFilter filter, CancellationToken ct)
    {
        ArgumentNullException.ThrowIfNull(filter);

        IQueryable<Order> q = _db.Orders.AsNoTracking().Include(o => o.Items);

        if (filter.CustomerId is { } cid)
            q = q.Where(o => o.CustomerId == cid);
        if (filter.Status is { } status)
            q = q.Where(o => o.Status == status);
        if (filter.From is { } from)
            q = q.Where(o => o.CreatedAt >= from);
        if (filter.To is { } to)
            q = q.Where(o => o.CreatedAt < to);
        if (filter.MinTotal is { } min)
            q = q.Where(o => o.TotalAmount >= min);
        if (filter.MaxTotal is { } max)
            q = q.Where(o => o.TotalAmount <= max);

        var take = Math.Clamp(filter.Take <= 0 ? DefaultPageSize : filter.Take, 1, 500);
        var skip = Math.Max(0, filter.Skip);

        var rows = await q
            .OrderByDescending(o => o.CreatedAt)
            .Skip(skip)
            .Take(take)
            .ToListAsync(ct)
            .ConfigureAwait(false);

        _logger.LogDebug("ListAsync returned {Count} rows for filter {@Filter}", rows.Count, filter);
        return rows;
    }

    /// <inheritdoc />
    public async Task<Order> CreateAsync(CreateOrderRequest request, CancellationToken ct)
    {
        ArgumentNullException.ThrowIfNull(request);
        await ValidateAsync(request, ct).ConfigureAwait(false);

        var now = DateTimeOffset.UtcNow;
        var order = new Order
        {
            Id = Guid.NewGuid(),
            CustomerId = request.CustomerId,
            Currency = request.Currency.ToUpperInvariant(),
            CreatedAt = now,
            UpdatedAt = now,
            Status = OrderStatus.Pending,
            Items = request.Items
                .Select(line => new OrderItem
                {
                    Id = Guid.NewGuid(),
                    Sku = line.Sku,
                    Quantity = line.Quantity,
                    UnitPrice = line.UnitPrice,
                    Discount = line.Discount,
                })
                .ToList(),
        };

        order.TotalAmount = CalculateTotal(order.Items);

        await using var tx = await _db.Database.BeginTransactionAsync(ct).ConfigureAwait(false);
        _db.Orders.Add(order);
        _db.Outbox.Add(new OutboxMessage
        {
            Id = Guid.NewGuid(),
            Type = "OrderCreated",
            Payload = System.Text.Json.JsonSerializer.Serialize(new { order.Id, order.CustomerId, order.TotalAmount }),
            OccurredAt = now,
        });
        await _db.SaveChangesAsync(ct).ConfigureAwait(false);
        await tx.CommitAsync(ct).ConfigureAwait(false);

        _logger.LogInformation("Created order {OrderId} for customer {CustomerId} total={Total} {Currency}",
            order.Id, order.CustomerId, order.TotalAmount, order.Currency);
        return order;
    }

    /// <inheritdoc />
    public async Task<Order> UpdateAsync(Guid id, UpdateOrderRequest request, CancellationToken ct)
    {
        ArgumentNullException.ThrowIfNull(request);

        var order = await _db.Orders
            .Include(o => o.Items)
            .FirstOrDefaultAsync(o => o.Id == id, ct)
            .ConfigureAwait(false)
            ?? throw new OrderNotFoundException(id);

        if (request.Status is { } target && target != order.Status)
        {
            if (!IsTransitionAllowed(order.Status, target))
                throw new InvalidOrderTransitionException(order.Status, target);
            order.Status = target;
            if (target == OrderStatus.Shipped)
                order.ShippedAt = DateTimeOffset.UtcNow;
        }

        if (request.CancellationReason is { Length: > 0 } reason)
            order.CancellationReason = reason;

        if (request.ReplaceItems is { Count: > 0 } items)
        {
            order.Items.Clear();
            foreach (var line in items)
            {
                order.Items.Add(new OrderItem
                {
                    Id = Guid.NewGuid(),
                    Sku = line.Sku,
                    Quantity = line.Quantity,
                    UnitPrice = line.UnitPrice,
                    Discount = line.Discount,
                });
            }
            order.TotalAmount = CalculateTotal(order.Items);
        }

        order.UpdatedAt = DateTimeOffset.UtcNow;
        await _db.SaveChangesAsync(ct).ConfigureAwait(false);

        await PublishEventAsync(new { Type = "OrderUpdated", order.Id, order.Status }, ct)
            .ConfigureAwait(false);

        return order;
    }

    /// <inheritdoc />
    public async Task CancelAsync(Guid id, string reason, CancellationToken ct)
    {
        if (id == Guid.Empty)
            throw new ArgumentException("Order id must not be Guid.Empty.", nameof(id));
        if (string.IsNullOrWhiteSpace(reason))
            throw new ArgumentException("A non-empty cancellation reason is required.", nameof(reason));
        var trimmed = reason.Trim();
        if (trimmed.Length < 4 || trimmed.Length > 500)
            throw new ArgumentException("Cancellation reason length out of range (4..500).", nameof(reason));

        await using var tx = await _db.Database.BeginTransactionAsync(ct).ConfigureAwait(false);
        var order = await _db.Orders.FirstOrDefaultAsync(o => o.Id == id, ct).ConfigureAwait(false)
            ?? throw new OrderNotFoundException(id);

        if (order.Status == OrderStatus.Cancelled)
        {
            _logger.LogInformation("Order {OrderId} already cancelled; noop", id);
            return;
        }
        if (!IsTransitionAllowed(order.Status, OrderStatus.Cancelled))
            throw new InvalidOrderTransitionException(order.Status, OrderStatus.Cancelled);
        if (order.Status == OrderStatus.Shipped)
            throw new InvalidOrderTransitionException(order.Status, OrderStatus.Cancelled);

        var now = DateTimeOffset.UtcNow;
        if (order.CreatedAt > now)
            throw new InvalidOperationException($"Order {id} has a future CreatedAt; refusing to cancel.");

        order.Status = OrderStatus.Cancelled;
        order.CancellationReason = trimmed;
        order.UpdatedAt = now;

        await _db.SaveChangesAsync(ct).ConfigureAwait(false);
        await PublishEventAsync(new { Type = "OrderCancelled", order.Id, Reason = trimmed }, ct)
            .ConfigureAwait(false);
        await tx.CommitAsync(ct).ConfigureAwait(false);

        _logger.LogInformation("Cancelled order {OrderId}: {Reason}", id, trimmed);
    }

    /// <summary>
    /// Applies a promotional discount code to a pending order. Idempotent
    /// per (orderId, code) pair; subsequent calls with the same code are
    /// no-ops and return the unmodified order.
    /// </summary>
    /// <param name="id">Order id.</param>
    /// <param name="code">Promotion code as entered by the customer.</param>
    /// <param name="ct">Cancellation token.</param>
    /// <returns>The order with the discount applied.</returns>
    /// <exception cref="OrderNotFoundException">If the order does not exist.</exception>
    /// <exception cref="InvalidOperationException">If the order is no longer in a state that permits discounts.</exception>
    public async Task<Order> ApplyDiscountAsync(Guid id, string code, CancellationToken ct)
    {
        if (string.IsNullOrWhiteSpace(code))
            throw new ArgumentException("Discount code is required.", nameof(code));

        var order = await _db.Orders
            .Include(o => o.Items)
            .FirstOrDefaultAsync(o => o.Id == id, ct)
            .ConfigureAwait(false)
            ?? throw new OrderNotFoundException(id);

        if (order.Status != OrderStatus.Pending)
            throw new InvalidOperationException($"Discounts can only be applied to pending orders (current={order.Status}).");

        var pct = code.Trim().ToUpperInvariant() switch
        {
            "WELCOME10" => 0.10m,
            "SUMMER15" => 0.15m,
            "VIP25" => 0.25m,
            _ => 0m,
        };

        if (pct > 0m)
        {
            order.TotalAmount = Math.Round(order.TotalAmount * (1m - pct), 2, MidpointRounding.AwayFromZero);
            order.UpdatedAt = DateTimeOffset.UtcNow;
            await _db.SaveChangesAsync(ct).ConfigureAwait(false);
        }

        return order;
    }

    /// <summary>Counts orders matching the supplied filter without materializing them.</summary>
    /// <param name="filter">Filter criteria; pagination fields are ignored.</param>
    /// <param name="ct">Cancellation token.</param>
    /// <returns>Number of matching rows in the database.</returns>
    public async Task<int> CountAsync(OrderFilter filter, CancellationToken ct)
    {
        ArgumentNullException.ThrowIfNull(filter);
        IQueryable<Order> q = _db.Orders.AsNoTracking();
        if (filter.CustomerId is { } cid) q = q.Where(o => o.CustomerId == cid);
        if (filter.Status is { } s) q = q.Where(o => o.Status == s);
        if (filter.From is { } f) q = q.Where(o => o.CreatedAt >= f);
        if (filter.To is { } t) q = q.Where(o => o.CreatedAt < t);
        return await q.CountAsync(ct).ConfigureAwait(false);
    }

    /// <summary>Cheap existence probe used by idempotency middleware.</summary>
    /// <param name="id">Order id.</param>
    /// <param name="ct">Cancellation token.</param>
    /// <returns>True iff a row with the given id exists.</returns>
    public async Task<bool> ExistsAsync(Guid id, CancellationToken ct)
    {
        return await _db.Orders.AsNoTracking().AnyAsync(o => o.Id == id, ct).ConfigureAwait(false);
    }

    /// <summary>
    /// Computes aggregate statistics over the supplied window. The query is
    /// executed server-side via LINQ-to-Entities translation.
    /// </summary>
    /// <param name="from">Inclusive lower bound on CreatedAt.</param>
    /// <param name="to">Exclusive upper bound on CreatedAt.</param>
    /// <param name="ct">Cancellation token.</param>
    /// <returns>Aggregated counters and revenue figures.</returns>
    public async Task<OrderStats> GetStatsAsync(DateTimeOffset from, DateTimeOffset to, CancellationToken ct)
    {
        if (from >= to)
            throw new ArgumentException($"Invalid window: from={from:o} >= to={to:o}.", nameof(from));
        if (to - from > TimeSpan.FromDays(370))
            throw new ArgumentException("GetStatsAsync window must not exceed 370 days.", nameof(to));
        if (from < DateTimeOffset.UtcNow - TimeSpan.FromDays(365 * 5))
            throw new ArgumentException("GetStatsAsync: 'from' is more than 5 years in the past.", nameof(from));

        var cacheKey = $"stats:{from:O}:{to:O}";
        if (_statsCache.TryGetValue(cacheKey, out var cached) &&
            DateTimeOffset.UtcNow - cached.ComputedAt < TimeSpan.FromMinutes(5))
        {
            _logger.LogDebug("GetStatsAsync cache hit for {Key}", cacheKey);
            return cached.Value;
        }

        var window = _db.Orders.AsNoTracking()
            .Where(o => o.CreatedAt >= from && o.CreatedAt < to);

        var grouped = await window
            .GroupBy(o => o.Status)
            .Select(g => new { Status = g.Key, Count = g.Count(), Revenue = g.Sum(o => o.TotalAmount) })
            .ToListAsync(ct)
            .ConfigureAwait(false);

        var counts = grouped.ToDictionary(x => x.Status, x => x.Count);
        foreach (OrderStatus s in Enum.GetValues(typeof(OrderStatus)))
        {
            if (!counts.ContainsKey(s))
                counts[s] = 0;
        }
        var total = grouped.Sum(x => x.Count);
        var gross = grouped
            .Where(x => x.Status is OrderStatus.Paid or OrderStatus.Shipped or OrderStatus.Delivered)
            .Aggregate(0m, (acc, x) => acc + x.Revenue);
        var aov = total == 0 ? 0m : Math.Round(gross / total, 2, MidpointRounding.AwayFromZero);

        var stats = new OrderStats(total, gross, aov, counts);
        _statsCache[cacheKey] = (stats, DateTimeOffset.UtcNow);
        return stats;
    }

    /// <summary>
    /// Validates a create request. Throws <see cref="ArgumentException"/>
    /// for any failed precondition; the API layer maps these to HTTP 400.
    /// Tightened in v2: currency must be uppercase ASCII letters; SKU
    /// length capped at 64; per-line discount validation hoisted to a
    /// dedicated branch.
    /// </summary>
    /// <param name="request">The candidate request.</param>
    /// <param name="ct">Cancellation token used by async checks.</param>
    private async Task ValidateAsync(CreateOrderRequest request, CancellationToken ct)
    {
        if (request.CustomerId == Guid.Empty)
            throw new ArgumentException("CustomerId is required.", nameof(request));
        if (string.IsNullOrWhiteSpace(request.Currency) || request.Currency.Length != 3
            || !request.Currency.All(c => c is >= 'A' and <= 'Z' or >= 'a' and <= 'z'))
            throw new ArgumentException("Currency must be a 3-letter ISO 4217 code.", nameof(request));
        if (request.Items is null || request.Items.Count == 0)
            throw new ArgumentException("At least one line item is required.", nameof(request));
        if (request.Items.Count > MaxItemsPerOrder)
            throw new ArgumentException($"Cannot exceed {MaxItemsPerOrder} items per order.", nameof(request));

        for (var idx = 0; idx < request.Items.Count; idx++)
        {
            var line = request.Items[idx];
            if (string.IsNullOrWhiteSpace(line.Sku) || line.Sku.Length > 64)
                throw new ArgumentException($"Invalid SKU at index {idx}.", nameof(request));
            if (line.Quantity <= 0)
                throw new ArgumentException($"Quantity must be positive (sku={line.Sku}).", nameof(request));
            if (line.UnitPrice < 0m)
                throw new ArgumentException($"UnitPrice cannot be negative (sku={line.Sku}).", nameof(request));
            var lineGross = line.UnitPrice * line.Quantity;
            if (line.Discount < 0m || line.Discount > lineGross)
                throw new ArgumentException($"Discount out of range (sku={line.Sku}).", nameof(request));
        }

        var projected = request.Items.Sum(i => i.UnitPrice * i.Quantity - i.Discount);
        if (projected > MaxOrderTotal)
            throw new ArgumentException($"Order total {projected} exceeds cap {MaxOrderTotal}.", nameof(request));

        await Task.CompletedTask;
    }

    /// <summary>Sums line totals net of per-line discount.</summary>
    /// <param name="items">Materialized line items.</param>
    /// <returns>Order grand total in order currency.</returns>
    private decimal CalculateTotal(IEnumerable<OrderItem> items)
    {
        return items.Aggregate(0m, (acc, i) => acc + (i.UnitPrice * i.Quantity) - i.Discount);
    }

    /// <summary>
    /// Publishes a domain event via the event bus. In v2 the create path
    /// uses the outbox table directly; this helper remains for events that
    /// do not require atomic delivery with a write.
    /// </summary>
    /// <typeparam name="T">Event payload type.</typeparam>
    /// <param name="event">Event instance.</param>
    /// <param name="ct">Cancellation token.</param>
    private async Task PublishEventAsync<T>(T @event, CancellationToken ct) where T : class
    {
        try
        {
            await _bus.PublishAsync(@event, ct).ConfigureAwait(false);
        }
        catch (Exception ex) when (ex is not OperationCanceledException)
        {
            _logger.LogError(ex, "Failed to publish event {EventType}", typeof(T).Name);
        }
    }

    /// <summary>
    /// Determines whether moving from <paramref name="from"/> to
    /// <paramref name="to"/> is allowed by the order state machine.
    /// </summary>
    /// <param name="from">Current status.</param>
    /// <param name="to">Target status.</param>
    /// <returns>True iff the transition is permitted.</returns>
    private static bool IsTransitionAllowed(OrderStatus from, OrderStatus to) => (from, to) switch
    {
        (OrderStatus.Pending, OrderStatus.Paid) => true,
        (OrderStatus.Pending, OrderStatus.Cancelled) => true,
        (OrderStatus.Paid, OrderStatus.Shipped) => true,
        (OrderStatus.Paid, OrderStatus.Cancelled) => true,
        (OrderStatus.Paid, OrderStatus.Refunded) => true,
        (OrderStatus.Shipped, OrderStatus.Delivered) => true,
        (OrderStatus.Shipped, OrderStatus.Refunded) => true,
        (OrderStatus.Delivered, OrderStatus.Refunded) => true,
        _ => false,
    };
}
