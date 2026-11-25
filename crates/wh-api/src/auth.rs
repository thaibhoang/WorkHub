//! Login for `wh serve --auth`, i.e. when WorkHub sits behind a public URL.
//!
//! Users live in the `users` table (argon2id hashes, managed with `wh user ...`).
//! Logging in creates a random session token: the browser keeps it in an
//! HttpOnly `wh_session` cookie, the database keeps only its sha256. Every
//! `/api` route except health and `/api/auth/*` requires a valid session, and
//! human commands record the logged-in user as their actor.
//!
//! Without `--auth` (the local default) every request acts as `$USER`, exactly
//! as before.

use std::{
    collections::HashMap,
    net::SocketAddr,
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

use argon2::{
    Argon2,
    password_hash::{PasswordHash, PasswordHasher, PasswordVerifier, SaltString, rand_core::OsRng},
};
use axum::{
    Extension, Json,
    extract::{ConnectInfo, Request, State},
    http::{HeaderMap, HeaderValue, Method, StatusCode, header},
    middleware::Next,
    response::{IntoResponse, Response},
};
use base64::Engine;
use rand::RngCore;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use sqlx::PgPool;
use wh_core::Actor;

use crate::{ApiError, AppState, commands::local_user};

pub const COOKIE: &str = "wh_session";
pub const SESSION_TTL: Duration = Duration::from_secs(30 * 24 * 3600);
pub const MIN_PASSWORD_LEN: usize = 12;

/// Failed logins allowed per client within [`LOCKOUT_WINDOW`].
const MAX_FAILURES: u32 = 5;
const LOCKOUT_WINDOW: Duration = Duration::from_secs(15 * 60);

#[derive(Clone, Default)]
pub struct AuthConfig {
    /// Require a login for the API (`wh serve --auth`).
    pub enabled: bool,
    /// Public origin, e.g. `https://workhub.example.com`. Mutating requests from
    /// any other `Origin` are refused, and an `https` origin makes the cookie `Secure`.
    pub public_url: Option<String>,
    /// Behind a reverse proxy on this host: take the client address from
    /// `X-Real-IP` (for login throttling) instead of the socket.
    pub trust_proxy: bool,
    failures: Arc<Mutex<HashMap<String, (u32, Instant)>>>,
}

impl AuthConfig {
    pub fn new(enabled: bool, public_url: Option<String>, trust_proxy: bool) -> Self {
        let public_url = public_url.map(|u| u.trim_end_matches('/').to_owned());
        Self {
            enabled,
            public_url,
            trust_proxy,
            failures: Default::default(),
        }
    }

    fn secure_cookie(&self) -> bool {
        self.public_url
            .as_deref()
            .is_some_and(|u| u.starts_with("https://"))
    }
}

/// The user a request acts as (inserted by [`require_user`]).
#[derive(Clone, Debug)]
pub struct CurrentUser(pub String);

impl CurrentUser {
    pub fn actor(&self) -> Actor {
        Actor::Human {
            user: self.0.clone(),
        }
    }
}

fn local_username() -> String {
    match local_user() {
        Actor::Human { user } => user,
        _ => "unknown".into(),
    }
}

// ---------- user management (shared with the CLI) ----------

#[derive(Debug, thiserror::Error)]
pub enum UserError {
    #[error("{0}")]
    Invalid(String),
    #[error("user '{0}' not found")]
    NotFound(String),
    #[error("user '{0}' already exists")]
    Exists(String),
    #[error(transparent)]
    Db(#[from] sqlx::Error),
    #[error("password hashing failed: {0}")]
    Hash(String),
}

fn validate(username: &str, password: &str) -> Result<(), UserError> {
    let ok_name = !username.is_empty()
        && username.len() <= 64
        && username
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || "._-".contains(c));
    if !ok_name {
        return Err(UserError::Invalid(
            "username must be 1-64 chars of [A-Za-z0-9._-]".into(),
        ));
    }
    if password.chars().count() < MIN_PASSWORD_LEN {
        return Err(UserError::Invalid(format!(
            "password must be at least {MIN_PASSWORD_LEN} characters"
        )));
    }
    Ok(())
}

pub fn hash_password(password: &str) -> Result<String, UserError> {
    let salt = SaltString::generate(&mut OsRng);
    Argon2::default()
        .hash_password(password.as_bytes(), &salt)
        .map(|h| h.to_string())
        .map_err(|e| UserError::Hash(e.to_string()))
}

fn verify_password(password: &str, hash: &str) -> bool {
    PasswordHash::new(hash).is_ok_and(|h| {
        Argon2::default()
            .verify_password(password.as_bytes(), &h)
            .is_ok()
    })
}

pub async fn add_user(pool: &PgPool, username: &str, password: &str) -> Result<(), UserError> {
    validate(username, password)?;
    let hash = hash_password(password)?;
    let res = sqlx::query(
        "INSERT INTO users (username, password_hash) VALUES ($1, $2) ON CONFLICT DO NOTHING",
    )
    .bind(username)
    .bind(hash)
    .execute(pool)
    .await?;
    if res.rows_affected() == 0 {
        return Err(UserError::Exists(username.into()));
    }
    Ok(())
}

/// Change a password and log the user out everywhere.
pub async fn set_password(pool: &PgPool, username: &str, password: &str) -> Result<(), UserError> {
    validate(username, password)?;
    let hash = hash_password(password)?;
    let mut tx = pool.begin().await?;
    let res = sqlx::query("UPDATE users SET password_hash = $2 WHERE username = $1")
        .bind(username)
        .bind(hash)
        .execute(&mut *tx)
        .await?;
    if res.rows_affected() == 0 {
        return Err(UserError::NotFound(username.into()));
    }
    sqlx::query("DELETE FROM auth_sessions WHERE username = $1")
        .bind(username)
        .execute(&mut *tx)
        .await?;
    tx.commit().await?;
    Ok(())
}

/// Delete a user (their sessions go with them).
pub async fn remove_user(pool: &PgPool, username: &str) -> Result<(), UserError> {
    let res = sqlx::query("DELETE FROM users WHERE username = $1")
        .bind(username)
        .execute(pool)
        .await?;
    if res.rows_affected() == 0 {
        return Err(UserError::NotFound(username.into()));
    }
    Ok(())
}

#[derive(Debug, Serialize, sqlx::FromRow)]
pub struct UserRow {
    pub username: String,
    pub created_at: chrono::DateTime<chrono::Utc>,
    pub sessions: i64,
    pub last_seen_at: Option<chrono::DateTime<chrono::Utc>>,
}

pub async fn list_users(pool: &PgPool) -> Result<Vec<UserRow>, UserError> {
    Ok(sqlx::query_as(
        "SELECT u.username, u.created_at,
                count(s.token_hash) FILTER (WHERE s.expires_at > now()) AS sessions,
                max(s.last_seen_at) AS last_seen_at
         FROM users u LEFT JOIN auth_sessions s USING (username)
         GROUP BY u.username, u.created_at ORDER BY u.username",
    )
    .fetch_all(pool)
    .await?)
}

pub async fn user_count(pool: &PgPool) -> Result<i64, sqlx::Error> {
    sqlx::query_scalar("SELECT count(*) FROM users")
        .fetch_one(pool)
        .await
}

// ---------- sessions ----------

fn token_hash(token: &str) -> Vec<u8> {
    Sha256::digest(token.as_bytes()).to_vec()
}

fn new_token() -> String {
    let mut bytes = [0u8; 32];
    rand::rngs::OsRng.fill_bytes(&mut bytes);
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bytes)
}

