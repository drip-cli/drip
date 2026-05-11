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
	defaultReadTimeout  = 10 * time.Second
	defaultWriteTimeout = 15 * time.Second
	defaultMaxBodyBytes = 1 << 20 // 1 MiB
	defaultRateLimitRPS = 50
	defaultPageSize     = 25
	maxPageSize         = 200
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
	if len(req.Password) < 12 || len(req.Password) > 128 {
		s.writeError(w, r, http.StatusBadRequest, "invalid_password", ErrInvalidInput)
		return
	}
	if strings.TrimSpace(req.Name) == "" || len(req.Name) > 120 {
		s.writeError(w, r, http.StatusBadRequest, "invalid_name", ErrInvalidInput)
		return
	}
	normalisedEmail := strings.ToLower(strings.TrimSpace(req.Email))
	var existing string
	switch err := s.db.QueryRowContext(ctx,
		`SELECT id FROM users WHERE email = $1`, normalisedEmail).Scan(&existing); {
	case err == nil:
		s.writeError(w, r, http.StatusConflict, "email_taken", fmt.Errorf("email already registered"))
		return
	case errors.Is(err, sql.ErrNoRows):
		// not present, continue
	default:
		s.logger.ErrorContext(ctx, "email lookup failed", "err", err, "cid", correlationID(ctx))
		s.writeError(w, r, http.StatusInternalServerError, "db_error", err)
		return
	}
	id := uuid.NewString()
	now := time.Now().UTC()
	const q = `INSERT INTO users (id, email, name, password_hash, created_at, updated_at) VALUES ($1, $2, $3, crypt($4, gen_salt('bf')), $5, $5)`
	if _, err := s.db.ExecContext(ctx, q, id, normalisedEmail, req.Name, req.Password, now); err != nil {
		s.logger.ErrorContext(ctx, "create user failed", "err", err, "cid", correlationID(ctx))
		s.writeError(w, r, http.StatusInternalServerError, "db_error", fmt.Errorf("insert user: %w", err))
		return
	}
	s.logger.InfoContext(ctx, "user created", "id", id, "ip", clientIP(r), "cid", correlationID(ctx))
	resp := UserResponse{ID: id, Email: normalisedEmail, Name: req.Name, CreatedAt: now, UpdatedAt: now}
	w.Header().Set("Location", "/users/"+id)
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
	if _, err := uuid.Parse(id); err != nil {
		s.writeError(w, r, http.StatusBadRequest, "malformed_id", ErrInvalidInput)
		return
	}
	requesterID, _ := ctx.Value(ctxKeyUserID{}).(string)
	if requesterID == "" {
		s.writeError(w, r, http.StatusUnauthorized, "missing_principal", ErrUnauthorized)
		return
	}
	if requesterID != id {
		var allowed bool
		err := s.db.QueryRowContext(ctx,
			`SELECT EXISTS (SELECT 1 FROM user_roles WHERE user_id = $1 AND role IN ('admin','support'))`,
			requesterID).Scan(&allowed)
		if err != nil {
			s.logger.ErrorContext(ctx, "authz lookup failed", "err", err, "cid", correlationID(ctx))
			s.writeError(w, r, http.StatusInternalServerError, "db_error", err)
			return
		}
		if !allowed {
			s.writeError(w, r, http.StatusForbidden, "forbidden", ErrUnauthorized)
			return
		}
	}
	var u UserResponse
	const q = `SELECT id, email, name, created_at, updated_at FROM users WHERE id = $1 AND deleted_at IS NULL`
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
	w.Header().Set("Cache-Control", "private, max-age=30")
	writeJSON(w, http.StatusOK, u)
}

