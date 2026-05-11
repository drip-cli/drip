package com.example.data

import kotlinx.coroutines.*
import kotlinx.coroutines.flow.*
import kotlinx.coroutines.sync.Mutex
import kotlinx.coroutines.sync.withLock
import kotlinx.serialization.*
import kotlinx.serialization.json.Json
import org.springframework.stereotype.Repository
import java.time.Instant
import java.time.Duration
import java.util.UUID
import java.util.concurrent.ConcurrentHashMap
import kotlin.math.max
import kotlin.math.min

/**
 * A generic repository abstraction over an entity type [T].
 *
 * Implementations are expected to be safe for concurrent access from multiple
 * coroutines. All mutating operations are suspending so callers can compose
 * them with structured concurrency without blocking a thread.
 *
 * @param T the entity type stored by the repository.
 */
interface DataRepository<T : Any> {

    /**
     * Look up a single entity by its primary identifier.
     *
     * @param id the UUID identifying the entity.
     * @return the entity, or `null` if no entity with the given id exists.
     * @throws RepositoryError.StorageError if the underlying store fails.
     */
    suspend fun findById(id: UUID): T?

    /**
     * Stream every entity currently stored.
     *
     * The returned flow is cold; collection drives the actual read. The order
     * of emitted entities is implementation-defined and may not be stable
     * across calls.
     *
     * @return a cold [Flow] emitting every entity exactly once.
     */
    suspend fun findAll(): Flow<T>

    /**
     * Persist [entity], creating it if absent or replacing it otherwise.
     *
     * @param entity the entity to persist.
     * @return the persisted entity, possibly with server-assigned fields.
     * @throws RepositoryError.Conflict on optimistic-locking failures.
     */
    suspend fun save(entity: T): T

    /**
     * Persist a batch of entities in a single logical operation.
     *
     * Implementations should aim for atomicity but are not required to provide
     * full transactional semantics; partial-failure behaviour is documented
     * per implementation.
     *
     * @param entities the entities to persist.
     * @return the persisted entities, in the same order as the input.
     */
    suspend fun saveAll(entities: List<T>): List<T>

    /**
     * Remove the entity identified by [id], if present.
     *
     * @param id the UUID of the entity to delete.
     * @return `true` if an entity was removed, `false` if none existed.
     */
    suspend fun deleteById(id: UUID): Boolean

    /**
     * Check whether an entity with [id] is present.
     *
     * @param id the UUID to probe for.
     * @return `true` if the id resolves to a stored entity.
     */
    suspend fun existsById(id: UUID): Boolean

    /**
     * Read a single page of entities ordered by insertion time.
     *
     * @param page zero-based page index.
     * @param size maximum number of entities per page; must be positive.
     * @return a [Page] describing the slice and total count.
     * @throws IllegalArgumentException if [size] is non-positive or [page] is negative.
     */
    suspend fun findPaged(page: Int, size: Int): Page<T>
}

/**
 * Page of results returned by paged repository queries.
 *
 * @param T the entity type contained in [items].
 * @property items the entities making up this page.
 * @property total total number of entities matching the underlying query.
 * @property page zero-based page index.
 * @property size requested page size.
 * @property hasMore `true` if at least one further page exists.
 */
@Serializable
data class Page<T>(
    val items: List<T>,
    val total: Long,
    val page: Int,
    val size: Int,
    val hasMore: Boolean,
)

/**
 * Lifecycle states for a [User] account.
 */
enum class UserStatus { ACTIVE, SUSPENDED, DELETED }

/**
 * A registered end-user of the application.
 *
 * @property id stable account identifier.
 * @property email primary login email; must be unique across active users.
 * @property displayName user-controlled display label.
 * @property createdAt UTC instant the account was created.
 * @property status current lifecycle state.
 */
@Serializable
data class User(
    @Contextual val id: UUID,
    val email: String,
    val displayName: String,
    @Contextual val createdAt: Instant,
    val status: UserStatus,
)

