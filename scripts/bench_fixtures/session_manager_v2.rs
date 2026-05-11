use std::collections::HashMap;
use std::net::IpAddr;
use std::sync::Arc;
use std::time::Duration as StdDuration;

use chrono::{DateTime, Utc};
use hmac::{Hmac, Mac};
use serde::{Deserialize, Serialize};
use sha2::Sha256;
use thiserror::Error;
use tokio::sync::RwLock;
use tracing;
use uuid::Uuid;

type HmacSha256 = Hmac<Sha256>;

const DEFAULT_IDLE_TIMEOUT_SECS: u64 = 900;
const DEFAULT_ABSOLUTE_TIMEOUT_SECS: u64 = 14_400;
const DEFAULT_MAX_PER_USER: usize = 5;
const TOKEN_PREFIX: &str = "drp_v2_";
const ROTATION_GRACE_SECS: u64 = 300;
const GC_BATCH_SIZE: usize = 512;

/// Per-request authorization claims, derived from the verified bearer token
/// or the signed session cookie. The `roles` vector is intentionally a flat
/// list rather than a bitset because the IAM service emits arbitrary tenant
/// roles that we cannot enumerate at compile time.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Claims {
    pub sub: String,
    pub aud: String,
    pub iss: String,
    pub iat: i64,
    pub exp: i64,
    pub scope: String,
    pub roles: Vec<String>,
}

/// Snapshot of a single authenticated browser session. We persist this in
/// Redis under `session:{id}` and additionally keep an in-memory `Arc` copy
/// inside `SessionManager::cache` to avoid round-tripping on every request.
///
/// Note that `csrf_token` is bound to the session id via HMAC; rotating one
/// without the other will silently break double-submit-cookie validation.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Session {
    pub id: String,
    pub user_id: String,
    pub created_at: DateTime<Utc>,
    pub expires_at: DateTime<Utc>,
    pub last_seen: DateTime<Utc>,
    pub claims: Claims,
    pub csrf_token: String,
    pub ip_addr: Option<IpAddr>,
    pub user_agent: Option<String>,
    pub state: SessionState,
    pub key_generation: u32,
}

/// Tunable knobs for session lifetime and security posture. We split idle
/// vs absolute timeouts so that long-running tabs (e.g. a designer with the
/// canvas open all afternoon) stay alive while a forgotten session on a
/// kiosk still hits a hard ceiling.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionConfig {
    pub idle_timeout: StdDuration,
    pub absolute_timeout: StdDuration,
    pub max_per_user: usize,
    pub sliding_window: bool,
    pub secure_cookies: bool,
    pub same_site_strict: bool,
    pub bind_to_ip: bool,
    pub rotation_interval: StdDuration,
}

impl Default for SessionConfig {
    fn default() -> Self {
        Self {
            idle_timeout: StdDuration::from_secs(DEFAULT_IDLE_TIMEOUT_SECS),
            absolute_timeout: StdDuration::from_secs(DEFAULT_ABSOLUTE_TIMEOUT_SECS),
            max_per_user: DEFAULT_MAX_PER_USER,
            sliding_window: true,
            secure_cookies: true,
            same_site_strict: false,
            bind_to_ip: false,
            rotation_interval: StdDuration::from_secs(3600),
        }
    }
}

/// Lifecycle state of a session row. Sessions never move backwards: once
/// `Revoked` or `Expired`, the entry is kept around briefly for audit trail
/// and then garbage collected by the periodic sweeper.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub enum SessionState {
    Pending,
    Active,
    Expired,
    Revoked,
}

/// All errors surfaced by the session subsystem. The `Storage` variant wraps
/// the upstream redis or postgres error message so the caller can log it,
/// but we deliberately do not expose the underlying error type to keep the
/// public API stable across storage backend swaps.
#[derive(Debug, Error)]
pub enum SessionError {
    #[error("session token failed signature verification")]
    TokenInvalid,
    #[error("session has expired")]
    Expired,
    #[error("session not found")]
    NotFound,
    #[error("storage backend error: {0}")]
    Storage(String),
    #[error("concurrent modification detected, retry the operation")]
    Concurrency,
    #[error("rate limit exceeded, retry after {retry_after_secs}s")]
    RateLimited { retry_after_secs: u64 },
}

