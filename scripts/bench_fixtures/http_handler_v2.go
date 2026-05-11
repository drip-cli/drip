package handlers

import (
	"context"
	"database/sql"
	"encoding/json"
	"errors"
	"fmt"
	"io"
	"log/slog"
	"net"
	"net/http"
	"strconv"
	"strings"
	"time"

	"github.com/google/uuid"
	"golang.org/x/time/rate"
)

// Default tunables for the HTTP layer. These are deliberately conservative
// so that an unconfigured Server still behaves reasonably in production.
const (
	defaultReadTimeout  = 15 * time.Second
	defaultWriteTimeout = 30 * time.Second
	defaultMaxBodyBytes = 2 << 20 // 2 MiB
	defaultRateLimitRPS = 50
	defaultPageSize     = 50
	maxPageSize         = 200
	maxBulkDeleteIDs    = 100
)

// Sentinel errors returned by the handlers and the underlying stores. They
// are wrapped (with %w) at call sites so that callers can use errors.Is to
// distinguish between not-found, unauthenticated and validation failures.
var (
	ErrNotFound     = errors.New("handlers: resource not found")
	ErrUnauthorized = errors.New("handlers: unauthorized")
	ErrInvalidInput = errors.New("handlers: invalid input")
)

// SessionStore abstracts the persistence layer used by AuthMiddleware to
// resolve a bearer token into a user identity. All methods are
// context-aware so that callers can enforce request-scoped deadlines.
type SessionStore interface {
	// Get returns the user id associated with the given session token, or
	// ErrNotFound if the token is unknown or has expired.
	Get(ctx context.Context, token string) (userID string, err error)

	// Set stores a session token for the given user with the provided TTL.
	// Implementations are expected to refresh existing tokens.
	Set(ctx context.Context, token, userID string, ttl time.Duration) error

	// Delete removes a session token. Deleting a missing token must not
	// return an error so that logout is idempotent.
	Delete(ctx context.Context, token string) error
}

// Config holds the tunable parameters for a Server. Zero values are
// replaced with sensible defaults inside NewServer.
type Config struct {
	ReadTimeout    time.Duration
	WriteTimeout   time.Duration
	MaxBodyBytes   int64
	RateLimitRPS   int
	EnableCORS     bool
	AllowedOrigins []string
	RequireMFA     bool
}

// Server is the entry point for the user-facing HTTP API. It owns its
// dependencies (database, logger, rate limiter, session store) so that
// individual handlers remain pure functions over (*Server, http.Request).
type Server struct {
	db           *sql.DB
	logger       *slog.Logger
	limiter      *rate.Limiter
	sessionStore SessionStore
	config       Config
}

// CreateUserRequest is the JSON payload accepted by CreateUser. The
// validate tags are consumed by an external validator middleware which is
// not shown here.
type CreateUserRequest struct {
	Email    string `json:"email" validate:"required,email"`
	Password string `json:"password" validate:"required,min=12,max=128"`
	Name     string `json:"name" validate:"required,min=1,max=120"`
}

// UpdateUserRequest is the JSON payload accepted by UpdateUser. All
// fields are optional pointers so that PATCH semantics can distinguish
// "field not present" from "field set to its zero value".
type UpdateUserRequest struct {
	Email *string `json:"email,omitempty" validate:"omitempty,email"`
	Name  *string `json:"name,omitempty" validate:"omitempty,min=1,max=120"`
}

// UserResponse is the canonical JSON shape returned for a single user. It
// intentionally omits the password hash and any internal flags.
type UserResponse struct {
	ID        string    `json:"id"`
	Email     string    `json:"email"`
	Name      string    `json:"name"`
	CreatedAt time.Time `json:"created_at"`
	UpdatedAt time.Time `json:"updated_at"`
}

// ListUsersResponse is the paginated envelope returned by ListUsers. The
// NextCursor field is empty when there are no further pages.
type ListUsersResponse struct {
	Users      []UserResponse `json:"users"`
	NextCursor string         `json:"next_cursor,omitempty"`
	Total      int            `json:"total"`
}