fn session_cookie(headers: &HeaderMap) -> Option<&str> {
    headers
        .get_all(header::COOKIE)
        .iter()
        .filter_map(|v| v.to_str().ok())
        .flat_map(|v| v.split(';'))
        .filter_map(|kv| kv.trim().split_once('='))
        .find(|(k, _)| *k == COOKIE)
        .map(|(_, v)| v)
        .filter(|v| !v.is_empty())
}

async fn session_user(pool: &PgPool, token: &str) -> Result<Option<String>, sqlx::Error> {
    sqlx::query_scalar(
        "UPDATE auth_sessions SET last_seen_at = now()
         WHERE token_hash = $1 AND expires_at > now() RETURNING username",
    )
    .bind(token_hash(token))
    .fetch_optional(pool)
    .await
}

fn set_cookie(cfg: &AuthConfig, value: &str, max_age: u64) -> HeaderValue {
    let secure = if cfg.secure_cookie() { "; Secure" } else { "" };
    HeaderValue::from_str(&format!(
        "{COOKIE}={value}; Path=/; HttpOnly; SameSite=Strict; Max-Age={max_age}{secure}"
    ))
    .expect("cookie is ascii")
}

fn unauthorized() -> Response {
    (
        StatusCode::UNAUTHORIZED,
        Json(serde_json::json!({ "error": "login required" })),
    )
        .into_response()
}