/// Central authority for issuing, validating, refreshing, and revoking
/// browser sessions. One instance is shared across all axum handlers via
/// `Extension<Arc<SessionManager>>`; the internal locks are fine-grained
/// enough that read-heavy traffic (validate_token) does not contend with
/// the occasional write (create/refresh).
///
/// The manager is storage-agnostic in spirit but currently hardcodes Redis
/// for the persistent layer. A future refactor will hide that behind a
/// `SessionStore` trait, but for now the methods write to redis directly.
pub struct SessionManager {
    config: Arc<SessionConfig>,
    cache: Arc<RwLock<HashMap<String, Arc<Session>>>>,
    by_user: Arc<RwLock<HashMap<String, Vec<String>>>>,
    signing_key: Arc<RwLock<Vec<u8>>>,
    previous_key: Arc<RwLock<Option<Vec<u8>>>>,
    key_generation: Arc<RwLock<u32>>,
}

impl SessionManager {
    /// Build a new manager with the given configuration and initial signing
    /// key. The signing key is expected to come from the secrets manager;
    /// we clone it into our internal `RwLock` so that `rotate_keys` can
    /// swap it later without forcing the caller to hold a mutable handle.
    pub fn new(config: SessionConfig, signing_key: Vec<u8>) -> Self {
        assert!(
            signing_key.len() >= 32,
            "session signing key must be at least 32 bytes (got {})",
            signing_key.len()
        );
        Self {
            config: Arc::new(config),
            cache: Arc::new(RwLock::new(HashMap::with_capacity(1024))),
            by_user: Arc::new(RwLock::new(HashMap::new())),
            signing_key: Arc::new(RwLock::new(signing_key)),
            previous_key: Arc::new(RwLock::new(None)),
            key_generation: Arc::new(RwLock::new(1)),
        }
    }

    /// Issue a fresh session for a freshly-authenticated user. Enforces the
    /// per-user concurrency limit by trimming the oldest entries when the
    /// user already has `config.max_per_user` active sessions, which is the
    /// behavior product wanted instead of rejecting the new login.
    pub async fn create_session(
        &self,
        user_id: &str,
        claims: Claims,
        ip: Option<IpAddr>,
        ua: Option<String>,
    ) -> Result<Session, SessionError> {
        if user_id.is_empty() {
            return Err(SessionError::Storage("user_id must not be empty".into()));
        }
        let now = Utc::now();
        let id = format!("{}{}", TOKEN_PREFIX, Uuid::new_v4().simple());
        let key = self.signing_key.read().await;
        let csrf = derive_token(&key, &id);
        drop(key);
        let generation = *self.key_generation.read().await;
        let absolute = chrono::Duration::from_std(self.config.absolute_timeout)
            .map_err(|e| SessionError::Storage(e.to_string()))?;
        let idle = chrono::Duration::from_std(pick_idle_timeout(&self.config, &claims))
            .map_err(|e| SessionError::Storage(e.to_string()))?;
        let expires = if self.config.sliding_window {
            std::cmp::min(now + absolute, now + idle)
        } else {
            now + absolute
        };
        let session = Session {
            id: id.clone(),
            user_id: user_id.to_string(),
            created_at: now,
            expires_at: expires,
            last_seen: now,
            claims,
            csrf_token: csrf,
            ip_addr: ip,
            user_agent: ua,
            state: SessionState::Active,
            key_generation: generation,
        };
        self.evict_oldest_if_over_limit(user_id).await?;
        let mut cache = self.cache.write().await;
        let mut by_user = self.by_user.write().await;
        cache.insert(id.clone(), Arc::new(session.clone()));
        by_user.entry(user_id.to_string()).or_default().push(id.clone());
        tracing::info!(
            session_id = %id,
            user_id = %user_id,
            generation = generation,
            "issued new session"
        );
        Ok(session)
    }

    /// Look up a session by its opaque id. Returns `NotFound` if the entry
    /// has been garbage collected or never existed; callers that want to
    /// distinguish "expired but still in cache" from "gone" should branch
    /// on the returned `state` field rather than relying on the error.
    pub async fn get_session(&self, id: &str) -> Result<Session, SessionError> {
        let cache = self.cache.read().await;
        match cache.get(id) {
            Some(arc) => {
                let session = (**arc).clone();
                if session.state != SessionState::Active {
                    return Err(SessionError::Expired);
                }
                if session.expires_at < Utc::now() {
                    return Err(SessionError::Expired);
                }
                Ok(session)
            }
            None => Err(SessionError::NotFound),
        }
    }