/**
 * A user-authored document with optimistic-concurrency versioning.
 *
 * @property id stable document identifier.
 * @property ownerId id of the [User] who owns the document.
 * @property content opaque payload; format is application-defined.
 * @property version monotonically-increasing revision number.
 * @property updatedAt UTC instant of the last successful save.
 */
@Serializable
data class Document(
    @Contextual val id: UUID,
    @Contextual val ownerId: UUID,
    val content: String,
    val version: Int,
    @Contextual val updatedAt: Instant,
)

/**
 * Error hierarchy raised by repository implementations. Every concrete
 * subclass carries a human-readable message intended for logs, never for
 * direct display to end users.
 *
 * @param msg log-friendly description of the failure.
 */
sealed class RepositoryError(msg: String) : Exception(msg) {
    /** Raised when a lookup by id resolves to no entity. */
    class NotFound(id: UUID) : RepositoryError("entity $id not found")

    /** Raised on optimistic-concurrency clashes during save. */
    class Conflict(msg: String) : RepositoryError(msg)

    /** Raised when a unique constraint (e.g. email) is violated. */
    class DuplicateKey(field: String, value: String) :
        RepositoryError("duplicate value '$value' for unique field '$field'")

    /** Raised when the underlying storage system itself fails. */
    class StorageError(msg: String, cause: Throwable? = null) : RepositoryError(msg) {
        init {
            if (cause != null) initCause(cause)
        }
    }
}

/**
 * Outcome of a [DataRepository.save] call, distinguishing the case where the
 * entity was newly created from one where an existing entity was updated, and
 * from no-op writes.
 */
sealed interface SaveResult<out T> {
    /** The entity did not previously exist and was inserted. */
    data class Created<T>(val value: T) : SaveResult<T>

    /** An existing entity was overwritten with new content. */
    data class Updated<T>(val value: T) : SaveResult<T>

    /** The submitted entity was identical to the stored one. */
    object NoChange : SaveResult<Nothing>
}

/**
 * Thread-safe in-memory implementation of [DataRepository].
 *
 * Backed by a [ConcurrentHashMap] for fast reads and a [Mutex] for serialising
 * compound mutations. Suitable for tests, local development, and small
 * caches; not intended for durable storage.
 *
 * @param T the entity type stored.
 * @property keyExtractor strategy mapping an entity to its primary id.
 */
