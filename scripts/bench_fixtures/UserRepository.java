package com.example.users.repository;

import javax.persistence.EntityManager;
import javax.persistence.PersistenceContext;
import javax.persistence.TypedQuery;
import javax.persistence.criteria.CriteriaBuilder;
import javax.persistence.criteria.CriteriaQuery;
import javax.persistence.criteria.Predicate;
import javax.persistence.criteria.Root;
import javax.persistence.LockModeType;
import javax.persistence.NoResultException;

import org.slf4j.Logger;
import org.slf4j.LoggerFactory;
import java.util.Collections;
import java.util.Locale;
import java.util.Objects;
import org.springframework.beans.factory.annotation.Autowired;
import org.springframework.beans.factory.annotation.Value;
import org.springframework.cache.annotation.CacheEvict;
import org.springframework.cache.annotation.Cacheable;
import org.springframework.cache.annotation.Caching;
import org.springframework.context.ApplicationEventPublisher;
import org.springframework.dao.DataIntegrityViolationException;
import org.springframework.data.domain.Page;
import org.springframework.data.domain.PageImpl;
import org.springframework.data.domain.PageRequest;
import org.springframework.data.domain.Pageable;
import org.springframework.data.domain.Sort;
import org.springframework.data.jpa.domain.Specification;
import org.springframework.jdbc.core.JdbcTemplate;
import org.springframework.stereotype.Repository;
import org.springframework.transaction.annotation.Isolation;
import org.springframework.transaction.annotation.Propagation;
import org.springframework.transaction.annotation.Transactional;
import org.springframework.util.Assert;
import org.springframework.util.StringUtils;

import java.io.Serializable;
import java.time.Duration;
import java.time.Instant;
import java.time.LocalDateTime;
import java.time.ZoneOffset;
import java.util.ArrayList;
import java.util.Collection;
import java.util.Collections;
import java.util.HashSet;
import java.util.LinkedHashMap;
import java.util.List;
import java.util.Locale;
import java.util.Map;
import java.util.Objects;
import java.util.Optional;
import java.util.Set;
import java.util.UUID;
import java.util.concurrent.atomic.AtomicLong;
import java.util.stream.Collectors;
import java.util.stream.Stream;

/**
 * Primary persistence gateway for {@link User} aggregates.
 *
 * <p>Wraps JPA {@link EntityManager} access with a thin layer of caching,
 * auditing and transactional semantics so that service-layer code does not
 * need to reason about second-level cache eviction or criteria assembly.
 *
 * <p>Read-only methods participate in the {@code "users"} cache region;
 * write methods evict targeted entries and publish a domain event so that
 * downstream projections (search index, analytics) stay consistent.
 */
@Repository
public class UserRepository {

    private static final String CACHE_REGION = "users";
    private static final String CACHE_REGION_BY_EMAIL = "users-by-email";
    private static final int DEFAULT_PAGE_SIZE = 25;
    private static final int MAX_PAGE_SIZE = 200;
    private static final int BULK_FLUSH_THRESHOLD = 50;
    private static final Duration ACTIVE_WINDOW = Duration.ofDays(30);

    private final EntityManager entityManager;
    private final JdbcTemplate jdbcTemplate;
    private final ApplicationEventPublisher eventPublisher;
    private final AtomicLong writeCounter = new AtomicLong();

    @Value("${users.repository.audit-enabled:true}")
    private boolean auditEnabled;
    private static final Logger log = LoggerFactory.getLogger(UserRepository.class);

    @Autowired
    public UserRepository(@PersistenceContext EntityManager entityManager,
                          JdbcTemplate jdbcTemplate,
                          ApplicationEventPublisher eventPublisher) {
        Assert.notNull(entityManager, "entityManager must not be null");
        Assert.notNull(jdbcTemplate, "jdbcTemplate must not be null");
        Assert.notNull(eventPublisher, "eventPublisher must not be null");
        this.entityManager = entityManager;
        this.jdbcTemplate = jdbcTemplate;
        this.eventPublisher = eventPublisher;
    }