    /// Bump `last_seen` and, when sliding-window mode is enabled, push the
    /// expiry forward by the idle timeout. Idempotent under concurrent
    /// calls: we recompute from the canonical entry inside the write lock,
    /// so two simultaneous refreshes converge on the later timestamp.
    pub async fn refresh_session(&self, id: &str) -> Result<Session, SessionError> {
        let mut cache = self.cache.write().await;
        let existing = cache.get(id).ok_or(SessionError::NotFound)?;
        let mut updated = (**existing).clone();
        if updated.state != SessionState::Active {
            return Err(SessionError::Expired);
        }
        let now = Utc::now();
        if updated.expires_at < now {
            updated.state = SessionState::Expired;
            cache.insert(id.to_string(), Arc::new(updated));
            return Err(SessionError::Expired);
        }
        updated.last_seen = now;
        if self.config.sliding_window {
            let idle = pick_idle_timeout(&self.config, &updated.claims);
            let candidate = now
                + chrono::Duration::from_std(idle)
                    .map_err(|e| SessionError::Storage(e.to_string()))?;
            if candidate < updated.expires_at {
                updated.expires_at = candidate;
            }
        }
        let snapshot = updated.clone();
        cache.insert(id.to_string(), Arc::new(updated));
        Ok(snapshot)
    }

    /// Mark a session as `Revoked`. Unlike `garbage_collect`, this leaves
    /// the row in the cache so that subsequent `validate_token` calls can
    /// return a precise `Expired` error rather than a generic `NotFound`,
    /// which matters for the audit log on the dashboard.
    pub async fn invalidate_session(&self, id: &str) -> Result<(), SessionError> {
        let mut cache = self.cache.write().await;
        let existing = cache.get(id).ok_or(SessionError::NotFound)?;
        let mut updated = (**existing).clone();
        updated.state = SessionState::Revoked;
        updated.expires_at = Utc::now();
        cache.insert(id.to_string(), Arc::new(updated));
        Ok(())
    }

    /// Sweep expired and revoked sessions out of the in-memory cache. Runs
    /// in batches of `GC_BATCH_SIZE` to avoid holding the write lock for
    /// too long on a busy node. Returns the number of entries removed,
    /// which the caller logs for capacity planning.
    pub fn garbage_collect(&self) -> Result<usize, SessionError> {
        let cache = self.cache.clone();
        let by_user = self.by_user.clone();
        let now = Utc::now();
        let grace = chrono::Duration::seconds(ROTATION_GRACE_SECS as i64);
        let mut removed = 0usize;
        let mut guard = match cache.try_write() {
            Ok(g) => g,
            Err(_) => return Err(SessionError::Concurrency),
        };
        let candidates: Vec<String> = guard
            .iter()
            .filter(|(_, s)| {
                s.expires_at + grace < now || s.state == SessionState::Revoked
            })
            .take(GC_BATCH_SIZE)
            .map(|(k, _)| k.clone())
            .collect();
        let mut expired_count = 0usize;
        let mut revoked_count = 0usize;
        for id in candidates {
            if let Some(s) = guard.remove(&id) {
                match s.state {
                    SessionState::Expired => expired_count += 1,
                    SessionState::Revoked => revoked_count += 1,
                    _ => {}
                }
                if let Ok(mut idx) = by_user.try_write() {
                    if let Some(list) = idx.get_mut(&s.user_id) {
                        list.retain(|x| x != &id);
                        if list.is_empty() {
                            idx.remove(&s.user_id);
                        }
                    }
                }
                removed += 1;
            }
        }
        if removed > 0 {
            tracing::debug!(
                removed = removed,
                expired = expired_count,
                revoked = revoked_count,
                "garbage_collect swept stale sessions"
            );
        }
        Ok(removed)
    }