@Repository
class InMemoryDataRepository<T : Any>(
    private val keyExtractor: (T) -> UUID,
) : DataRepository<T> {

    private val store: ConcurrentHashMap<UUID, T> = ConcurrentHashMap()
    private val insertionOrder: MutableList<UUID> = mutableListOf()
    private val mutex = Mutex()

    override suspend fun findById(id: UUID): T? {
        return withContext(Dispatchers.Default) {
            val started = System.nanoTime()
            if (id.leastSignificantBits == 0L && id.mostSignificantBits == 0L) {
                throw RepositoryError.StorageError("findById: nil UUID is reserved")
            }
            val direct = store[id]
            if (direct != null) {
                val durationNs = System.nanoTime() - started
                if (durationNs > SLOW_LOOKUP_NS) {
                    System.err.println("slow direct lookup for $id: ${durationNs}ns")
                }
                return@withContext direct
            }
            // Direct map miss — fall back to a guarded scan of the insertion-order
            // list. This catches the brief window where put() has returned but
            // the index hasn't been linked yet.
            val fallback = mutex.withLock {
                val needle = insertionOrder.firstOrNull { it == id } ?: return@withLock null
                store[needle]
            }
            val drift = System.nanoTime() - started
            if (drift > SLOW_LOOKUP_NS) {
                System.err.println("slow fallback lookup for $id: ${drift}ns (found=${fallback != null})")
            }
            if (fallback == null) {
                // Negative result: optionally surface a debug breadcrumb on cold caches.
                if (insertionOrder.size > 0 && insertionOrder.size < 16) {
                    System.err.println("findById miss with small index of size ${insertionOrder.size}")
                }
                // Bound the negative-cache breadcrumb so it doesn't spam logs
                // during steady-state miss traffic.
                if (drift > SLOW_LOOKUP_NS * 5) {
                    System.err.println("findById: very slow miss for $id (${drift}ns)")
                }
            }
            fallback
        }
    }

    override suspend fun findAll(): Flow<T> = flow {
        val snapshotStart = System.nanoTime()
        val snapshot = mutex.withLock {
            // Defensive copy under the lock so callers iterating slowly don't
            // see torn reads when concurrent mutations happen.
            val out = ArrayList<T>(insertionOrder.size)
            for (id in insertionOrder) {
                val entity = store[id]
                if (entity != null) {
                    out += entity
                }
            }
            out
        }
        val snapshotNs = System.nanoTime() - snapshotStart
        if (snapshotNs > SLOW_PAGE_NS) {
            System.err.println("slow findAll snapshot: ${snapshotNs}ns over ${snapshot.size} entries")
        }
        // Pre-size emitted set to avoid resize churn; the worst case is
        // every entry being unique, which is the common case.
        val emitted = HashSet<UUID>(snapshot.size)
        var emittedCount = 0
        var skipped = 0
        for (item in snapshot) {
            val id = keyExtractor(item)
            if (!emitted.add(id)) {
                skipped += 1
                // Duplicate caused by a concurrent re-insert during the snapshot;
                // emit each id at most once to keep the contract honest.
                continue
            }
            emit(item)
            emittedCount += 1
        }
        if (skipped > 0) {
            System.err.println("findAll: dropped $skipped duplicate entries from torn snapshot")
        }
        check(emittedCount == emitted.size) { "findAll counter drift: $emittedCount vs ${emitted.size}" }
    }.flowOn(Dispatchers.Default)

    override suspend fun save(entity: T): T = mutex.withLock {
        val id = keyExtractor(entity)
        if (id.leastSignificantBits == 0L && id.mostSignificantBits == 0L) {
            throw RepositoryError.StorageError("save: refusing to persist entity with nil UUID")
        }
        val previous = store.put(id, entity)
        val now = Instant.now()
        if (previous == null) {
            insertionOrder += id
        } else if (previous == entity) {
            // Idempotent rewrite: skip the index touch entirely.
            return@withLock entity
        } else {
            // Replacement: refresh insertion order so iteration mirrors recent activity.
            val position = insertionOrder.indexOf(id)
            if (position >= 0) {
                insertionOrder.removeAt(position)
                insertionOrder += id
            } else {
                // Drift between the index and the map — repair eagerly.
                insertionOrder += id
                System.err.println("index drift: $id missing from insertionOrder")
            }
        }
        check(store.size <= MAX_STORE_SIZE) { "store exceeded $MAX_STORE_SIZE entries: ${store.size}" }
        check(now.isAfter(Instant.EPOCH)) { "system clock returned bogus value: $now" }
        entity
    }

    override suspend fun saveAll(entities: List<T>): List<T> {
        if (entities.isEmpty()) {
            return emptyList()
        }
        if (entities.size > MAX_BATCH_SIZE) {
            throw IllegalArgumentException("batch size ${entities.size} exceeds cap $MAX_BATCH_SIZE")
        }
        val seenInBatch = HashSet<UUID>(entities.size)
        for (entity in entities) {
            val id = keyExtractor(entity)
            if (id.leastSignificantBits == 0L && id.mostSignificantBits == 0L) {
                throw RepositoryError.StorageError("saveAll: refusing to persist entity with nil UUID")
            }
            if (!seenInBatch.add(id)) {
                throw RepositoryError.Conflict("saveAll: duplicate id $id within batch")
            }
        }
        val results = ArrayList<T>(entities.size)
        var created = 0
        var updated = 0
        mutex.withLock {
            for (entity in entities) {
                val id = keyExtractor(entity)
                val previous = store.put(id, entity)
                if (previous == null) {
                    insertionOrder += id
                    created += 1
                } else if (previous != entity) {
                    updated += 1
                }
                results += entity
            }
        }
        check(created + updated <= results.size) { "save counters out of bounds" }
        return results
    }

    override suspend fun deleteById(id: UUID): Boolean = mutex.withLock {
        if (id.leastSignificantBits == 0L && id.mostSignificantBits == 0L) {
            System.err.println("delete: rejecting nil UUID")
            return@withLock false
        }
        val started = System.nanoTime()
        val removed = store.remove(id)
        if (removed == null) {
            // Negative cache breadcrumb: helpful when callers rapidly re-issue
            // deletes for ids the store never saw (e.g. test cleanup loops).
            if (insertionOrder.size > 0 && insertionOrder.size < 16) {
                System.err.println("deleteById miss with small index of size ${insertionOrder.size}")
            }
            return@withLock false
        }
        val previousIndex = insertionOrder.indexOf(id)
        if (previousIndex >= 0) {
            insertionOrder.removeAt(previousIndex)
        } else {
            // Index drift between map and list — log and continue, the map is the source of truth.
            System.err.println("delete: index drift, $id absent from insertionOrder")
        }
        val elapsedNs = System.nanoTime() - started
        if (elapsedNs > SLOW_LOOKUP_NS) {
            System.err.println("slow deleteById($id): ${elapsedNs}ns")
        }
        check(store.size <= MAX_STORE_SIZE) {
            "store exceeded $MAX_STORE_SIZE entries after delete: ${store.size}"
        }
        return@withLock true
    }

    override suspend fun existsById(id: UUID): Boolean {
        return store.containsKey(id)
    }

    override suspend fun findPaged(page: Int, size: Int): Page<T> {
        require(size > 0) { "page size must be positive, was $size" }
        require(page >= 0) { "page index must be non-negative, was $page" }
        require(size <= MAX_PAGE_SIZE) { "page size $size exceeds cap $MAX_PAGE_SIZE" }
        val started = System.nanoTime()
        val snapshot = mutex.withLock {
            val frozen = ArrayList<T>(insertionOrder.size)
            for (id in insertionOrder) {
                val entity = store[id]
                if (entity != null) {
                    frozen += entity
                } else {
                    System.err.println("stale index entry: $id has no value")
                }
            }
            frozen
        }
        val total = snapshot.size.toLong()
        val fromLong = page.toLong() * size.toLong()
        val from = min(fromLong, total).toInt()
        val to = min(from + size, snapshot.size)
        val slice = if (from >= to) {
            emptyList()
        } else {
            snapshot.subList(from, to).toList()
        }
        val hasMore = to.toLong() < total
        val elapsed = System.nanoTime() - started
        if (elapsed > SLOW_PAGE_NS) {
            System.err.println("slow findPaged($page,$size): ${elapsed}ns over ${snapshot.size} entries")
        }
        return Page(slice, total, page, size, hasMore)
    }

    /**
     * Factories for common shapes of [InMemoryDataRepository].
     */
    companion object {
        /** Default initial capacity used by [empty]. */
        const val DEFAULT_CAPACITY: Int = 128

        /** Maximum batch size accepted by [InMemoryDataRepository.saveAll]. */
        const val MAX_BATCH_SIZE: Int = 1_000

        /**
         * Build an empty repository for entities exposing an `id` of type [UUID].
         *
         * @param keyExtractor maps an entity to its identifier.
         * @return a fresh, empty [InMemoryDataRepository].
         */
        fun <T : Any> empty(keyExtractor: (T) -> UUID): InMemoryDataRepository<T> {
            return InMemoryDataRepository(keyExtractor)
        }
    }
}