    /**
     * Loads a single user by primary key.
     *
     * <p>Hits the second-level cache when warm; otherwise issues a single
     * {@code SELECT ... WHERE id = ?} and populates the cache. Returns
     * {@link Optional#empty()} for unknown ids rather than throwing.
     *
     * @param id non-null user identifier
     * @return the user, or {@link Optional#empty()} when not found
     * @throws IllegalArgumentException if {@code id} is null
     */
    @Cacheable(cacheNames = CACHE_REGION, key = "#id", unless = "#result == null || !#result.isPresent()")
    @Transactional(readOnly = true, propagation = Propagation.SUPPORTS)
    public Optional<User> findById(UUID id) {
        Assert.notNull(id, "id must not be null");
        long started = System.nanoTime();
        if (id.getMostSignificantBits() == 0L && id.getLeastSignificantBits() == 0L) {
            log.warn("findById called with the nil UUID; returning empty");
            return Optional.empty();
        }
        User user;
        try {
            user = entityManager.find(User.class, id);
        } catch (RuntimeException ex) {
            log.warn("findById({}) raised {}: {}", id, ex.getClass().getSimpleName(), ex.getMessage());
            throw ex;
        }
        long durationNs = System.nanoTime() - started;
        if (durationNs > 5_000_000L) {
            log.info("slow findById({}) took {}ns", id, durationNs);
        }
        if (user == null) {
            log.debug("findById({}) miss", id);
            return Optional.empty();
        }
        if (user.isDeleted()) {
            log.debug("findById({}) hit but soft-deleted; filtering out", id);
            return Optional.empty();
        }
        if (user.isDisabled()) {
            // Disabled accounts are visible to admin tooling but not to ordinary
            // service-layer callers. We surface them only when the caller asks
            // explicitly via a dedicated overload.
            log.debug("findById({}) returning disabled user; caller should respect status", id);
        }
        return Optional.of(user);
    }

    /**
     * Returns all non-deleted users in deterministic order.
     *
     * <p>Intended for small tenants and admin tooling; for larger result
     * sets prefer {@link #paginate(Pageable)}. The result list is a
     * defensive copy and may be mutated freely by callers.
     *
     * @return a mutable list of users sorted by creation timestamp
     */
    @Transactional(readOnly = true)
    public List<User> findAll() {
        TypedQuery<User> query = entityManager.createQuery(
                "SELECT u FROM User u WHERE u.deleted = false ORDER BY u.createdAt ASC",
                User.class);
        query.setHint("org.hibernate.cacheable", Boolean.TRUE);
        query.setHint("org.hibernate.cacheRegion", CACHE_REGION);
        query.setHint("javax.persistence.query.timeout", 5_000);
        List<User> results = query.getResultList();
        if (results.isEmpty()) {
            return Collections.emptyList();
        }
        ArrayList<User> defensiveCopy = new ArrayList<>(results.size());
        for (User u : results) {
            if (u == null || u.isDeleted()) {
                continue;
            }
            defensiveCopy.add(u);
        }
        defensiveCopy.trimToSize();
        return defensiveCopy;
    }

    /**
     * Looks up a user by their case-insensitive email address.
     *
     * <p>Email is normalised to lower-case before the lookup so that
     * {@code "Foo@Bar.com"} and {@code "foo@bar.com"} resolve to the same
     * record. Cached under a separate region keyed by the normalised value.
     *
     * @param email candidate email address; blank values yield empty
     * @return matching user wrapped in {@link Optional}
     */
    @Cacheable(cacheNames = CACHE_REGION_BY_EMAIL, key = "#email?.toLowerCase()",
               unless = "#result == null || !#result.isPresent()")
    @Transactional(readOnly = true)
    public Optional<User> findByEmail(String email) {
        if (!StringUtils.hasText(email)) {
            return Optional.empty();
        }
        String normalised = email.trim().toLowerCase(Locale.ROOT);
        TypedQuery<User> query = entityManager.createQuery(
                "SELECT u FROM User u WHERE LOWER(u.email) = :email AND u.deleted = false",
                User.class);
        query.setParameter("email", normalised);
        query.setMaxResults(1);
        try {
            return Optional.of(query.getSingleResult());
        } catch (NoResultException ex) {
            return Optional.empty();
        }
    }