// BulkDeleteRequest is the admin-only payload accepted by
// BulkDeleteUsers. The number of ids is capped server-side to keep the
// transaction short.
type BulkDeleteRequest struct {
	IDs []string `json:"ids" validate:"required,min=1,max=100,dive,uuid"`
}

// ErrorResponse is the uniform error envelope. CorrelationID lets clients
// quote a single identifier when reporting bugs.
type ErrorResponse struct {
	Code          string `json:"code"`
	Message       string `json:"message"`
	CorrelationID string `json:"correlation_id"`
}

// NewServer constructs a Server, replacing zero-valued Config fields with
// sensible defaults. The session store and database must be non-nil.
func NewServer(db *sql.DB, logger *slog.Logger, store SessionStore, cfg Config) (*Server, error) {
	if db == nil {
		return nil, fmt.Errorf("handlers: db is required")
	}
	if store == nil {
		return nil, fmt.Errorf("handlers: session store is required")
	}
	if logger == nil {
		logger = slog.Default()
	}
	if cfg.ReadTimeout == 0 {
		cfg.ReadTimeout = defaultReadTimeout
	}
	if cfg.WriteTimeout == 0 {
		cfg.WriteTimeout = defaultWriteTimeout
	}
	if cfg.MaxBodyBytes == 0 {
		cfg.MaxBodyBytes = defaultMaxBodyBytes
	}
	if cfg.RateLimitRPS == 0 {
		cfg.RateLimitRPS = defaultRateLimitRPS
	}
	limiter := rate.NewLimiter(rate.Limit(cfg.RateLimitRPS), cfg.RateLimitRPS*2)
	return &Server{
		db:           db,
		logger:       logger,
		limiter:      limiter,
		sessionStore: store,
		config:       cfg,
	}, nil
}

// CreateUser handles POST /users. It validates the request, hashes the
// password (delegated to the data layer), writes a row to the database
// and returns the freshly-created UserResponse with status 201.
func (s *Server) CreateUser(w http.ResponseWriter, r *http.Request) {
	ctx := r.Context()
	var req CreateUserRequest
	if err := readJSON(r, &req, s.config.MaxBodyBytes); err != nil {
		s.writeError(w, r, http.StatusBadRequest, "invalid_body", err)
		return
	}
	if err := validateEmail(req.Email); err != nil {
		s.writeError(w, r, http.StatusBadRequest, "invalid_email", err)
		return
	}
	id := uuid.NewString()
	now := time.Now().UTC()
	const q = `INSERT INTO users (id, email, name, password_hash, created_at, updated_at) VALUES ($1, $2, $3, crypt($4, gen_salt('bf')), $5, $5)`
	if _, err := s.db.ExecContext(ctx, q, id, req.Email, req.Name, req.Password, now); err != nil {
		s.logger.ErrorContext(ctx, "create user failed", "err", err, "cid", correlationID(ctx))
		s.writeError(w, r, http.StatusInternalServerError, "db_error", fmt.Errorf("insert user: %w", err))
		return
	}
	resp := UserResponse{ID: id, Email: req.Email, Name: req.Name, CreatedAt: now, UpdatedAt: now}
	writeJSON(w, http.StatusCreated, resp)
}

// GetUser handles GET /users/{id}. It loads a single row by primary key
// and returns 404 when the row is absent. Database failures are logged
// with the correlation id so that they can be cross-referenced.
func (s *Server) GetUser(w http.ResponseWriter, r *http.Request) {
	ctx := r.Context()
	id := r.PathValue("id")
	if id == "" {
		s.writeError(w, r, http.StatusBadRequest, "missing_id", ErrInvalidInput)
		return
	}
	var u UserResponse
	const q = `SELECT id, email, name, created_at, updated_at FROM users WHERE id = $1`
	row := s.db.QueryRowContext(ctx, q, id)
	if err := row.Scan(&u.ID, &u.Email, &u.Name, &u.CreatedAt, &u.UpdatedAt); err != nil {
		if errors.Is(err, sql.ErrNoRows) {
			s.writeError(w, r, http.StatusNotFound, "not_found", ErrNotFound)
			return
		}
		s.logger.ErrorContext(ctx, "get user failed", "err", err, "cid", correlationID(ctx))
		s.writeError(w, r, http.StatusInternalServerError, "db_error", err)
		return
	}
	writeJSON(w, http.StatusOK, u)
}