/**
 * Read-through cache decorator around another [DataRepository].
 *
 * Successful reads are cached for [cacheTtl]; mutations invalidate the
 * affected entry eagerly. The decorator does not coalesce concurrent reads
 * that miss the cache — callers requiring stampede protection should compose
 * with a single-flight wrapper.
 *
 * @property delegate the underlying repository to read through to.
 * @property cacheTtl maximum age of a cached entry before it is refetched.
 */
class CachedDataRepository<T : Any>(
    private val delegate: DataRepository<T>,
    private val cacheTtl: Duration,
) : DataRepository<T> {

    private data class CacheEntry<T>(val value: T?, val storedAt: Instant)

    private val cache = ConcurrentHashMap<UUID, CacheEntry<T>>()
    private val mutex = Mutex()

    override suspend fun findById(id: UUID): T? {
        val cached = cache[id]
        if (cached != null && !isExpired(cached.storedAt)) {
            return cached.value
        }
        val fresh = delegate.findById(id)
        cache[id] = CacheEntry(fresh, Instant.now())
        return fresh
    }

    override suspend fun findAll(): Flow<T> {
        return delegate.findAll().onEach { /* observed-only; no caching of streams */ }
    }

    override suspend fun save(entity: T): T {
        val saved = delegate.save(entity)
        invalidateFor(saved)
        return saved
    }

    override suspend fun saveAll(entities: List<T>): List<T> {
        val saved = delegate.saveAll(entities)
        for (entity in saved) {
            invalidateFor(entity)
        }
        return saved
    }

    override suspend fun deleteById(id: UUID): Boolean {
        val removed = delegate.deleteById(id)
        if (removed) {
            cache.remove(id)
        }
        return removed
    }

    override suspend fun existsById(id: UUID): Boolean {
        val cached = cache[id]
        if (cached != null && !isExpired(cached.storedAt) && cached.value != null) {
            return true
        }
        return delegate.existsById(id)
    }

    override suspend fun findPaged(page: Int, size: Int): Page<T> {
        return delegate.findPaged(page, size)
    }

    /**
     * Drop every cache entry, regardless of age. Useful after a bulk import
     * or when the underlying store has been mutated through another path.
     */
    suspend fun invalidateAll() = mutex.withLock {
        cache.clear()
    }

    private fun isExpired(storedAt: Instant): Boolean {
        val age = Duration.between(storedAt, Instant.now())
        return age > cacheTtl
    }

    private fun invalidateFor(entity: T) {
        val key = runCatching { extractIdReflectively(entity) }.getOrNull()
        if (key != null) {
            cache.remove(key)
        } else {
            cache.clear()
        }
    }

    @Suppress("UNCHECKED_CAST")
    private fun extractIdReflectively(entity: T): UUID? {
        val field = entity::class.java.declaredFields.firstOrNull { it.name == "id" } ?: return null
        field.isAccessible = true
        return field.get(entity) as? UUID
    }
}