// ---------- middleware ----------

/// Resolve the request's user: the session cookie's owner with `--auth`, else `$USER`.
pub async fn require_user(State(s): State<AppState>, mut req: Request, next: Next) -> Response {
    let user = if s.auth.enabled {
        let Some(token) = session_cookie(req.headers()) else {
            return unauthorized();
        };
        match session_user(s.pool(), token).await {
            Ok(Some(u)) => u,
            Ok(None) => return unauthorized(),
            Err(e) => return ApiError::from(anyhow::Error::from(e)).into_response(),
        }
    } else {
        local_username()
    };
    req.extensions_mut().insert(CurrentUser(user));
    next.run(req).await
}

/// With `--auth`, refuse state-changing requests that a browser marks as coming
/// from another site or origin (CSRF, including from sibling subdomains, which
/// `SameSite` alone treats as the same site).
pub async fn check_origin(State(s): State<AppState>, req: Request, next: Next) -> Response {
    let safe = matches!(*req.method(), Method::GET | Method::HEAD | Method::OPTIONS);
    if s.auth.enabled && !safe && !same_origin(&s.auth, req.headers()) {
        return (
            StatusCode::FORBIDDEN,
            Json(serde_json::json!({ "error": "cross-origin request refused" })),
        )
            .into_response();
    }
    next.run(req).await
}

fn same_origin(cfg: &AuthConfig, headers: &HeaderMap) -> bool {
    let get = |name: &str| headers.get(name).and_then(|v| v.to_str().ok());
    if let Some(site) = get("sec-fetch-site")
        && !matches!(site, "same-origin" | "none")
    {
        return false;
    }
    let Some(origin) = get("origin") else {
        // Not a browser (curl, scripts): the session cookie is still required.
        return true;
    };
    match &cfg.public_url {
        Some(public) => origin == public,
        None => {
            let host = get("x-forwarded-host").or_else(|| get("host"));
            host.is_some_and(|h| origin.split_once("://").is_some_and(|(_, o)| o == h))
        }
    }
}

// ---------- handlers ----------

#[derive(Serialize)]
pub struct Me {
    auth_enabled: bool,
    user: Option<String>,
}

pub async fn me(State(s): State<AppState>, headers: HeaderMap) -> Result<Json<Me>, ApiError> {
    let user = if !s.auth.enabled {
        Some(local_username())
    } else {
        match session_cookie(&headers) {
            Some(t) => session_user(s.pool(), t)
                .await
                .map_err(anyhow::Error::from)?,
            None => None,
        }
    };
    Ok(Json(Me {
        auth_enabled: s.auth.enabled,
        user,
    }))
}