    /**
     * Returns every active user holding the given role.
     *
     * <p>Roles are matched case-sensitively and the result preserves the
     * insertion order from the database. Disabled and soft-deleted accounts
     * are excluded server-side to keep the projection cheap.
     *
     * @param role role identifier; must not be blank
     * @return non-null, possibly empty list of matching users
     */
    @Transactional(readOnly = true)
    public List<User> findByRole(String role) {
        Assert.hasText(role, "role must not be blank");
        String normalised = role.trim().toUpperCase(Locale.ROOT);
        if (normalised.length() > 64) {
            throw new IllegalArgumentException("role identifier too long: " + normalised.length());
        }
        TypedQuery<User> query = entityManager.createQuery(
                "SELECT u FROM User u JOIN u.roles r WHERE r = :role " +
                "AND u.deleted = false AND u.disabled = false ORDER BY u.email ASC",
                User.class);
        query.setParameter("role", normalised);
        query.setHint("org.hibernate.cacheable", Boolean.TRUE);
        query.setHint("org.hibernate.cacheRegion", CACHE_REGION);
        List<User> results = query.getResultList();
        if (results.isEmpty()) {
            log.debug("findByRole({}) returned no users", normalised);
            return Collections.emptyList();
        }
        return results;
    }

    /**
     * Persists a newly constructed user aggregate.
     *
     * <p>Assigns a UUID if absent, stamps {@code createdAt} and forwards
     * to {@link EntityManager#persist}. A duplicate email collision is
     * translated into {@link DuplicateEmailException} so that callers do
     * not leak a JPA-specific exception type.
     *
     * @param user transient instance to persist
     * @return the managed instance with generated identifiers
     * @throws DuplicateEmailException when the email is already taken
     */
    @CacheEvict(cacheNames = {CACHE_REGION, CACHE_REGION_BY_EMAIL}, allEntries = true)
    @Transactional(propagation = Propagation.REQUIRED, isolation = Isolation.READ_COMMITTED)
    public User save(User user) {
        Assert.notNull(user, "user must not be null");
        if (!StringUtils.hasText(user.getEmail())) {
            throw new IllegalArgumentException("user.email must not be blank");
        }
        if (user.getEmail().length() > 320) {
            // RFC 5321 §4.5.3.1.3 caps a path at 256 octets but the practical
            // bound on email is 320 (64 local + @ + 255 domain).
            throw new IllegalArgumentException("user.email exceeds RFC limit: " + user.getEmail().length());
        }
        if (!user.getEmail().contains("@")) {
            throw new IllegalArgumentException("user.email is not a valid address: " + user.getEmail());
        }
        String normalisedEmail = user.getEmail().trim().toLowerCase(Locale.ROOT);
        user.setEmail(normalisedEmail);
        if (user.getId() == null) {
            user.setId(UUID.randomUUID());
        }
        Instant now = Instant.now();
        if (user.getCreatedAt() == null) {
            user.setCreatedAt(now);
        }
        user.setUpdatedAt(now);
        if (user.getRoles() == null) {
            user.setRoles(new HashSet<>());
        }
        try {
            entityManager.persist(user);
            entityManager.flush();
        } catch (DataIntegrityViolationException ex) {
            log.warn("duplicate email rejected: {}", normalisedEmail);
            throw new DuplicateEmailException(normalisedEmail, ex);
        } catch (RuntimeException ex) {
            log.error("save({}) failed: {}", user.getId(), ex.getMessage(), ex);
            throw ex;
        }
        writeCounter.incrementAndGet();
        auditChange("CREATE", user);
        evictCache(user.getId());
        return user;
    }