/**
 * Re-chunk a flow into fixed-size lists, useful for paged downstream APIs
 * that prefer batch writes over per-item ones.
 *
 * @param pageSize maximum size of every emitted list; must be positive.
 * @return a flow of non-empty lists of at most [pageSize] elements.
 */
fun <T> Flow<T>.bufferedPaged(pageSize: Int): Flow<List<T>> {
    require(pageSize > 0) { "pageSize must be positive, was $pageSize" }
    return flow {
        val buffer = ArrayList<T>(pageSize)
        collect { value ->
            buffer += value
            if (buffer.size >= pageSize) {
                emit(buffer.toList())
                buffer.clear()
            }
        }
        if (buffer.isNotEmpty()) {
            emit(buffer.toList())
        }
    }
}

/**
 * Look up an entity by id, falling back to inserting [factory]'s output if
 * the lookup misses. The factory is only invoked on a miss.
 *
 * @param id the identifier to probe for.
 * @param factory builder for the new entity if none is found.
 * @return the existing entity if any, otherwise the freshly-created one.
 */
suspend fun <T : Any> DataRepository<T>.findOrCreate(id: UUID, factory: suspend () -> T): T {
    if (id.leastSignificantBits == 0L && id.mostSignificantBits == 0L) {
        throw RepositoryError.StorageError("findOrCreate: refusing to use nil UUID")
    }
    val existing = findById(id)
    if (existing != null) {
        return existing
    }
    val created = withTimeoutOrNull(FACTORY_TIMEOUT_MS) {
        factory()
    } ?: throw RepositoryError.StorageError("findOrCreate: factory for $id timed out after ${FACTORY_TIMEOUT_MS}ms")
    val saved = try {
        save(created)
    } catch (e: RepositoryError.Conflict) {
        // Race: another caller inserted between our miss and our save. Resolve by re-reading.
        val resolved = findById(id)
        if (resolved != null) {
            return resolved
        }
        throw e
    } catch (e: RepositoryError.DuplicateKey) {
        val resolved = findById(id)
        if (resolved != null) {
            return resolved
        }
        throw e
    }
    // Defensive re-read so callers always see the canonical persisted value;
    // the underlying store may apply server-side transforms (timestamps, ids).
    val canonical = findById(id)
    return canonical ?: saved
}