#[derive(Deserialize)]
pub struct LoginBody {
    username: String,
    password: String,
}

fn client_key(cfg: &AuthConfig, headers: &HeaderMap, peer: Option<SocketAddr>) -> String {
    let proxied = cfg
        .trust_proxy
        .then(|| headers.get("x-real-ip").and_then(|v| v.to_str().ok()))
        .flatten();
    proxied
        .map(str::to_owned)
        .or_else(|| peer.map(|p| p.ip().to_string()))
        .unwrap_or_else(|| "unknown".into())
}

/// `Some(remaining lockout)` if the client has used up its failed attempts.
fn locked_out(cfg: &AuthConfig, key: &str) -> Option<Duration> {
    let mut map = cfg.failures.lock().unwrap();
    map.retain(|_, (_, since)| since.elapsed() < LOCKOUT_WINDOW);
    map.get(key)
        .filter(|(n, _)| *n >= MAX_FAILURES)
        .map(|(_, since)| LOCKOUT_WINDOW.saturating_sub(since.elapsed()))
}

fn record_failure(cfg: &AuthConfig, key: &str) {
    let mut map = cfg.failures.lock().unwrap();
    let e = map.entry(key.to_owned()).or_insert((0, Instant::now()));
    e.0 += 1;
}

/// Hash of an unguessable password, checked when the user does not exist so that
/// a wrong username takes as long as a wrong password.
static DUMMY_HASH: std::sync::LazyLock<String> =
    std::sync::LazyLock::new(|| hash_password(&new_token()).expect("hash dummy password"));

pub async fn login(
    State(s): State<AppState>,
    peer: Option<Extension<ConnectInfo<SocketAddr>>>,
    headers: HeaderMap,
    Json(b): Json<LoginBody>,
) -> Result<Response, ApiError> {
    if !s.auth.enabled {
        return Err(ApiError::bad_request("this server runs without --auth"));
    }
    let key = client_key(&s.auth, &headers, peer.map(|Extension(ConnectInfo(a))| a));
    if let Some(wait) = locked_out(&s.auth, &key) {
        let msg = format!(
            "too many failed logins; try again in {} min",
            wait.as_secs().div_ceil(60)
        );
        return Err(ApiError(StatusCode::TOO_MANY_REQUESTS, msg));
    }
    let stored: Option<String> =
        sqlx::query_scalar("SELECT password_hash FROM users WHERE username = $1")
            .bind(&b.username)
            .fetch_optional(s.pool())
            .await
            .map_err(anyhow::Error::from)?;
    let found = stored.is_some();
    let hash = stored.unwrap_or_else(|| DUMMY_HASH.clone());
    let password = b.password;
    let valid = tokio::task::spawn_blocking(move || verify_password(&password, &hash))
        .await
        .map_err(anyhow::Error::from)?;
    if !(found && valid) {
        record_failure(&s.auth, &key);
        tracing::warn!(client = %key, user = %b.username, "failed login");
        return Err(ApiError(
            StatusCode::UNAUTHORIZED,
            "wrong username or password".into(),
        ));
    }

    let token = new_token();
    let mut tx = s.pool().begin().await.map_err(anyhow::Error::from)?;
    sqlx::query("DELETE FROM auth_sessions WHERE expires_at <= now()")
        .execute(&mut *tx)
        .await
        .map_err(anyhow::Error::from)?;
    sqlx::query(
        "INSERT INTO auth_sessions (token_hash, username, expires_at) VALUES ($1, $2, now() + $3)",
    )
    .bind(token_hash(&token))
    .bind(&b.username)
    .bind(chrono::Duration::from_std(SESSION_TTL).expect("ttl fits"))
    .execute(&mut *tx)
    .await
    .map_err(anyhow::Error::from)?;
    tx.commit().await.map_err(anyhow::Error::from)?;
    s.auth.failures.lock().unwrap().remove(&key);
    tracing::info!(client = %key, user = %b.username, "login");

    let mut res = Json(Me {
        auth_enabled: true,
        user: Some(b.username),
    })
    .into_response();
    res.headers_mut().insert(
        header::SET_COOKIE,
        set_cookie(&s.auth, &token, SESSION_TTL.as_secs()),
    );
    Ok(res)
}