// UpdateUser handles PUT /users/{id}. The request body is treated as a
// full replacement of the mutable fields (name, email). Password updates
// flow through a dedicated endpoint that is not implemented here.
func (s *Server) UpdateUser(w http.ResponseWriter, r *http.Request) {
	ctx := r.Context()
	id := r.PathValue("id")
	if id == "" {
		s.writeError(w, r, http.StatusBadRequest, "missing_id", ErrInvalidInput)
		return
	}
	if _, err := uuid.Parse(id); err != nil {
		s.writeError(w, r, http.StatusBadRequest, "malformed_id", ErrInvalidInput)
		return
	}
	requesterID, _ := ctx.Value(ctxKeyUserID{}).(string)
	if requesterID == "" {
		s.writeError(w, r, http.StatusUnauthorized, "missing_principal", ErrUnauthorized)
		return
	}
	if requesterID != id {
		s.writeError(w, r, http.StatusForbidden, "forbidden", ErrUnauthorized)
		return
	}
	var req CreateUserRequest
	if err := readJSON(r, &req, s.config.MaxBodyBytes); err != nil {
		s.writeError(w, r, http.StatusBadRequest, "invalid_body", err)
		return
	}
	if err := validateEmail(req.Email); err != nil {
		s.writeError(w, r, http.StatusBadRequest, "invalid_email", err)
		return
	}
	if strings.TrimSpace(req.Name) == "" || len(req.Name) > 120 {
		s.writeError(w, r, http.StatusBadRequest, "invalid_name", ErrInvalidInput)
		return
	}
	normalisedEmail := strings.ToLower(strings.TrimSpace(req.Email))
	now := time.Now().UTC()
	const q = `UPDATE users SET email = $1, name = $2, updated_at = $3 WHERE id = $4 AND deleted_at IS NULL`
	res, err := s.db.ExecContext(ctx, q, normalisedEmail, req.Name, now, id)
	if err != nil {
		s.logger.ErrorContext(ctx, "update user failed", "err", err, "cid", correlationID(ctx))
		s.writeError(w, r, http.StatusInternalServerError, "db_error", err)
		return
	}
	if n, _ := res.RowsAffected(); n == 0 {
		s.writeError(w, r, http.StatusNotFound, "not_found", ErrNotFound)
		return
	}
	s.logger.InfoContext(ctx, "user updated", "id", id, "cid", correlationID(ctx))
	writeJSON(w, http.StatusOK, UserResponse{ID: id, Email: normalisedEmail, Name: req.Name, UpdatedAt: now})
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

// ListUsers handles GET /users. It supports cursor-based pagination via
// the ?cursor= query parameter and a configurable page size capped at
// maxPageSize. Results are ordered by creation time descending.
func (s *Server) ListUsers(w http.ResponseWriter, r *http.Request) {
	ctx := r.Context()
	requesterID, _ := ctx.Value(ctxKeyUserID{}).(string)
	if requesterID == "" {
		s.writeError(w, r, http.StatusUnauthorized, "missing_principal", ErrUnauthorized)
		return
	}
	var allowed bool
	if err := s.db.QueryRowContext(ctx,
		`SELECT EXISTS (SELECT 1 FROM user_roles WHERE user_id = $1 AND role IN ('admin','support'))`,
		requesterID).Scan(&allowed); err != nil {
		s.logger.ErrorContext(ctx, "authz lookup failed", "err", err, "cid", correlationID(ctx))
		s.writeError(w, r, http.StatusInternalServerError, "db_error", err)
		return
	}
	if !allowed {
		s.writeError(w, r, http.StatusForbidden, "forbidden", ErrUnauthorized)
		return
	}
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
	cursor := q.Get("cursor")
	if cursor != "" {
		if _, err := uuid.Parse(cursor); err != nil {
			s.writeError(w, r, http.StatusBadRequest, "invalid_cursor", ErrInvalidInput)
			return
		}
	}
	rows, err := s.queryUsers(ctx, cursor, limit)
	if err != nil {
		s.logger.ErrorContext(ctx, "list users failed", "err", err, "cid", correlationID(ctx))
		s.writeError(w, r, http.StatusInternalServerError, "db_error", err)
		return
	}
	resp := ListUsersResponse{Users: rows, Total: len(rows)}
	if len(rows) == limit {
		resp.NextCursor = rows[len(rows)-1].ID
	}
	w.Header().Set("Cache-Control", "private, max-age=15")
	writeJSON(w, http.StatusOK, resp)
}

// queryUsers is a small helper around the paginated SELECT used by
// ListUsers. It is split out to keep the handler readable.
func (s *Server) queryUsers(ctx context.Context, cursor string, limit int) ([]UserResponse, error) {
	const base = `SELECT id, email, name, created_at, updated_at FROM users`
	var (
		rows *sql.Rows
		err  error
	)
	if cursor == "" {
		rows, err = s.db.QueryContext(ctx, base+` ORDER BY created_at DESC LIMIT $1`, limit)
	} else {
		rows, err = s.db.QueryContext(ctx, base+` WHERE id < $1 ORDER BY created_at DESC LIMIT $2`, cursor, limit)
	}
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
		ctx = context.WithValue(ctx, ctxKeyUserID{}, userID)
		next.ServeHTTP(w, r.WithContext(ctx))
	})
}

// RateLimitMiddleware applies a token-bucket limiter shared across the
// whole server. Requests that exceed the budget receive 429 with a
// Retry-After hint derived from the limiter's reservation.
func (s *Server) RateLimitMiddleware(next http.Handler) http.Handler {
	return http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		res := s.limiter.Reserve()
		if !res.OK() {
			s.writeError(w, r, http.StatusTooManyRequests, "rate_limited", fmt.Errorf("limiter exhausted"))
			return
		}
		delay := res.Delay()
		if delay > 0 {
			res.Cancel()
			w.Header().Set("Retry-After", strconv.Itoa(int(delay.Seconds())+1))
			s.writeError(w, r, http.StatusTooManyRequests, "rate_limited", fmt.Errorf("retry after %s", delay))
			return
		}
		next.ServeHTTP(w, r)
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