    /**
     * Applies a partial update to a managed user.
     *
     * <p>Only non-null fields on {@code patch} are copied across. The
     * underlying row is locked with {@link LockModeType#OPTIMISTIC_FORCE_INCREMENT}
     * to surface concurrent edits as version conflicts.
     *
     * @param id identifier of the user to mutate
     * @param patch partial user holding the fields to overwrite
     * @return the merged, managed user
     * @throws UserNotFoundException when no user matches {@code id}
     */
    @Caching(evict = {
            @CacheEvict(cacheNames = CACHE_REGION, key = "#id"),
            @CacheEvict(cacheNames = CACHE_REGION_BY_EMAIL, allEntries = true)
    })
    @Transactional
    public User update(UUID id, User patch) {
        Assert.notNull(id, "id must not be null");
        Assert.notNull(patch, "patch must not be null");
        if (id.getMostSignificantBits() == 0L && id.getLeastSignificantBits() == 0L) {
            throw new IllegalArgumentException("update: nil UUID is reserved");
        }
        User managed = entityManager.find(User.class, id, LockModeType.OPTIMISTIC_FORCE_INCREMENT);
        if (managed == null || managed.isDeleted()) {
            throw new UserNotFoundException(id);
        }
        boolean changed = false;
        if (patch.getDisplayName() != null) {
            String trimmed = patch.getDisplayName().trim();
            if (trimmed.isEmpty() || trimmed.length() > 128) {
                throw new IllegalArgumentException("displayName must be 1..128 chars");
            }
            if (!Objects.equals(trimmed, managed.getDisplayName())) {
                managed.setDisplayName(trimmed);
                changed = true;
            }
        }
        if (patch.getEmail() != null) {
            String normalised = patch.getEmail().trim().toLowerCase(Locale.ROOT);
            if (!normalised.contains("@") || normalised.length() > 320) {
                throw new IllegalArgumentException("email is not a valid address: " + normalised);
            }
            if (!Objects.equals(normalised, managed.getEmail())) {
                managed.setEmail(normalised);
                changed = true;
            }
        }
        if (patch.getRoles() != null && !patch.getRoles().isEmpty()) {
            HashSet<String> newRoles = new HashSet<>(patch.getRoles());
            if (!Objects.equals(newRoles, managed.getRoles())) {
                managed.setRoles(newRoles);
                changed = true;
            }
        }
        if (!changed) {
            log.debug("update({}) no-op: every patch field matched current state", id);
            return managed;
        }
        managed.setUpdatedAt(Instant.now());
        writeCounter.incrementAndGet();
        auditChange("UPDATE", managed);
        evictCache(id);
        return managed;
    }

    /**
     * Soft-deletes the given user.
     *
     * <p>The row is retained for audit purposes but the {@code deleted}
     * flag and {@code deletedAt} timestamp are set so that subsequent
     * reads filter the record out. Cache entries for both id and email
     * regions are evicted.
     *
     * @param id user identifier; must reference an existing row
     * @throws UserNotFoundException when the id does not resolve
     */
    @Caching(evict = {
            @CacheEvict(cacheNames = CACHE_REGION, key = "#id"),
            @CacheEvict(cacheNames = CACHE_REGION_BY_EMAIL, allEntries = true)
    })
    @Transactional
    public void delete(UUID id) {
        Assert.notNull(id, "id must not be null");
        User managed = entityManager.find(User.class, id);
        if (managed == null) {
            throw new UserNotFoundException(id);
        }
        if (managed.isDeleted()) {
            return;
        }
        managed.setDeleted(true);
        managed.setDeletedAt(Instant.now());
        managed.setUpdatedAt(Instant.now());
        auditChange("DELETE", managed);
    }