// UpdateUser handles PATCH /users/{id}. Only the fields that are present
// in the request body are updated; missing fields keep their previous
// value. The query is built dynamically to avoid clobbering columns.
func (s *Server) UpdateUser(w http.ResponseWriter, r *http.Request) {
	ctx := r.Context()
	id := r.PathValue("id")
	if id == "" {
		s.writeError(w, r, http.StatusBadRequest, "missing_id", ErrInvalidInput)
		return
	}
	var req UpdateUserRequest
	if err := readJSON(r, &req, s.config.MaxBodyBytes); err != nil {
		s.writeError(w, r, http.StatusBadRequest, "invalid_body", err)
		return
	}
	sets := make([]string, 0, 3)
	args := make([]any, 0, 4)
	if req.Email != nil {
		if err := validateEmail(*req.Email); err != nil {
			s.writeError(w, r, http.StatusBadRequest, "invalid_email", err)
			return
		}
		args = append(args, *req.Email)
		sets = append(sets, fmt.Sprintf("email = $%d", len(args)))
	}
	if req.Name != nil {
		args = append(args, *req.Name)
		sets = append(sets, fmt.Sprintf("name = $%d", len(args)))
	}
	if len(sets) == 0 {
		s.writeError(w, r, http.StatusBadRequest, "empty_patch", ErrInvalidInput)
		return
	}
	now := time.Now().UTC()
	args = append(args, now)
	sets = append(sets, fmt.Sprintf("updated_at = $%d", len(args)))
	args = append(args, id)
	q := fmt.Sprintf(`UPDATE users SET %s WHERE id = $%d`, strings.Join(sets, ", "), len(args))
	res, err := s.db.ExecContext(ctx, q, args...)
	if err != nil {
		s.logger.ErrorContext(ctx, "update user failed", "err", err, "cid", correlationID(ctx))
		s.writeError(w, r, http.StatusInternalServerError, "db_error", err)
		return
	}
	if n, _ := res.RowsAffected(); n == 0 {
		s.writeError(w, r, http.StatusNotFound, "not_found", ErrNotFound)
		return
	}
	w.WriteHeader(http.StatusNoContent)
}

// DeleteUser handles DELETE /users/{id}. It is idempotent: deleting an
// already-absent user returns 204 rather than 404, so that retries from
// flaky clients don't generate spurious errors.
func (s *Server) DeleteUser(w http.ResponseWriter, r *http.Request) {
	ctx := r.Context()
	id := r.PathValue("id")
	if id == "" {
		s.writeError(w, r, http.StatusBadRequest, "missing_id", ErrInvalidInput)
		return
	}
	const q = `DELETE FROM users WHERE id = $1`
	if _, err := s.db.ExecContext(ctx, q, id); err != nil {
		s.logger.ErrorContext(ctx, "delete user failed", "err", err, "cid", correlationID(ctx))
		s.writeError(w, r, http.StatusInternalServerError, "db_error", err)
		return
	}
	w.WriteHeader(http.StatusNoContent)
}