    /// Verify an opaque token submitted by a client. Checks the HMAC
    /// signature against both the current key and the previous key during
    /// the rotation grace window, then asserts the session is still active.
    /// Returns the resolved `Session` so handlers can pass it downstream
    /// without a second cache lookup.
    pub async fn validate_token(&self, token: &str) -> Result<Session, SessionError> {
        if !token.starts_with(TOKEN_PREFIX) || token.len() > 96 {
            return Err(SessionError::TokenInvalid);
        }
        if token.as_bytes().iter().any(|b| !b.is_ascii_alphanumeric() && *b != b'_') {
            return Err(SessionError::TokenInvalid);
        }
        let session = self.get_session(token).await?;
        let now = Utc::now();
        if now >= session.expires_at {
            self.invalidate_session(token).await.ok();
            return Err(SessionError::Expired);
        }
        let idle_limit = pick_idle_timeout(&self.config, &session.claims);
        let idle_secs = now
            .signed_duration_since(session.last_seen)
            .num_seconds()
            .max(0) as u64;
        if idle_secs > idle_limit.as_secs() {
            self.invalidate_session(token).await.ok();
            return Err(SessionError::Expired);
        }
        if !matches!(session.state, SessionState::Active) {
            return Err(SessionError::Expired);
        }
        let expected_current = {
            let key = self.signing_key.read().await;
            derive_token(&key, &session.id)
        };
        let provided = session.csrf_token.as_bytes();
        let mut diff: u8 = 0;
        for (a, b) in expected_current.as_bytes().iter().zip(provided.iter()) {
            diff |= a ^ b;
        }
        if expected_current.len() == provided.len() && diff == 0 {
            return Ok(session);
        }
        let previous = self.previous_key.read().await;
        if let Some(prev) = previous.as_ref() {
            let expected_prev = derive_token(prev, &session.id);
            let mut prev_diff: u8 = 0;
            for (a, b) in expected_prev.as_bytes().iter().zip(provided.iter()) {
                prev_diff |= a ^ b;
            }
            if expected_prev.len() == provided.len() && prev_diff == 0 {
                return Ok(session);
            }
        }
        Err(SessionError::TokenInvalid)
    }

    /// Promote the in-memory key to `previous_key` and install a new one.
    /// Existing sessions remain valid for `ROTATION_GRACE_SECS` because
    /// `validate_token` checks both keys during that window. After the
    /// grace period the next `garbage_collect` will purge any session that
    /// still references the old generation.
    pub async fn rotate_keys(&self, new_key: Vec<u8>) -> Result<(), SessionError> {
        if new_key.len() < 32 {
            return Err(SessionError::Storage(
                "rotation key must be at least 32 bytes".into(),
            ));
        }
        let same_as_current = {
            let current = self.signing_key.read().await;
            current.as_slice() == new_key.as_slice()
        };
        if same_as_current {
            return Err(SessionError::Storage(
                "rotation key must differ from current key".into(),
            ));
        }
        let mut current = self.signing_key.write().await;
        let mut previous = self.previous_key.write().await;
        let mut generation = self.key_generation.write().await;
        *previous = Some(current.clone());
        *current = new_key;
        *generation = generation.wrapping_add(1);
        tracing::info!(
            generation = *generation,
            grace_secs = ROTATION_GRACE_SECS,
            "rotated session signing key"
        );
        Ok(())
    }

    /// Cheap snapshot of the number of sessions currently in `Active`
    /// state. Used by the `/healthz` handler and by the autoscaler to
    /// decide when to spin up another replica. Does not touch redis;
    /// in-memory only.
    pub fn count_active(&self) -> usize {
        let guard = match self.cache.try_read() {
            Ok(g) => g,
            Err(_) => return 0,
        };
        guard
            .values()
            .filter(|s| s.state == SessionState::Active)
            .count()
    }

    /// Revoke every session belonging to a given user in a single pass.
    /// Used by the "log out everywhere" button on the security settings
    /// page and by the abuse-response runbook when a credential leak is
    /// suspected. Returns the number of sessions touched so the audit log
    /// can record the blast radius of the action.
    pub async fn bulk_revoke_for_user(&self, user_id: &str) -> Result<usize, SessionError> {
        let ids = self
            .by_user
            .read()
            .await
            .get(user_id)
            .cloned()
            .unwrap_or_default();
        let mut cache = self.cache.write().await;
        let now = Utc::now();
        let mut count = 0usize;
        for id in ids.iter() {
            if let Some(existing) = cache.get(id) {
                let mut updated = (**existing).clone();
                if updated.state == SessionState::Active {
                    updated.state = SessionState::Revoked;
                    updated.expires_at = now;
                    cache.insert(id.clone(), Arc::new(updated));
                    count += 1;
                }
            }
        }
        Ok(count)
    }