pub async fn logout(State(s): State<AppState>, headers: HeaderMap) -> Result<Response, ApiError> {
    if let Some(token) = session_cookie(&headers) {
        sqlx::query("DELETE FROM auth_sessions WHERE token_hash = $1")
            .bind(token_hash(token))
            .execute(s.pool())
            .await
            .map_err(anyhow::Error::from)?;
    }
    let mut res = StatusCode::NO_CONTENT.into_response();
    res.headers_mut()
        .insert(header::SET_COOKIE, set_cookie(&s.auth, "", 0));
    Ok(res)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn headers(pairs: &[(&'static str, &str)]) -> HeaderMap {
        let mut h = HeaderMap::new();
        for (k, v) in pairs {
            h.append(*k, HeaderValue::from_str(v).unwrap());
        }
        h
    }

    #[test]
    fn reads_session_cookie_among_others() {
        let h = headers(&[("cookie", "a=1; wh_session=tok; b=2")]);
        assert_eq!(session_cookie(&h), Some("tok"));
        assert_eq!(session_cookie(&headers(&[("cookie", "wh_session=")])), None);
        assert_eq!(session_cookie(&HeaderMap::new()), None);
    }

    #[test]
    fn origin_rules() {
        let public = AuthConfig::new(true, Some("https://wh.example.com/".into()), true);
        assert!(same_origin(
            &public,
            &headers(&[("origin", "https://wh.example.com")])
        ));
        assert!(!same_origin(
            &public,
            &headers(&[("origin", "https://evil.example.com")])
        ));
        assert!(!same_origin(
            &public,
            &headers(&[("sec-fetch-site", "same-site")])
        ));
        assert!(
            same_origin(&public, &HeaderMap::new()),
            "non-browser clients still need the cookie"
        );

        let local = AuthConfig::new(true, None, false);
        assert!(same_origin(
            &local,
            &headers(&[
                ("origin", "http://127.0.0.1:7070"),
                ("host", "127.0.0.1:7070")
            ])
        ));
        assert!(!same_origin(
            &local,
            &headers(&[("origin", "http://other:7070"), ("host", "127.0.0.1:7070")])
        ));
    }

    #[test]
    fn password_roundtrip_and_validation() {
        let h = hash_password("correct horse battery").unwrap();
        assert!(verify_password("correct horse battery", &h));
        assert!(!verify_password("wrong", &h));
        assert!(validate("thai", "short").is_err());
        assert!(validate("bad name", "long enough password").is_err());
        assert!(validate("thai.b-1", "long enough password").is_ok());
    }

    #[test]
    fn lockout_after_max_failures() {
        let cfg = AuthConfig::new(true, None, false);
        for _ in 0..MAX_FAILURES {
            assert!(locked_out(&cfg, "1.2.3.4").is_none());
            record_failure(&cfg, "1.2.3.4");
        }
        assert!(locked_out(&cfg, "1.2.3.4").is_some());
        assert!(locked_out(&cfg, "5.6.7.8").is_none());
    }

    #[test]
    fn client_key_trusts_proxy_only_when_told() {
        let h = headers(&[("x-real-ip", "9.9.9.9")]);
        let peer = Some("127.0.0.1:5000".parse().unwrap());
        assert_eq!(
            client_key(&AuthConfig::new(true, None, true), &h, peer),
            "9.9.9.9"
        );
        assert_eq!(
            client_key(&AuthConfig::new(true, None, false), &h, peer),
            "127.0.0.1"
        );
    }
}