// BulkDeleteUsers handles POST /admin/users:bulk-delete. It accepts up
// to maxBulkDeleteIDs ids in a single transaction and returns the
// number of rows that were actually removed.
func (s *Server) BulkDeleteUsers(w http.ResponseWriter, r *http.Request) {
	ctx := r.Context()
	requesterID, _ := ctx.Value(ctxKeyUserID{}).(string)
	if requesterID == "" {
		s.writeError(w, r, http.StatusUnauthorized, "missing_principal", ErrUnauthorized)
		return
	}
	var allowed bool
	if err := s.db.QueryRowContext(ctx,
		`SELECT EXISTS (SELECT 1 FROM user_roles WHERE user_id = $1 AND role = 'admin')`,
		requesterID).Scan(&allowed); err != nil {
		s.logger.ErrorContext(ctx, "authz lookup failed", "err", err, "cid", correlationID(ctx))
		s.writeError(w, r, http.StatusInternalServerError, "db_error", err)
		return
	}
	if !allowed {
		s.writeError(w, r, http.StatusForbidden, "forbidden", ErrUnauthorized)
		return
	}
	var req BulkDeleteRequest
	if err := readJSON(r, &req, s.config.MaxBodyBytes); err != nil {
		s.writeError(w, r, http.StatusBadRequest, "invalid_body", err)
		return
	}
	if len(req.IDs) == 0 || len(req.IDs) > maxBulkDeleteIDs {
		s.writeError(w, r, http.StatusBadRequest, "id_count", ErrInvalidInput)
		return
	}
	seen := make(map[string]struct{}, len(req.IDs))
	for _, id := range req.IDs {
		if _, err := uuid.Parse(id); err != nil {
			s.writeError(w, r, http.StatusBadRequest, "malformed_id", ErrInvalidInput)
			return
		}
		if id == requesterID {
			s.writeError(w, r, http.StatusBadRequest, "self_delete_forbidden", ErrInvalidInput)
			return
		}
		seen[id] = struct{}{}
	}
	if len(seen) != len(req.IDs) {
		s.writeError(w, r, http.StatusBadRequest, "duplicate_ids", ErrInvalidInput)
		return
	}
	tx, err := s.db.BeginTx(ctx, nil)
	if err != nil {
		s.writeError(w, r, http.StatusInternalServerError, "db_error", fmt.Errorf("begin: %w", err))
		return
	}
	defer func() { _ = tx.Rollback() }()
	const q = `DELETE FROM users WHERE id = ANY($1) AND id NOT IN (SELECT user_id FROM user_roles WHERE role = 'system')`
	res, err := tx.ExecContext(ctx, q, req.IDs)
	if err != nil {
		s.logger.ErrorContext(ctx, "bulk delete failed", "err", err, "cid", correlationID(ctx))
		s.writeError(w, r, http.StatusInternalServerError, "db_error", err)
		return
	}
	if err := tx.Commit(); err != nil {
		s.writeError(w, r, http.StatusInternalServerError, "db_error", fmt.Errorf("commit: %w", err))
		return
	}
	n, _ := res.RowsAffected()
	s.logger.InfoContext(ctx, "bulk delete completed", "requested", len(req.IDs), "deleted", n, "cid", correlationID(ctx))
	writeJSON(w, http.StatusOK, map[string]int64{"deleted": n})
}

// ListUsers handles GET /users. It supports cursor-based pagination via
// ?cursor=, a configurable ?limit= capped at maxPageSize, and an
// optional ?since=RFC3339 filter that returns only users created after
// the supplied timestamp.
func (s *Server) ListUsers(w http.ResponseWriter, r *http.Request) {
	ctx := r.Context()
	q := r.URL.Query()
	limit := defaultPageSize
	if raw := q.Get("limit"); raw != "" {
		n, err := strconv.Atoi(raw)
		if err != nil || n <= 0 {
			s.writeError(w, r, http.StatusBadRequest, "invalid_limit", ErrInvalidInput)
			return
		}
		if n > maxPageSize {
			n = maxPageSize
		}
		limit = n
	}
	var since time.Time
	if raw := q.Get("since"); raw != "" {
		t, err := time.Parse(time.RFC3339, raw)
		if err != nil {
			s.writeError(w, r, http.StatusBadRequest, "invalid_since", ErrInvalidInput)
			return
		}
		since = t.UTC()
	}
	cursor := q.Get("cursor")
	rows, err := s.queryUsers(ctx, cursor, limit, since)
	if err != nil {
		s.logger.ErrorContext(ctx, "list users failed", "err", err, "cid", correlationID(ctx))
		s.writeError(w, r, http.StatusInternalServerError, "db_error", err)
		return
	}
	resp := ListUsersResponse{Users: rows, Total: len(rows)}
	if len(rows) == limit {
		resp.NextCursor = rows[len(rows)-1].ID
	}
	writeJSON(w, http.StatusOK, resp)
}