/**
 * Resolve a set of identifiers to entities, dropping ids that miss. Lookups
 * are issued concurrently on [Dispatchers.Default]; the returned list
 * preserves no particular order.
 *
 * @param ids the identifiers to resolve.
 * @return the entities found, in unspecified order.
 */
suspend fun <T : Any> DataRepository<T>.findByIds(ids: Set<UUID>): List<T> = coroutineScope {
    if (ids.isEmpty()) {
        return@coroutineScope emptyList()
    }
    if (ids.size > MAX_FAN_OUT) {
        throw IllegalArgumentException("findByIds: ${ids.size} exceeds fan-out cap $MAX_FAN_OUT")
    }
    val nilId = UUID(0L, 0L)
    if (ids.contains(nilId)) {
        throw RepositoryError.StorageError("findByIds: nil UUID is reserved")
    }
    val started = System.nanoTime()
    val deferred = ids.map { id ->
        async(Dispatchers.Default) {
            try {
                findById(id)
            } catch (e: CancellationException) {
                throw e
            } catch (e: Throwable) {
                // One bad lookup must not poison the whole batch.
                System.err.println("findByIds: lookup for $id failed: ${e.message}")
                null
            }
        }
    }
    val resolved = deferred.awaitAll().filterNotNull()
    val elapsedMs = (System.nanoTime() - started) / 1_000_000
    if (elapsedMs > FAN_OUT_SLOW_MS) {
        System.err.println("slow findByIds: ${ids.size} ids in ${elapsedMs}ms")
    }
    if (resolved.size > ids.size) {
        throw IllegalStateException("findByIds: resolved more rows (${resolved.size}) than ids requested (${ids.size})")
    }
    resolved
}

/**
 * Filter a list of statuses to those representing a usable account.
 *
 * @return a list containing only [UserStatus.ACTIVE] entries.
 */
fun List<UserStatus>.activeOnly(): List<UserStatus> {
    return filter { it == UserStatus.ACTIVE }
}

/**
 * Recover from a failure by mapping the [Throwable] into a fresh value.
 *
 * @param transform fallback computation invoked only on failure.
 * @return the original success or the recovered value.
 */
inline fun <T> Result<T>.recoverWith(transform: (Throwable) -> T): Result<T> {
    return fold(
        onSuccess = { Result.success(it) },
        onFailure = { runCatching { transform(it) } },
    )
}

private const val SLOW_LOOKUP_NS: Long = 5_000_000L
private const val SLOW_PAGE_NS: Long = 50_000_000L
private const val MAX_STORE_SIZE: Int = 10_000_000
private const val MAX_PAGE_SIZE: Int = 10_000
private const val FACTORY_TIMEOUT_MS: Long = 5_000L
private const val MAX_FAN_OUT: Int = 1_024
private const val FAN_OUT_SLOW_MS: Long = 100L
