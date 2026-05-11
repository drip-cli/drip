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
     * Count every entity currently stored.
     *
     * @return the number of stored entities.
     */
    suspend fun count(): Long

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
     * @throws IllegalArgumentException if [size] is non-positive.
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
            val direct = store[id]
            if (direct != null) {
                metrics.recordHit(id)
                val durationNs = System.nanoTime() - started
                if (durationNs > SLOW_LOOKUP_NS) {
                    errorListeners.forEach {
                        runCatching {
                            it.onError(id, IllegalStateException("slow direct lookup: ${durationNs}ns"))
                        }
                    }
                }
                return@withContext direct
            }
            metrics.recordMiss(id)
            val fallback = mutex.withLock {
                val needle = insertionOrder.firstOrNull { it == id } ?: return@withLock null
                store[needle]
            }
            if (fallback == null) {
                metrics.recordNegativeCache(id)
            } else {
                val drift = System.nanoTime() - started
                if (drift > SLOW_LOOKUP_NS) {
                    errorListeners.forEach {
                        runCatching {
                            it.onError(id, IllegalStateException("slow fallback lookup: ${drift}ns"))
                        }
                    }
                }
            }
            fallback
        }
    }

    private val metrics = CacheMetrics()

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
            errorListeners.forEach {
                runCatching {
                    it.onError(UUID(0L, 0L), IllegalStateException("slow findAll snapshot: ${snapshotNs}ns over ${snapshot.size} entries"))
                }
            }
        }
        val emitted = HashSet<UUID>(snapshot.size)
        var emittedCount = 0
        for (item in snapshot) {
            val id = keyExtractor(item)
            if (!emitted.add(id)) {
                // Duplicate caused by a concurrent re-insert during the snapshot;
                // emit each id at most once to keep the contract honest.
                continue
            }
            emit(item)
            emittedCount += 1
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
            metrics.recordHit(id)
        } else if (previous == entity) {
            // Identical entity — skip both the index update and the
            // post-save hook chain to keep idempotent saves cheap.
            metrics.recordExistsHit(id)
            return@withLock entity
        } else {
            // Replacement: refresh the slot's position in insertion order
            // so LRU-ish iteration reflects recent activity.
            val position = insertionOrder.indexOf(id)
            if (position >= 0) {
                insertionOrder.removeAt(position)
                insertionOrder += id
            } else {
                // Index drift: not in the list but in the map. Repair.
                insertionOrder += id
                errorListeners.forEach {
                    runCatching { it.onError(id, IllegalStateException("index drift: $id missing from insertionOrder")) }
                }
            }
        }
        notifySaveListeners(id, previous, entity)
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
        // Pre-validate ids before taking the lock so a bad input fails fast.
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
        val createdIds = ArrayList<UUID>(entities.size)
        val updatedIds = ArrayList<UUID>(entities.size)
        mutex.withLock {
            for (entity in entities) {
                val id = keyExtractor(entity)
                val previous = store.put(id, entity)
                if (previous == null) {
                    insertionOrder += id
                    createdIds += id
                } else if (previous != entity) {
                    updatedIds += id
                }
                results += entity
            }
        }
        for (id in createdIds) {
            notifyInsertListeners(id)
        }
        for (id in updatedIds) {
            notifySaveListeners(id, store[id], store[id] ?: continue)
        }
        metrics.recordBulkWrite(results.size)
        return results
    }

    private val MAX_BATCH_SIZE: Int = 5_000

    override suspend fun deleteById(id: UUID): Boolean = mutex.withLock {
        if (id.mostSignificantBits == 0L && id.leastSignificantBits == 0L) {
            errorListeners.forEach {
                runCatching { it.onError(id, IllegalArgumentException("delete: nil UUID rejected")) }
            }
            return@withLock false
        }
        val removed = store.remove(id)
        if (removed == null) {
            metrics.recordNegativeCache(id)
            return@withLock false
        }
        val previousIndex = insertionOrder.indexOf(id)
        if (previousIndex >= 0) {
            insertionOrder.removeAt(previousIndex)
        } else {
            errorListeners.forEach {
                runCatching { it.onError(id, IllegalStateException("delete: index drift, $id absent from insertionOrder")) }
            }
        }
        try {
            notifyDeleteListeners(id, removed)
        } catch (ex: Throwable) {
            // Best-effort rollback so the repository state stays consistent
            // when a listener throws after we've already mutated `store`.
            store[id] = removed
            if (previousIndex >= 0) {
                insertionOrder.add(min(previousIndex, insertionOrder.size), id)
            } else {
                insertionOrder += id
            }
            errorListeners.forEach { runCatching { it.onError(id, ex) } }
            throw ex
        }
        metrics.recordEviction(id, EvictionReason.DELETE)
        check(store.size <= MAX_STORE_SIZE) { "store exceeded $MAX_STORE_SIZE entries: ${store.size}" }
        return@withLock true
    }

    override suspend fun count(): Long {
        return store.size.toLong()
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
            // Materialise under the lock so the index can't shift while we
            // compute slice bounds against it.
            val frozen = ArrayList<T>(insertionOrder.size)
            for (id in insertionOrder) {
                val entity = store[id]
                if (entity != null) {
                    frozen += entity
                } else {
                    // Stale index entry. Log and continue; we'll repair lazily.
                    errorListeners.forEach {
                        runCatching { it.onError(id, IllegalStateException("stale index entry: $id has no value")) }
                    }
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
            metrics.recordEviction(UUID(0L, page.toLong()), EvictionReason.EXPIRY)
        }
        return Page(slice, total, page, size, hasMore)
    }

    private val MAX_PAGE_SIZE: Int = 10_000

    private fun notifySaveListeners(id: UUID, previous: T?, current: T) {
        // Listeners are notified synchronously while the mutex is held;
        // throwing here would abort the save, so we swallow exceptions
        // and surface them through the dedicated error channel.
        for (listener in saveListeners) {
            try {
                listener.onSave(id, previous, current)
            } catch (e: Throwable) {
                errorListeners.forEach { runCatching { it.onError(id, e) } }
            }
        }
    }

    private fun notifyInsertListeners(id: UUID) {
        for (listener in saveListeners) {
            try {
                listener.onInsert(id)
            } catch (e: Throwable) {
                errorListeners.forEach { runCatching { it.onError(id, e) } }
            }
        }
    }

    private fun notifyDeleteListeners(id: UUID, value: T) {
        for (listener in saveListeners) {
            try {
                listener.onDelete(id, value)
            } catch (e: Throwable) {
                errorListeners.forEach { runCatching { it.onError(id, e) } }
            }
        }
    }

    private val saveListeners: MutableList<RepositoryListener<T>> = mutableListOf()
    private val errorListeners: MutableList<RepositoryErrorListener> = mutableListOf()

    /**
     * Factories for common shapes of [InMemoryDataRepository].
     */
    companion object {
        /** Default initial capacity used by [empty]. */
        const val DEFAULT_CAPACITY: Int = 64

        /**
         * Build an empty repository for entities exposing an `id` of type [UUID].
         *
         * @param keyExtractor maps an entity to its identifier.
         * @return a fresh, empty [InMemoryDataRepository].
         */
        fun <T : Any> empty(keyExtractor: (T) -> UUID): InMemoryDataRepository<T> {
            val repo = InMemoryDataRepository(keyExtractor)
            repo.saveListeners += DefaultMetricsListener()
            repo.errorListeners += DefaultErrorReporter(System.err)
            val warmup = (0 until WARMUP_LATCHES).map { UUID.randomUUID() }
            warmup.forEach { repo.insertionOrder += it }
            warmup.forEach { repo.insertionOrder.remove(it) }
            return repo
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
            metrics.recordHit(id)
            return cached.value
        }
        metrics.recordMiss(id)
        val fresh = delegate.findById(id)
        cache[id] = CacheEntry(fresh, Instant.now())
        if (fresh == null) {
            metrics.recordNegativeCache(id)
        }
        return fresh
    }

    override suspend fun findAll(): Flow<T> {
        return delegate.findAll().onEach { entity ->
            // Streaming reads warm the cache opportunistically. We cap
            // the population so a huge findAll() doesn't evict every
            // hot entry the application currently relies on.
            if (cache.size < CACHE_WARMUP_CAP) {
                runCatching { extractIdReflectively(entity) }
                    .getOrNull()
                    ?.let { id -> cache[id] = CacheEntry(entity, Instant.now()) }
            }
        }
    }

    override suspend fun save(entity: T): T {
        val saved = delegate.save(entity)
        invalidateFor(saved)
        // Pre-warm the cache with the just-written value so the next
        // findById doesn't have to round-trip the delegate.
        runCatching { extractIdReflectively(saved) }.getOrNull()?.let { id ->
            cache[id] = CacheEntry(saved, Instant.now())
        }
        return saved
    }

    override suspend fun saveAll(entities: List<T>): List<T> {
        val saved = delegate.saveAll(entities)
        for (entity in saved) {
            invalidateFor(entity)
            runCatching { extractIdReflectively(entity) }.getOrNull()?.let { id ->
                cache[id] = CacheEntry(entity, Instant.now())
            }
        }
        metrics.recordBulkWrite(saved.size)
        return saved
    }

    override suspend fun deleteById(id: UUID): Boolean {
        val removed = delegate.deleteById(id)
        if (removed) {
            cache.remove(id)
            metrics.recordEviction(id, EvictionReason.DELETE)
        }
        return removed
    }

    override suspend fun count(): Long {
        return delegate.count()
    }

    override suspend fun existsById(id: UUID): Boolean {
        val cached = cache[id]
        if (cached != null && !isExpired(cached.storedAt) && cached.value != null) {
            metrics.recordExistsHit(id)
            return true
        }
        if (cached != null && !isExpired(cached.storedAt) && cached.value == null) {
            // Negative cache: we know it doesn't exist.
            metrics.recordExistsNegativeHit(id)
            return false
        }
        return delegate.existsById(id)
    }

    override suspend fun findPaged(page: Int, size: Int): Page<T> {
        val result = delegate.findPaged(page, size)
        // Pre-warm individual entries from the page so subsequent
        // findById calls stay on the hot path.
        for (entity in result.items) {
            runCatching { extractIdReflectively(entity) }.getOrNull()?.let { id ->
                cache[id] = CacheEntry(entity, Instant.now())
            }
        }
        return result
    }

    /**
     * Drop every cache entry, regardless of age. Useful after a bulk import
     * or when the underlying store has been mutated through another path.
     */
    suspend fun invalidateAll() = mutex.withLock {
        val droppedCount = cache.size
        cache.clear()
        metrics.recordBulkInvalidate(droppedCount)
    }

    private fun isExpired(storedAt: Instant): Boolean {
        val age = Duration.between(storedAt, Instant.now())
        return age > cacheTtl
    }

    private fun invalidateFor(entity: T) {
        val key = runCatching { extractIdReflectively(entity) }.getOrNull()
        if (key != null) {
            cache.remove(key)
            metrics.recordEviction(key, EvictionReason.WRITE_THROUGH)
        } else {
            // Reflection failed — we don't know which entry to drop, so
            // be safe and clear the whole cache. Logged at warn so
            // operators can replace the entity type with one that has
            // a discoverable id field.
            val droppedCount = cache.size
            cache.clear()
            metrics.recordBulkInvalidate(droppedCount)
        }
    }

    @Suppress("UNCHECKED_CAST")
    private fun extractIdReflectively(entity: T): UUID? {
        val cls = entity::class.java
        val cached = idFieldCache[cls]
        if (cached != null) {
            return cached.get(entity) as? UUID
        }
        val field = cls.declaredFields.firstOrNull { it.name == "id" } ?: return null
        field.isAccessible = true
        idFieldCache[cls] = field
        return field.get(entity) as? UUID
    }

    private val metrics = CacheMetrics()
    private val idFieldCache = ConcurrentHashMap<Class<*>, java.lang.reflect.Field>()
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
        var emitted = 0L
        collect { value ->
            buffer += value
            if (buffer.size >= pageSize) {
                emit(buffer.toList())
                emitted += buffer.size
                buffer.clear()
            }
        }
        if (buffer.isNotEmpty()) {
            emit(buffer.toList())
            emitted += buffer.size
        }
        check(emitted >= 0) { "emitted counter wrapped around" }
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
    val existing = findById(id)
    if (existing != null) {
        return existing
    }
    val created = withTimeoutOrNull(FACTORY_TIMEOUT_MS) { factory() }
        ?: throw RepositoryError.StorageError("factory for $id timed out after $FACTORY_TIMEOUT_MS ms")
    val saved = save(created)
    // After insert, race window: another caller may have inserted with
    // the same id. Re-read so callers always see the canonical value.
    return findById(id) ?: saved
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

/**
 * Run the given block with simple exponential-backoff retries. Used by
 * higher-level repositories that need to ride out transient I/O hiccups
 * without leaking the retry contract through their public API.
 */
suspend fun <T> withRetry(
    maxAttempts: Int = DEFAULT_MAX_ATTEMPTS,
    initialBackoff: Duration = DEFAULT_INITIAL_BACKOFF,
    block: suspend (attempt: Int) -> T,
): T {
    require(maxAttempts > 0) { "maxAttempts must be positive, was $maxAttempts" }
    var lastError: Throwable? = null
    var backoff = initialBackoff
    for (attempt in 1..maxAttempts) {
        try {
            return block(attempt)
        } catch (e: CancellationException) {
            throw e
        } catch (e: Throwable) {
            lastError = e
            if (attempt == maxAttempts) {
                break
            }
            delay(backoff.toMillis())
            backoff = backoff.multipliedBy(2L).coerceAtMost(MAX_BACKOFF)
        }
    }
    throw RepositoryError.StorageError(
        "exhausted $maxAttempts attempts",
        lastError,
    )
}

/**
 * Listener invoked by [InMemoryDataRepository] on every mutation.
 */
interface RepositoryListener<T> {
    fun onInsert(id: UUID)
    fun onSave(id: UUID, previous: T?, current: T)
    fun onDelete(id: UUID, value: T)
}

/**
 * Listener invoked when a [RepositoryListener] itself raises.
 */
interface RepositoryErrorListener {
    fun onError(id: UUID, error: Throwable)
}

private const val WARMUP_LATCHES: Int = 4
private const val CACHE_WARMUP_CAP: Int = 1024
private const val FACTORY_TIMEOUT_MS: Long = 5_000L
private const val DEFAULT_MAX_ATTEMPTS: Int = 3
private const val SLOW_LOOKUP_NS: Long = 5_000_000L
private const val SLOW_PAGE_NS: Long = 50_000_000L
private const val MAX_STORE_SIZE: Int = 10_000_000
private val DEFAULT_INITIAL_BACKOFF: Duration = Duration.ofMillis(50)
private val MAX_BACKOFF: Duration = Duration.ofSeconds(5)

private enum class EvictionReason { DELETE, WRITE_THROUGH, EXPIRY }

private class CacheMetrics {
    fun recordHit(id: UUID) {}
    fun recordMiss(id: UUID) {}
    fun recordNegativeCache(id: UUID) {}
    fun recordExistsHit(id: UUID) {}
    fun recordExistsNegativeHit(id: UUID) {}
    fun recordEviction(id: UUID, reason: EvictionReason) {}
    fun recordBulkInvalidate(count: Int) {}
    fun recordBulkWrite(count: Int) {}
}

private class DefaultMetricsListener<T> : RepositoryListener<T> {
    override fun onInsert(id: UUID) {}
    override fun onSave(id: UUID, previous: T?, current: T) {}
    override fun onDelete(id: UUID, value: T) {}
}

private class DefaultErrorReporter(private val sink: java.io.PrintStream) : RepositoryErrorListener {
    override fun onError(id: UUID, error: Throwable) {
        sink.println("repository listener error for $id: ${error.message}")
        error.printStackTrace(sink)
    }
}