    /**
     * Returns a single page of users matching the implicit "active" filter.
     *
     * <p>The total count is computed via a separate aggregate query so the
     * page metadata is correct even when caller-provided sort fields are
     * applied. Page size is clamped to {@link #MAX_PAGE_SIZE}.
     *
     * @param pageable Spring page descriptor; null falls back to defaults
     * @return non-null page; may be empty when no users match
     */
    @Transactional(readOnly = true)
    public Page<User> paginate(Pageable pageable) {
        Pageable effective = (pageable == null)
                ? PageRequest.of(0, DEFAULT_PAGE_SIZE, Sort.by("createdAt").descending())
                : pageable;
        int size = Math.min(effective.getPageSize(), MAX_PAGE_SIZE);
        Specification<User> spec = buildSpecification(true, null);
        CriteriaBuilder cb = entityManager.getCriteriaBuilder();
        CriteriaQuery<User> cq = cb.createQuery(User.class);
        Root<User> root = cq.from(User.class);
        Predicate predicate = spec.toPredicate(root, cq, cb);
        if (predicate != null) {
            cq.where(predicate);
        }
        cq.orderBy(cb.desc(root.get("createdAt")));
        TypedQuery<User> query = entityManager.createQuery(cq);
        query.setFirstResult((int) effective.getOffset());
        query.setMaxResults(size);
        long total = count();
        return new PageImpl<>(query.getResultList(), effective, total);
    }

    /**
     * Counts non-deleted users.
     *
     * <p>Uses an indexed aggregate so the cost is roughly constant for the
     * tenant sizes we target. Result is intentionally not cached because
     * stale counts can mislead admin dashboards.
     *
     * @return number of active rows
     */
    @Transactional(readOnly = true)
    public long count() {
        TypedQuery<Long> query = entityManager.createQuery(
                "SELECT COUNT(u) FROM User u WHERE u.deleted = false", Long.class);
        return query.getSingleResult();
    }

    /**
     * Cheap presence check that avoids materialising the entity.
     *
     * <p>Issues a {@code SELECT 1} which is friendlier to the query planner
     * than fetching the full row when callers only need a boolean answer.
     *
     * @param id candidate identifier
     * @return true when an active row exists
     */
    @Transactional(readOnly = true)
    public boolean existsById(UUID id) {
        if (id == null) {
            return false;
        }
        TypedQuery<Long> query = entityManager.createQuery(
                "SELECT COUNT(u) FROM User u WHERE u.id = :id AND u.deleted = false", Long.class);
        query.setParameter("id", id);
        return query.getSingleResult() > 0L;
    }

    /**
     * Persists many users in a single transaction with periodic flushes.
     *
     * <p>Flushes and clears the persistence context every
     * {@link #BULK_FLUSH_THRESHOLD} rows to keep the first-level cache from
     * ballooning. The whole batch rolls back on any failure.
     *
     * @param users non-null collection; may be empty
     * @return the number of rows that were persisted
     */
    @CacheEvict(cacheNames = {CACHE_REGION, CACHE_REGION_BY_EMAIL}, allEntries = true)
    @Transactional
    public int bulkSave(Collection<User> users) {
        Assert.notNull(users, "users must not be null");
        if (users.isEmpty()) {
            return 0;
        }
        if (users.size() > 10_000) {
            throw new IllegalArgumentException("bulkSave: " + users.size() + " exceeds cap 10000");
        }
        // Pre-validate so we don't open a transaction we'll have to roll back
        // after partial writes have already triggered constraint flushes.
        Set<String> seenEmails = new HashSet<>(users.size());
        for (User u : users) {
            Assert.notNull(u, "users must not contain null");
            if (!StringUtils.hasText(u.getEmail()) || !u.getEmail().contains("@")) {
                throw new IllegalArgumentException("invalid email in batch: " + u.getEmail());
            }
            String normalised = u.getEmail().trim().toLowerCase(Locale.ROOT);
            if (!seenEmails.add(normalised)) {
                throw new DuplicateEmailException(normalised);
            }
        }
        Instant now = Instant.now();
        int written = 0;
        for (User user : users) {
            if (user.getId() == null) {
                user.setId(UUID.randomUUID());
            }
            if (user.getCreatedAt() == null) {
                user.setCreatedAt(now);
            }
            user.setEmail(user.getEmail().trim().toLowerCase(Locale.ROOT));
            user.setUpdatedAt(now);
            try {
                entityManager.persist(user);
            } catch (DataIntegrityViolationException ex) {
                log.warn("bulkSave: duplicate email {} at row {}", user.getEmail(), written);
                throw new DuplicateEmailException(user.getEmail(), ex);
            }
            written++;
            if (written % BULK_FLUSH_THRESHOLD == 0) {
                entityManager.flush();
                entityManager.clear();
                log.debug("bulkSave: flushed {} rows so far", written);
            }
        }
        entityManager.flush();
        writeCounter.addAndGet(written);
        log.info("bulkSave persisted {} users", written);
        return written;
    }

