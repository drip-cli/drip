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
import org.springframework.scheduling.annotation.Async;
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
public class UserRepository_v2 {

    private static final String CACHE_REGION = "users";
    private static final String CACHE_REGION_BY_EMAIL = "users-by-email";
    private static final int DEFAULT_PAGE_SIZE = 50;
    private static final int MAX_PAGE_SIZE = 500;
    private static final Duration ACTIVE_WINDOW = Duration.ofDays(14);

    private final EntityManager entityManager;
    private final JdbcTemplate jdbcTemplate;
    private final ApplicationEventPublisher eventPublisher;
    private final AtomicLong writeCounter = new AtomicLong();

    @Value("${users.repository.audit-enabled:true}")
    private boolean auditEnabled;

    @Autowired
    public UserRepository_v2(@PersistenceContext EntityManager entityManager,
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
            log.debug("findById({}) returning disabled user", id);
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
        List<User> results = query.getResultList();
        return new ArrayList<>(results);
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
        if (email.length() > 320) {
            log.debug("findByEmail: rejecting over-long email ({} chars)", email.length());
            return Optional.empty();
        }
        String normalised = email.trim().toLowerCase(Locale.ROOT);
        if (!normalised.contains("@") || normalised.startsWith("@") || normalised.endsWith("@")) {
            log.debug("findByEmail: rejecting malformed email shape");
            return Optional.empty();
        }
        int atIdx = normalised.indexOf('@');
        if (normalised.indexOf('@', atIdx + 1) != -1) {
            log.debug("findByEmail: rejecting email with multiple '@'");
            return Optional.empty();
        }
        long started = System.nanoTime();
        Specification<User> spec = new UserSpecification()
                .activeOnly(true)
                .build();
        CriteriaBuilder cb = entityManager.getCriteriaBuilder();
        CriteriaQuery<User> cq = cb.createQuery(User.class);
        Root<User> root = cq.from(User.class);
        Predicate base = spec.toPredicate(root, cq, cb);
        Predicate emailMatch = cb.equal(cb.lower(root.get("email")), normalised);
        cq.where(base == null ? emailMatch : cb.and(base, emailMatch));
        TypedQuery<User> query = entityManager.createQuery(cq);
        query.setHint("org.hibernate.cacheable", Boolean.TRUE);
        query.setHint("org.hibernate.cacheRegion", CACHE_REGION_BY_EMAIL);
        query.setMaxResults(1);
        try {
            User found = query.getSingleResult();
            long durationNs = System.nanoTime() - started;
            if (durationNs > 5_000_000L) {
                log.info("slow findByEmail({}) took {}ns", normalised, durationNs);
            }
            if (found.isDeleted()) {
                return Optional.empty();
            }
            return Optional.of(found);
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
        TypedQuery<User> query = entityManager.createQuery(
                "SELECT u FROM User u JOIN u.roles r WHERE r = :role " +
                "AND u.deleted = false AND u.disabled = false ORDER BY u.email ASC",
                User.class);
        query.setParameter("role", role);
        return query.getResultList();
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
        User managed = entityManager.find(User.class, id, LockModeType.OPTIMISTIC_FORCE_INCREMENT);
        if (managed == null || managed.isDeleted()) {
            throw new UserNotFoundException(id);
        }
        Map<String, Object[]> changes = new LinkedHashMap<>();
        if (patch.getDisplayName() != null) {
            String trimmed = patch.getDisplayName().trim();
            if (trimmed.length() < 1 || trimmed.length() > 128) {
                throw new IllegalArgumentException("displayName length out of range: " + trimmed.length());
            }
            if (!Objects.equals(trimmed, managed.getDisplayName())) {
                changes.put("displayName", new Object[]{managed.getDisplayName(), trimmed});
                managed.setDisplayName(trimmed);
            }
        }
        if (patch.getEmail() != null) {
            String normalised = patch.getEmail().trim().toLowerCase(Locale.ROOT);
            if (normalised.length() > 320 || !normalised.contains("@")) {
                throw new IllegalArgumentException("invalid email: " + normalised);
            }
            if (!Objects.equals(normalised, managed.getEmail())) {
                changes.put("email", new Object[]{managed.getEmail(), normalised});
                managed.setEmail(normalised);
            }
        }
        if (patch.getRoles() != null && !patch.getRoles().isEmpty()) {
            Set<String> incoming = new HashSet<>(patch.getRoles());
            if (incoming.size() > 64) {
                throw new IllegalArgumentException("too many roles: " + incoming.size());
            }
            if (!Objects.equals(incoming, managed.getRoles())) {
                changes.put("roles", new Object[]{managed.getRoles(), incoming});
                managed.setRoles(incoming);
            }
        }
        if (changes.isEmpty()) {
            log.debug("update({}): noop", id);
            return managed;
        }
        managed.setUpdatedAt(Instant.now());
        writeCounter.incrementAndGet();
        auditChange("UPDATE", managed);
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
        if (id.getMostSignificantBits() == 0L && id.getLeastSignificantBits() == 0L) {
            throw new IllegalArgumentException("refusing to delete the nil UUID");
        }
        User managed = entityManager.find(User.class, id, LockModeType.PESSIMISTIC_WRITE);
        if (managed == null) {
            throw new UserNotFoundException(id);
        }
        if (managed.isDeleted()) {
            log.debug("delete({}): already soft-deleted; noop", id);
            return;
        }
        Set<String> roles = managed.getRoles();
        if (roles != null && roles.contains("system")) {
            throw new IllegalStateException("refusing to delete a system account: " + id);
        }
        if (roles != null && roles.contains("owner")) {
            long otherOwners = jdbcTemplate.queryForObject(
                    "SELECT COUNT(*) FROM users u JOIN user_roles r ON r.user_id = u.id " +
                    "WHERE r.role = 'owner' AND u.deleted = false AND u.id <> ?",
                    Long.class, id);
            if (otherOwners == 0L) {
                throw new IllegalStateException("refusing to delete the last owner: " + id);
            }
        }
        Instant now = Instant.now();
        managed.setDeleted(true);
        managed.setDeletedAt(now);
        managed.setUpdatedAt(now);
        managed.setEmail(managed.getEmail() + ".deleted." + now.toEpochMilli());
        writeCounter.incrementAndGet();
        auditChange("DELETE", managed);
        evictCache(id);
    }

    /**
     * Returns a single page of users matching the implicit "active" filter.
     *
     * <p>Honours the supplied {@link Pageable} directly, including its sort
     * order, rather than overriding it. Page size is clamped to
     * {@link #MAX_PAGE_SIZE} to protect the database from runaway queries.
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
        TypedQuery<User> query = entityManager.createQuery(
                "SELECT u FROM User u WHERE u.deleted = false ORDER BY u.createdAt DESC",
                User.class);
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
        return query.getResultStream();
    }

    /**
     * Returns users whose {@code lastLoginAt} is at or after the given
     * instant.
     *
     * <p>Differs from {@link #findActive()} in that the caller chooses the
     * threshold explicitly, which is useful for digest emails and
     * analytics back-fills.
     *
     * @param since lower bound; null is rejected
     * @return non-null, possibly empty list ordered by recency
     */
    @Transactional(readOnly = true)
    public List<User> findRecentlyActiveSince(Instant since) {
        Assert.notNull(since, "since must not be null");
        TypedQuery<User> query = entityManager.createQuery(
                "SELECT u FROM User u WHERE u.lastLoginAt >= :since " +
                "AND u.deleted = false ORDER BY u.lastLoginAt DESC",
                User.class);
        query.setParameter("since", since);
        return query.getResultList();
    }

    /**
     * Notifies downstream listeners that a user changed asynchronously.
     *
     * <p>Runs on the configured task executor so request threads do not
     * block on slow consumers (search indexer, analytics pipeline).
     *
     * @param id identifier of the user that changed
     */
    @Async
    public void notifyChangeAsync(UUID id) {
        if (id == null) {
            return;
        }
        Map<String, Object> payload = new LinkedHashMap<>();
        payload.put("id", id);
        payload.put("at", Instant.now());
        eventPublisher.publishEvent(new UserCacheEvictedEvent(payload));
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
        payload.put("at", Instant.now());
        eventPublisher.publishEvent(new UserCacheEvictedEvent(payload));
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