// queryUsers is a small helper around the paginated SELECT used by
// ListUsers. It is split out to keep the handler readable and to make
// the optional ?since= filter easy to compose.
func (s *Server) queryUsers(ctx context.Context, cursor string, limit int, since time.Time) ([]UserResponse, error) {
	const base = `SELECT id, email, name, created_at, updated_at FROM users`
	conds := make([]string, 0, 2)
	args := make([]any, 0, 3)
	if cursor != "" {
		args = append(args, cursor)
		conds = append(conds, fmt.Sprintf("id < $%d", len(args)))
	}
	if !since.IsZero() {
		args = append(args, since)
		conds = append(conds, fmt.Sprintf("created_at >= $%d", len(args)))
	}
	q := base
	if len(conds) > 0 {
		q += " WHERE " + strings.Join(conds, " AND ")
	}
	args = append(args, limit)
	q += fmt.Sprintf(" ORDER BY created_at DESC LIMIT $%d", len(args))
	rows, err := s.db.QueryContext(ctx, q, args...)
	if err != nil {
		return nil, fmt.Errorf("query users: %w", err)
	}
	defer rows.Close()
	out := make([]UserResponse, 0, limit)
	for rows.Next() {
		var u UserResponse
		if err := rows.Scan(&u.ID, &u.Email, &u.Name, &u.CreatedAt, &u.UpdatedAt); err != nil {
			return nil, fmt.Errorf("scan user: %w", err)
		}
		out = append(out, u)
	}
	return out, rows.Err()
}

// AuthMiddleware extracts the bearer token from the Authorization header
// and resolves it via the session store. The user id is injected into
// the request context so that downstream handlers can read it.
func (s *Server) AuthMiddleware(next http.Handler) http.Handler {
	return http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		ctx := r.Context()
		auth := r.Header.Get("Authorization")
		token := strings.TrimPrefix(auth, "Bearer ")
		if token == "" || token == auth {
			s.writeError(w, r, http.StatusUnauthorized, "missing_token", ErrUnauthorized)
			return
		}
		userID, err := s.sessionStore.Get(ctx, token)
		if err != nil {
			s.logger.InfoContext(ctx, "auth lookup failed", "err", err, "cid", correlationID(ctx))
			s.writeError(w, r, http.StatusUnauthorized, "invalid_token", ErrUnauthorized)
			return
		}
		if s.config.RequireMFA && r.Header.Get("X-MFA-Verified") != "1" {
			s.writeError(w, r, http.StatusUnauthorized, "mfa_required", ErrUnauthorized)
			return
		}
		ctx = context.WithValue(ctx, ctxKeyUserID{}, userID)
		next.ServeHTTP(w, r.WithContext(ctx))
	})
}

// ErrorHandler is a recover middleware. Panics are converted into 500
// responses and the stack frame is logged at error level so that the
// process keeps serving subsequent requests.
func (s *Server) ErrorHandler(next http.Handler) http.Handler {
	return http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		defer func() {
			if rec := recover(); rec != nil {
				s.logger.ErrorContext(r.Context(), "panic in handler",
					"panic", rec,
					"path", r.URL.Path,
					"cid", correlationID(r.Context()),
				)
				s.writeError(w, r, http.StatusInternalServerError, "internal", fmt.Errorf("panic: %v", rec))
			}
		}()
		next.ServeHTTP(w, r)
	})
}