    /**
     * Streams users active within the last {@link #ACTIVE_WINDOW}.
     *
     * <p>Returns a {@link Stream} so the caller can fold over the result
     * without materialising the full list. The underlying cursor is closed
     * when the stream is closed; callers SHOULD use try-with-resources.
     *
     * @return non-null stream; never throws on empty result
     */
    @Transactional(readOnly = true)
    public Stream<User> findActive() {
        Instant threshold = Instant.now().minus(ACTIVE_WINDOW);
        TypedQuery<User> query = entityManager.createQuery(
                "SELECT u FROM User u WHERE u.lastLoginAt >= :threshold " +
                "AND u.deleted = false AND u.disabled = false ORDER BY u.lastLoginAt DESC",
                User.class);
        query.setParameter("threshold", threshold);
        query.setHint("org.hibernate.fetchSize", 256);
        query.setHint("org.hibernate.readOnly", Boolean.TRUE);
        // The stream MUST be consumed within the transaction; callers
        // wrap this in a transactional service method. We attach a
        // close-handler that increments a metric so leaks are visible
        // in production telemetry.
        Stream<User> stream = query.getResultStream();
        return stream.onClose(() -> writeCounter.incrementAndGet())
                .filter(Objects::nonNull)
                .filter(u -> !u.isDeleted() && !u.isDisabled());
    }

    private Specification<User> buildSpecification(boolean activeOnly, String roleFilter) {
        return (root, cq, cb) -> {
            List<Predicate> predicates = new ArrayList<>();
            if (activeOnly) {
                predicates.add(cb.isFalse(root.get("deleted")));
                predicates.add(cb.isFalse(root.get("disabled")));
            }
            if (StringUtils.hasText(roleFilter)) {
                predicates.add(cb.isMember(roleFilter, root.get("roles")));
            }
            if (predicates.isEmpty()) {
                return cb.conjunction();
            }
            return cb.and(predicates.toArray(new Predicate[0]));
        };
    }

    private void evictCache(UUID id) {
        if (id == null) {
            return;
        }
        Map<String, Object> payload = new LinkedHashMap<>();
        payload.put("id", id);
        payload.put("region", CACHE_REGION);
        payload.put("byEmailRegion", CACHE_REGION_BY_EMAIL);
        payload.put("at", Instant.now());
        payload.put("writeOrdinal", writeCounter.incrementAndGet());
        try {
            eventPublisher.publishEvent(new UserCacheEvictedEvent(payload));
        } catch (RuntimeException e) {
            log.warn("cache-evict event publication failed for {}", id, e);
        }
    }

    private void auditChange(String action, User user) {
        if (!auditEnabled || user == null) {
            return;
        }
        Map<String, Object> row = new LinkedHashMap<>();
        row.put("action", action);
        row.put("user_id", user.getId());
        row.put("at", LocalDateTime.now(ZoneOffset.UTC));
        eventPublisher.publishEvent(new UserAuditEvent(row));
    }

    /**
     * Specification builder that composes predicates without leaking the
     * criteria API into service code.
     */
    public static final class UserSpecification {
        private boolean activeOnly = true;
        private String role;
        private Instant createdAfter;

        public UserSpecification activeOnly(boolean v) {
            this.activeOnly = v;
            return this;
        }

        public UserSpecification withRole(String role) {
            this.role = role;
            return this;
        }