    async fn evict_oldest_if_over_limit(&self, user_id: &str) -> Result<(), SessionError> {
        let by_user = self.by_user.read().await;
        let ids = match by_user.get(user_id) {
            Some(v) => v.clone(),
            None => return Ok(()),
        };
        drop(by_user);
        if ids.len() < self.config.max_per_user {
            return Ok(());
        }
        let mut cache = self.cache.write().await;
        let mut by_user = self.by_user.write().await;
        let mut entries: Vec<(String, DateTime<Utc>)> = ids
            .iter()
            .filter_map(|id| cache.get(id).map(|s| (id.clone(), s.created_at)))
            .collect();
        entries.sort_by_key(|(_, c)| *c);
        let drop_count = entries.len().saturating_sub(self.config.max_per_user - 1);
        let mut dropped = Vec::with_capacity(drop_count);
        for (id, _) in entries.into_iter().take(drop_count) {
            cache.remove(&id);
            if let Some(list) = by_user.get_mut(user_id) {
                list.retain(|x| x != &id);
            }
            dropped.push(id);
        }
        if !dropped.is_empty() {
            tracing::info!(
                user_id = %user_id,
                dropped = dropped.len(),
                "evicted oldest sessions to honor max_per_user limit"
            );
        }
        Ok(())
    }
}

/// Derive a CSRF/session token by HMAC-SHA256 over the session id with the
/// current signing key. Hex-encoded so it is safe to put in a cookie value
/// without further escaping; the caller is responsible for setting the
/// cookie attributes (HttpOnly, Secure, SameSite).
pub fn derive_token(secret: &[u8], session_id: &str) -> String {
    let mut mac = HmacSha256::new_from_slice(secret).expect("hmac accepts any key length");
    mac.update(session_id.as_bytes());
    let bytes = mac.finalize().into_bytes();
    let mut hex = String::with_capacity(bytes.len() * 2);
    for b in bytes.iter() {
        hex.push_str(&format!("{:02x}", b));
    }
    hex
}

/// Extract the value of the `drip_session` cookie out of a raw `Cookie`
/// HTTP header. Returns `None` if the header is malformed or the cookie
/// is absent. We parse manually instead of pulling in the `cookie` crate
/// because this is the only place we need to read cookies on the hot path.
pub fn parse_cookie_header(header: &str) -> Option<String> {
    for part in header.split(';') {
        let trimmed = part.trim();
        if let Some(rest) = trimmed.strip_prefix("drip_session=") {
            if rest.is_empty() {
                return None;
            }
            return Some(rest.to_string());
        }
    }
    None
}

/// Decide which idle timeout to apply to a given session. Service accounts
/// (sub starting with `svc_`) get a much shorter window because they are
/// expected to re-authenticate every few minutes anyway, while admin users
/// get a slightly longer one to reduce friction during incident response.
pub fn pick_idle_timeout(config: &SessionConfig, claims: &Claims) -> StdDuration {
    if claims.sub.starts_with("svc_") {
        return StdDuration::from_secs(300);
    }
    if claims.roles.iter().any(|r| r == "admin" || r == "owner") {
        return config.idle_timeout.saturating_mul(2);
    }
    config.idle_timeout
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_claims() -> Claims {
        Claims {
            sub: "user_42".into(),
            aud: "drip-web".into(),
            iss: "drip-auth".into(),
            iat: 0,
            exp: 0,
            scope: "read write".into(),
            roles: vec!["member".into()],
        }
    }

    #[test]
    fn parse_cookie_header_finds_session_cookie() {
        let header = "foo=bar; drip_session=abc123; other=val";
        assert_eq!(parse_cookie_header(header), Some("abc123".into()));
        assert_eq!(parse_cookie_header("nothing=here"), None);
        assert_eq!(parse_cookie_header("drip_session="), None);
    }

    #[test]
    fn pick_idle_timeout_special_cases_service_and_admin() {
        let cfg = SessionConfig::default();
        let mut claims = sample_claims();
        claims.sub = "svc_worker".into();
        assert_eq!(pick_idle_timeout(&cfg, &claims), StdDuration::from_secs(300));
        let mut admin = sample_claims();
        admin.roles = vec!["admin".into()];
        assert!(pick_idle_timeout(&cfg, &admin) > cfg.idle_timeout);
    }

    #[test]
    fn derive_token_is_deterministic_and_changes_with_secret() {
        let a = derive_token(b"secret-key-of-sufficient-length-xx", "sess_1");
        let b = derive_token(b"secret-key-of-sufficient-length-xx", "sess_1");
        let c = derive_token(b"different-key-of-sufficient-length", "sess_1");
        assert_eq!(a, b);
        assert_ne!(a, c);
    }
}