// writeError is a small helper that converts an internal error into the
// uniform ErrorResponse envelope. It must never panic, even on a broken
// ResponseWriter, so it ignores the encoder error.
func (s *Server) writeError(w http.ResponseWriter, r *http.Request, status int, code string, err error) {
	cid := correlationID(r.Context())
	resp := ErrorResponse{Code: code, Message: err.Error(), CorrelationID: cid}
	writeJSON(w, status, resp)
}

// ctxKeyUserID is the context key under which the authenticated user id
// is stored. It is unexported so that no external package can collide
// with it accidentally.
type ctxKeyUserID struct{}

// ctxKeyCorrelationID is the context key under which the per-request
// correlation id is stored. See correlationID for retrieval semantics.
type ctxKeyCorrelationID struct{}

// writeJSON serialises body as JSON and writes it to w with the given
// status code. The Content-Type header is always set, even for empty
// bodies, to match the rest of the API.
func writeJSON(w http.ResponseWriter, status int, body any) {
	w.Header().Set("Content-Type", "application/json; charset=utf-8")
	w.WriteHeader(status)
	if body == nil {
		return
	}
	enc := json.NewEncoder(w)
	enc.SetEscapeHTML(false)
	if err := enc.Encode(body); err != nil {
		// We have already committed the status code; the connection is
		// effectively poisoned, so there is nothing to do but bail.
		return
	}
}

// readJSON decodes the request body into dst, enforcing maxBytes as a
// hard upper bound. Unknown fields are rejected so that typos in the
// client are surfaced loudly rather than silently ignored.
func readJSON(r *http.Request, dst any, maxBytes int64) error {
	if maxBytes <= 0 {
		maxBytes = defaultMaxBodyBytes
	}
	r.Body = http.MaxBytesReader(nil, r.Body, maxBytes)
	dec := json.NewDecoder(r.Body)
	dec.DisallowUnknownFields()
	if err := dec.Decode(dst); err != nil {
		if errors.Is(err, io.EOF) {
			return fmt.Errorf("%w: empty body", ErrInvalidInput)
		}
		return fmt.Errorf("%w: %v", ErrInvalidInput, err)
	}
	if dec.More() {
		return fmt.Errorf("%w: trailing data after JSON value", ErrInvalidInput)
	}
	return nil
}

// validateEmail performs a deliberately permissive sanity check. Real
// email validation is hard; we leave the strict checks to the database
// constraint and to the eventual confirmation email round-trip.
func validateEmail(s string) error {
	if len(s) < 3 || len(s) > 254 {
		return fmt.Errorf("%w: email length out of range", ErrInvalidInput)
	}
	at := strings.IndexByte(s, '@')
	if at <= 0 || at == len(s)-1 {
		return fmt.Errorf("%w: missing local or domain part", ErrInvalidInput)
	}
	if strings.ContainsAny(s, " \t\r\n") {
		return fmt.Errorf("%w: whitespace not allowed", ErrInvalidInput)
	}
	return nil
}

// correlationID returns the per-request correlation id, generating one
// lazily when none is present. The generated value is not stored back
// into the context: that is the responsibility of the middleware which
// also writes the X-Correlation-ID response header.
func correlationID(ctx context.Context) string {
	if v, ok := ctx.Value(ctxKeyCorrelationID{}).(string); ok && v != "" {
		return v
	}
	return uuid.NewString()
}

// clientIP extracts the best-effort client IP from the request. It
// honours the X-Forwarded-For header when present, taking the leftmost
// entry, and falls back to RemoteAddr.
func clientIP(r *http.Request) string {
	if xff := r.Header.Get("X-Forwarded-For"); xff != "" {
		if comma := strings.IndexByte(xff, ','); comma > 0 {
			return strings.TrimSpace(xff[:comma])
		}
		return strings.TrimSpace(xff)
	}
	host, _, err := net.SplitHostPort(r.RemoteAddr)
	if err != nil {
		return r.RemoteAddr
	}
	return host
}

// ensure rate import remains referenced in v2 even though
// RateLimitMiddleware was moved to a separate package.
var _ = rate.NewLimiter