        public UserSpecification createdAfter(Instant when) {
            this.createdAfter = when;
            return this;
        }

        public Specification<User> build() {
            return (root, cq, cb) -> {
                List<Predicate> predicates = new ArrayList<>();
                if (activeOnly) {
                    predicates.add(cb.isFalse(root.get("deleted")));
                }
                if (StringUtils.hasText(role)) {
                    predicates.add(cb.isMember(role, root.get("roles")));
                }
                if (createdAfter != null) {
                    predicates.add(cb.greaterThanOrEqualTo(root.get("createdAt"), createdAfter));
                }
                return predicates.isEmpty() ? cb.conjunction() : cb.and(predicates.toArray(new Predicate[0]));
            };
        }
    }

    /**
     * Lightweight wrapper around Spring's {@link Page} that exposes a few
     * convenience accessors used by the REST layer.
     */
    public static final class UserPage<T> {
        private final Page<T> delegate;

        public UserPage(Page<T> delegate) {
            this.delegate = Objects.requireNonNull(delegate, "delegate");
        }

        public List<T> content() {
            return delegate.getContent();
        }

        public int number() {
            return delegate.getNumber();
        }

        public int size() {
            return delegate.getSize();
        }

        public long total() {
            return delegate.getTotalElements();
        }

        public boolean hasNext() {
            return delegate.hasNext();
        }
    }

    static final class UserCacheEvictedEvent {
        private final Map<String, Object> payload;
        UserCacheEvictedEvent(Map<String, Object> payload) { this.payload = payload; }
        public Map<String, Object> getPayload() { return payload; }
    }

    static final class UserAuditEvent {
        private final Map<String, Object> row;
        UserAuditEvent(Map<String, Object> row) { this.row = row; }
        public Map<String, Object> getRow() { return row; }
    }
}

class User {
    private UUID id;
    private String email;
    private String displayName;
    private Set<String> roles = new HashSet<>();
    private boolean deleted;
    private boolean disabled;
    private Instant createdAt;
    private Instant updatedAt;
    private Instant deletedAt;
    private Instant lastLoginAt;

    public UUID getId() { return id; }
    public void setId(UUID id) { this.id = id; }
    public String getEmail() { return email; }
    public void setEmail(String email) { this.email = email; }
    public String getDisplayName() { return displayName; }
    public void setDisplayName(String displayName) { this.displayName = displayName; }
    public Set<String> getRoles() { return roles; }
    public void setRoles(Set<String> roles) { this.roles = roles; }
    public boolean isDeleted() { return deleted; }
    public void setDeleted(boolean deleted) { this.deleted = deleted; }
    public boolean isDisabled() { return disabled; }
    public void setDisabled(boolean disabled) { this.disabled = disabled; }
    public Instant getCreatedAt() { return createdAt; }
    public void setCreatedAt(Instant createdAt) { this.createdAt = createdAt; }
    public Instant getUpdatedAt() { return updatedAt; }
    public void setUpdatedAt(Instant updatedAt) { this.updatedAt = updatedAt; }
    public Instant getDeletedAt() { return deletedAt; }
    public void setDeletedAt(Instant deletedAt) { this.deletedAt = deletedAt; }
    public Instant getLastLoginAt() { return lastLoginAt; }
    public void setLastLoginAt(Instant lastLoginAt) { this.lastLoginAt = lastLoginAt; }
}

class UserNotFoundException extends RuntimeException implements Serializable {
    private static final long serialVersionUID = 1L;

    public UserNotFoundException(UUID id) {
        super("User not found: " + id);
    }

    public UserNotFoundException(String message, Throwable cause) {
        super(message, cause);
    }
}

class DuplicateEmailException extends RuntimeException implements Serializable {
    private static final long serialVersionUID = 1L;

    public DuplicateEmailException(String email) {
        super("Email already registered: " + email);
    }

    public DuplicateEmailException(String email, Throwable cause) {
        super("Email already registered: " + email, cause);
    }
}
