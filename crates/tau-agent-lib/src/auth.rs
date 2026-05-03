//! OAuth credential management and storage.
//!
//! Implements the Anthropic OAuth flow (PKCE authorization code) and
//! persistent credential storage with file locking for safe multi-process access.

use std::fs;
use std::io::{BufRead, BufReader, Write};
use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use fs2::FileExt;
use rand::RngExt;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

// ---------------------------------------------------------------------------
// Constants (match Claude Code / pi-ai)
// ---------------------------------------------------------------------------

const CLIENT_ID: &str = "9d1c250a-e61b-44d9-88ed-5944d1962f5e";
const AUTHORIZE_URL: &str = "https://claude.ai/oauth/authorize";
const TOKEN_URL: &str = "https://platform.claude.com/v1/oauth/token";
const CALLBACK_PORT: u16 = 53692;
const REDIRECT_URI: &str = "http://localhost:53692/callback";
const SCOPES: &str = "org:create_api_key user:profile user:inference user:sessions:claude_code user:mcp_servers user:file_upload";

/// 5-minute buffer before expiry to trigger refresh.
const EXPIRY_BUFFER_MS: u64 = 5 * 60 * 1000;

// ---------------------------------------------------------------------------
// Credentials
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OAuthCredentials {
    pub refresh: String,
    pub access: String,
    /// Milliseconds since epoch when the access token expires (with buffer applied).
    pub expires: u64,
}

impl OAuthCredentials {
    pub fn is_expired(&self) -> bool {
        crate::types::timestamp_ms() >= self.expires
    }
}

// ---------------------------------------------------------------------------
// PKCE
// ---------------------------------------------------------------------------

struct PkceChallenge {
    verifier: String,
    challenge: String,
}

fn generate_pkce() -> PkceChallenge {
    let mut rng = rand::rng();
    let mut bytes = [0u8; 32];
    rng.fill(&mut bytes);
    let verifier = URL_SAFE_NO_PAD.encode(bytes);

    let hash = Sha256::digest(verifier.as_bytes());
    let challenge = URL_SAFE_NO_PAD.encode(hash);

    PkceChallenge {
        verifier,
        challenge,
    }
}

// ---------------------------------------------------------------------------
// OAuth login flow
// ---------------------------------------------------------------------------

/// Run the Anthropic OAuth login flow.
///
/// 1. Start local callback server on port 53692
/// 2. Print authorization URL for user to open
/// 3. Wait for callback with auth code
/// 4. Exchange code for tokens
///
/// Returns credentials to persist.
pub fn login_anthropic() -> crate::Result<OAuthCredentials> {
    let pkce = generate_pkce();

    // Start callback server
    let listener = TcpListener::bind(("127.0.0.1", CALLBACK_PORT))
        .map_err(|e| crate::Error::Io(format!("bind callback server: {}", e)))?;

    // Build authorization URL
    let auth_url = format!(
        "{}?code=true&client_id={}&response_type=code&redirect_uri={}&scope={}&code_challenge={}&code_challenge_method=S256&state={}",
        AUTHORIZE_URL,
        CLIENT_ID,
        urlencod(REDIRECT_URI),
        urlencod(SCOPES),
        pkce.challenge,
        pkce.verifier,
    );

    eprintln!("\nOpen this URL in your browser to log in:\n");
    eprintln!("  {}\n", auth_url);
    eprintln!("Waiting for authentication callback...\n");

    // Wait for the callback
    let (code, state) = wait_for_callback(&listener)?;

    if state != pkce.verifier {
        return Err(crate::Error::Parse("OAuth state mismatch".into()));
    }

    eprintln!("Exchanging authorization code for tokens...");

    // Exchange code for tokens
    exchange_code(&code, &state, &pkce.verifier, REDIRECT_URI)
}

/// Wait for the OAuth callback on the local server.
/// Returns (code, state).
fn wait_for_callback(listener: &TcpListener) -> crate::Result<(String, String)> {
    let (mut stream, _) = listener
        .accept()
        .map_err(|e| crate::Error::Io(format!("accept callback: {}", e)))?;

    let mut reader = BufReader::new(&stream);
    let mut request_line = String::new();
    reader
        .read_line(&mut request_line)
        .map_err(|e| crate::Error::Io(e.to_string()))?;

    // Parse: GET /callback?code=...&state=... HTTP/1.1
    let path = request_line
        .split_whitespace()
        .nth(1)
        .ok_or_else(|| crate::Error::Parse("bad callback request".into()))?;

    // Parse query string from path (e.g., /callback?code=...&state=...)
    let query = path
        .split_once('?')
        .map(|(_, q)| q)
        .ok_or_else(|| crate::Error::Parse("missing query in callback".into()))?;

    let code = parse_query_param(query, "code")
        .ok_or_else(|| crate::Error::Parse("missing code in callback".into()))?;

    let state = parse_query_param(query, "state")
        .ok_or_else(|| crate::Error::Parse("missing state in callback".into()))?;

    // Send success response
    let body = "<!DOCTYPE html><html><body><h2>Authentication successful!</h2><p>You can close this window.</p></body></html>";
    let response = format!(
        "HTTP/1.1 200 OK\r\nContent-Type: text/html; charset=utf-8\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
        body.len(),
        body,
    );
    stream
        .write_all(response.as_bytes())
        .map_err(|e| crate::Error::Io(e.to_string()))?;

    Ok((code, state))
}

// ---------------------------------------------------------------------------
// Token exchange/refresh types
// ---------------------------------------------------------------------------

#[derive(Serialize)]
struct TokenExchangeRequest {
    grant_type: &'static str,
    client_id: &'static str,
    code: String,
    state: String,
    redirect_uri: String,
    code_verifier: String,
}

#[derive(Serialize)]
struct TokenRefreshRequest {
    grant_type: &'static str,
    client_id: &'static str,
    refresh_token: String,
}

#[derive(Deserialize)]
struct TokenResponse {
    access_token: String,
    refresh_token: String,
    expires_in: u64,
}

impl TokenResponse {
    fn into_credentials(self) -> OAuthCredentials {
        OAuthCredentials {
            access: self.access_token,
            refresh: self.refresh_token,
            expires: crate::types::timestamp_ms() + self.expires_in * 1000 - EXPIRY_BUFFER_MS,
        }
    }
}

/// Exchange authorization code for tokens.
fn exchange_code(
    code: &str,
    state: &str,
    verifier: &str,
    redirect_uri: &str,
) -> crate::Result<OAuthCredentials> {
    let body = TokenExchangeRequest {
        grant_type: "authorization_code",
        client_id: CLIENT_ID,
        code: code.to_string(),
        state: state.to_string(),
        redirect_uri: redirect_uri.to_string(),
        code_verifier: verifier.to_string(),
    };

    let resp: TokenResponse = ureq::post(TOKEN_URL)
        .header("content-type", "application/json")
        .header("accept", "application/json")
        .send_json(&body)
        .map_err(|e| crate::Error::Http(format!("token exchange: {}", e)))?
        .body_mut()
        .read_json()
        .map_err(|e| crate::Error::Parse(format!("token response: {}", e)))?;

    Ok(resp.into_credentials())
}

// ---------------------------------------------------------------------------
// Token refresh
// ---------------------------------------------------------------------------

/// Refresh an expired OAuth token.
pub fn refresh_token(refresh_tok: &str) -> crate::Result<OAuthCredentials> {
    let body = TokenRefreshRequest {
        grant_type: "refresh_token",
        client_id: CLIENT_ID,
        refresh_token: refresh_tok.to_string(),
    };

    let resp: TokenResponse = ureq::post(TOKEN_URL)
        .header("content-type", "application/json")
        .header("accept", "application/json")
        .send_json(&body)
        .map_err(|e| crate::Error::Http(format!("token refresh: {}", e)))?
        .body_mut()
        .read_json()
        .map_err(|e| crate::Error::Parse(format!("refresh response: {}", e)))?;

    Ok(resp.into_credentials())
}

// ---------------------------------------------------------------------------
// Refresh indirection (test hookable)
// ---------------------------------------------------------------------------

/// Call the OAuth refresh endpoint.
///
/// This indirection exists so unit tests can swap in a counter-incrementing
/// closure without needing an HTTP mock.  In production it just calls
/// `refresh_token`.
#[cfg(not(test))]
fn do_refresh(refresh_tok: &str) -> crate::Result<OAuthCredentials> {
    refresh_token(refresh_tok)
}

#[cfg(test)]
fn do_refresh(refresh_tok: &str) -> crate::Result<OAuthCredentials> {
    tests::test_refresh_hook(refresh_tok)
}

// ---------------------------------------------------------------------------
// Credential storage (auth.json with file locking)
// ---------------------------------------------------------------------------

/// Persistent credential store backed by a JSON file.
///
/// Concurrency model:
/// - The `flock` on the file (shared/exclusive) provides cross-process
///   exclusion against other `tau` invocations (e.g. `tau login` while a
///   daemon is running).
/// - Within a single process, `flock(2)` semantics are ambiguous when
///   multiple threads use distinct fds for the same file (the kernel
///   may convert an existing lock rather than block).  To get reliable
///   in-process serialisation we hold an additional `Mutex<()>` around
///   every read/write/refresh.  The mutex is taken *before* the flock,
///   so contention between same-process threads serialises here and the
///   flock effectively only sees one fd per process at a time.
pub struct AuthStorage {
    path: PathBuf,
    in_process_lock: Arc<Mutex<()>>,
}

/// What's stored in auth.json per provider.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum AuthCredential {
    ApiKey { key: String },
    Oauth(OAuthCredentials),
}

type AuthData = std::collections::HashMap<String, AuthCredential>;

impl AuthStorage {
    /// Create storage at `~/.config/tau/auth.json`.
    pub fn default_path() -> PathBuf {
        crate::paths::config_dir().join("auth.json")
    }

    pub fn new(path: PathBuf) -> Self {
        Self {
            path,
            in_process_lock: Arc::new(Mutex::new(())),
        }
    }

    pub fn open_default() -> Self {
        Self::new(Self::default_path())
    }

    /// Read all credentials (under shared lock).
    fn read_locked(&self) -> crate::Result<AuthData> {
        // Serialise same-process readers/writers before touching the fd
        // to avoid relying on flock's ambiguous in-process semantics.
        // INFALLIBLE: `in_process_lock` guards a `Mutex<()>` with no protected state,
        // so recovering from `PoisonError` cannot observe a torn invariant.
        let _in_proc = self
            .in_process_lock
            .lock()
            .unwrap_or_else(|p| p.into_inner());
        self.read_locked_inner()
    }

    /// Inner read: assumes the in-process mutex is already held by the
    /// caller.  Used by `set` / `remove` to make the read-modify-write
    /// cycle atomic against same-process writers.
    fn read_locked_inner(&self) -> crate::Result<AuthData> {
        if !self.path.exists() {
            tracing::warn!(
                path = %self.path.display(),
                "auth read_locked: file does not exist, returning empty AuthData"
            );
            return Ok(AuthData::new());
        }
        let file = fs::File::open(&self.path).map_err(|e| crate::Error::Io(e.to_string()))?;
        let size_before_lock = file.metadata().map(|m| m.len()).unwrap_or(0);
        file.lock_shared()
            .map_err(|e| crate::Error::Io(format!("lock {}: {}", self.path.display(), e)))?;
        let size_after_lock = file.metadata().map(|m| m.len()).unwrap_or(0);
        let data: AuthData = match serde_json::from_reader(&file) {
            Ok(d) => d,
            Err(e) => {
                tracing::warn!(
                    path = %self.path.display(),
                    size_before_lock,
                    size_after_lock,
                    err = %e,
                    "auth read_locked: parse failed under shared lock"
                );
                file.unlock().ok();
                return Err(crate::Error::Parse(e.to_string()));
            }
        };
        file.unlock().map_err(|e| crate::Error::Io(e.to_string()))?;

        let providers: Vec<&str> = data.keys().map(|s| s.as_str()).collect();
        tracing::trace!(
            path = %self.path.display(),
            size_before_lock,
            size_after_lock,
            ?providers,
            "auth read_locked ok"
        );
        Ok(data)
    }

    /// Inner write: assumes the in-process mutex is already held by the
    /// caller.  Used by `set` / `remove` to make the read-modify-write
    /// cycle atomic against same-process writers.
    fn write_locked_inner(&self, data: &AuthData) -> crate::Result<()> {
        ensure_parent(&self.path)?;
        // Open WITHOUT truncating: `OpenOptions::truncate(true)` would
        // truncate as a side-effect of `open()`, before any lock is held,
        // exposing a window in which a concurrent reader sees a 0-byte
        // file and fails to parse.  We truncate *after* taking the
        // exclusive lock instead.
        let file = fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(&self.path)
            .map_err(|e| crate::Error::Io(e.to_string()))?;
        file.lock_exclusive()
            .map_err(|e| crate::Error::Io(format!("lock {}: {}", self.path.display(), e)))?;
        // Now under the exclusive lock — truncate and rewrite.  This is
        // atomic with respect to concurrent `read_locked` callers, who
        // will block on `lock_shared` until we release.
        file.set_len(0)
            .map_err(|e| crate::Error::Io(e.to_string()))?;
        use std::io::Seek;
        (&file)
            .seek(std::io::SeekFrom::Start(0))
            .map_err(|e| crate::Error::Io(e.to_string()))?;
        serde_json::to_writer_pretty(&file, data)
            .map_err(|e| crate::Error::Parse(e.to_string()))?;
        file.unlock().map_err(|e| crate::Error::Io(e.to_string()))?;

        // chmod 600
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&self.path, fs::Permissions::from_mode(0o600))
                .map_err(|e| crate::Error::Io(e.to_string()))?;
        }

        Ok(())
    }

    /// Store a credential for a provider.
    pub fn set(&self, provider: &str, cred: AuthCredential) -> crate::Result<()> {
        // Hold the in-process mutex across the entire read-modify-write
        // cycle so two same-process callers writing disjoint providers
        // both survive (no last-writer-wins from interleaved RMW).
        // INFALLIBLE: `Mutex<()>` — no protected state, so poison recovery is safe.
        let _in_proc = self
            .in_process_lock
            .lock()
            .unwrap_or_else(|p| p.into_inner());
        let mut data = self.read_locked_inner().unwrap_or_default();
        data.insert(provider.to_string(), cred);
        self.write_locked_inner(&data)
    }

    /// Remove a credential.
    pub fn remove(&self, provider: &str) -> crate::Result<()> {
        // INFALLIBLE: `Mutex<()>` — no protected state, so poison recovery is safe.
        let _in_proc = self
            .in_process_lock
            .lock()
            .unwrap_or_else(|p| p.into_inner());
        let mut data = self.read_locked_inner().unwrap_or_default();
        data.remove(provider);
        self.write_locked_inner(&data)
    }

    /// Get credential for a provider.
    pub fn get(&self, provider: &str) -> crate::Result<Option<AuthCredential>> {
        let span = tracing::trace_span!("auth.get", provider = provider);
        let _enter = span.enter();
        let data = self.read_locked()?;
        let cred = data.get(provider).cloned();
        tracing::trace!(present = cred.is_some(), "auth.get result");
        Ok(cred)
    }

    /// List all providers with credentials.
    pub fn list(&self) -> crate::Result<Vec<String>> {
        let data = self.read_locked().unwrap_or_default();
        Ok(data.keys().cloned().collect())
    }

    /// Get API key for a provider, auto-refreshing OAuth tokens if needed.
    /// Performs refresh under exclusive file lock to prevent races.
    pub fn get_api_key(&self, provider: &str) -> crate::Result<Option<String>> {
        self.get_api_key_excluding(provider, None)
    }

    /// Resolve an API key for `provider`, preferring a value different from
    /// `stale` ("the token that just got rejected").
    ///
    /// Semantics (OAuth credentials):
    /// 1. Read the credential under a shared file lock.
    /// 2. If the stored access token is **different** from `stale` and is
    ///    not expired, return it without performing any HTTP refresh.  This
    ///    is the hot path that breaks the 401-thrash cycle after a long
    ///    429 sleep: a sibling session already refreshed and wrote the new
    ///    token; we simply adopt it.
    /// 3. Otherwise (stored access equals `stale`, or it is expired, or no
    ///    credential exists yet) fall through to `refresh_locked`, which
    ///    takes the exclusive lock, re-reads under that lock, and only
    ///    calls the OAuth endpoint when the stored creds really are stale.
    ///
    /// For non-OAuth credentials the stored key is returned unchanged; for
    /// missing credentials we fall back to `env_api_key`.
    pub fn get_api_key_excluding(
        &self,
        provider: &str,
        stale: Option<&str>,
    ) -> crate::Result<Option<String>> {
        let cred = match self.get(provider)? {
            Some(c) => c,
            None => {
                // Fallback to env var
                let env = env_api_key(provider);
                if env.is_none() {
                    tracing::warn!(
                        provider,
                        "auth.get_api_key_excluding: no auth entry and no env var — returning Ok(None)"
                    );
                } else {
                    tracing::trace!(
                        provider,
                        "auth.get_api_key_excluding: no auth entry, falling back to env var"
                    );
                }
                return Ok(env);
            }
        };

        match cred {
            AuthCredential::ApiKey { key } => Ok(Some(key)),
            AuthCredential::Oauth(oauth) => {
                // If the stored token differs from the caller's stale one
                // and is still valid, adopt it without refreshing.  This is
                // the fast path that breaks the thrash cycle.
                if !oauth.is_expired() && stale != Some(oauth.access.as_str()) {
                    tracing::trace!(
                        provider,
                        "auth.get_api_key_excluding: returning stored OAuth access token"
                    );
                    return Ok(Some(oauth.access));
                }
                // Either the stored token is expired, or it equals the
                // stale token the caller just tried — in both cases we go
                // through the refresh path (which double-checks under the
                // exclusive lock before actually hitting the network).
                if !oauth.is_expired() {
                    // Stored equals stale but not expired: the server has
                    // already rejected this token, so force a refresh.
                    let result = self.refresh_locked(provider, &oauth.refresh);
                    if matches!(result, Ok(None)) {
                        tracing::warn!(
                            provider,
                            reason = "refresh_locked returned None for non-expired-but-stale token",
                            "auth.get_api_key_excluding: refresh produced None"
                        );
                    }
                    return result;
                }
                let result = self.refresh_locked(provider, &oauth.refresh);
                if matches!(result, Ok(None)) {
                    tracing::warn!(
                        provider,
                        reason = "refresh_locked returned None for expired token",
                        "auth.get_api_key_excluding: refresh produced None"
                    );
                }
                result
            }
        }
    }

    /// Refresh token under exclusive file lock, re-reading first in case
    /// another process already refreshed.
    fn refresh_locked(&self, provider: &str, stale_refresh: &str) -> crate::Result<Option<String>> {
        // Serialise in-process callers before flock.  Held across the
        // entire refresh — including the network call to the OAuth
        // endpoint — so a concurrent same-process `set` cannot interleave
        // its read-modify-write with ours and clobber the credential we
        // are about to write.  flock alone is unreliable within a single
        // process when distinct fds are used.
        // INFALLIBLE: `Mutex<()>` — no protected state, so poison recovery is safe.
        let _in_proc = self
            .in_process_lock
            .lock()
            .unwrap_or_else(|p| p.into_inner());
        tracing::debug!(provider, "auth refresh_locked: entry");
        ensure_parent(&self.path)?;

        // Open or create the file
        let file = fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(&self.path)
            .map_err(|e| crate::Error::Io(e.to_string()))?;
        file.lock_exclusive()
            .map_err(|e| crate::Error::Io(format!("lock: {}", e)))?;

        // Re-read under lock — another process may have refreshed
        let file_len = file.metadata().map(|m| m.len()).unwrap_or(0);
        let mut data: AuthData = if file_len > 0 {
            match serde_json::from_reader(&file) {
                Ok(d) => d,
                Err(e) => {
                    tracing::warn!(
                        path = %self.path.display(),
                        provider,
                        file_len,
                        err = %e,
                        "auth refresh_locked: parse failed under exclusive lock; refusing to overwrite"
                    );
                    file.unlock().ok();
                    return Err(crate::Error::Parse(format!(
                        "auth.json malformed (refresh path): {}. Re-authenticate with `tau login {}` to repair.",
                        e, provider
                    )));
                }
            }
        } else {
            tracing::warn!(
                path = %self.path.display(),
                provider,
                "auth refresh_locked: file is empty under exclusive lock — starting from empty AuthData"
            );
            AuthData::new()
        };

        if let Some(AuthCredential::Oauth(existing)) = data.get(provider)
            && !existing.is_expired()
        {
            // Another process already refreshed
            let key = existing.access.clone();
            tracing::debug!(
                provider,
                "auth refresh_locked: another process already refreshed under lock — adopting stored token"
            );
            file.unlock().map_err(|e| crate::Error::Io(e.to_string()))?;
            return Ok(Some(key));
        }

        // Actually refresh
        let refresh_tok = if let Some(AuthCredential::Oauth(existing)) = data.get(provider) {
            existing.refresh.clone()
        } else {
            stale_refresh.to_string()
        };

        tracing::debug!(provider, "auth refresh_locked: calling do_refresh");
        let new_creds = match do_refresh(&refresh_tok) {
            Ok(c) => c,
            Err(e) => {
                tracing::warn!(
                    provider,
                    err = %e,
                    "auth refresh_locked: do_refresh failed"
                );
                file.unlock().ok();
                return Err(crate::Error::Http(format!(
                    "token refresh failed for {}: {}",
                    provider, e
                )));
            }
        };

        let key = new_creds.access.clone();
        data.insert(provider.to_string(), AuthCredential::Oauth(new_creds));

        // Rewrite file
        let _ = file.set_len(0);
        // Seek to beginning
        use std::io::Seek;
        let _ = (&file).seek(std::io::SeekFrom::Start(0));
        serde_json::to_writer_pretty(&file, &data)
            .map_err(|e| crate::Error::Parse(e.to_string()))?;
        file.unlock().map_err(|e| crate::Error::Io(e.to_string()))?;

        tracing::debug!(
            provider,
            "auth refresh_locked: refresh succeeded, wrote new credential"
        );
        Ok(Some(key))
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Diagnostic accessor for `env_api_key` so the resolver can report
/// `has_env` in its `Ok(None)` warning without exposing module internals
/// or duplicating the env-var convention.
pub(crate) fn env_api_key_for_diagnostics(provider: &str) -> Option<String> {
    env_api_key(provider)
}

fn env_api_key(provider: &str) -> Option<String> {
    let var = match provider {
        "anthropic" => "ANTHROPIC_API_KEY",
        "openai" => "OPENAI_API_KEY",
        "qwen" | "dashscope" => "DASHSCOPE_API_KEY",
        _ => {
            // Try PROVIDER_API_KEY convention
            let upper = provider.to_uppercase().replace('-', "_");
            return std::env::var(format!("{}_API_KEY", upper)).ok();
        }
    };
    std::env::var(var).ok()
}

fn ensure_parent(path: &Path) -> crate::Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .map_err(|e| crate::Error::Io(format!("mkdir {}: {}", parent.display(), e)))?;
    }
    Ok(())
}

/// Minimal percent-encoding for URL query params.
fn urlencod(s: &str) -> String {
    let mut out = String::with_capacity(s.len() * 2);
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(b as char);
            }
            _ => {
                out.push('%');
                out.push_str(&format!("{:02X}", b));
            }
        }
    }
    out
}

/// Parse a query parameter from a query string.
fn parse_query_param(query: &str, name: &str) -> Option<String> {
    for pair in query.split('&') {
        if let Some((k, v)) = pair.split_once('=')
            && k == name
        {
            // Basic percent-decoding
            return Some(percent_decode(v));
        }
    }
    None
}

/// Basic percent-decoding.
fn percent_decode(s: &str) -> String {
    let mut out = Vec::new();
    let mut bytes = s.bytes();
    while let Some(b) = bytes.next() {
        if b == b'%' {
            let hi = bytes.next().unwrap_or(0);
            let lo = bytes.next().unwrap_or(0);
            let hex = [hi, lo];
            if let Ok(s) = std::str::from_utf8(&hex)
                && let Ok(val) = u8::from_str_radix(s, 16)
            {
                out.push(val);
                continue;
            }
            out.push(b'%');
            out.push(hi);
            out.push(lo);
        } else if b == b'+' {
            out.push(b' ');
        } else {
            out.push(b);
        }
    }
    String::from_utf8(out).unwrap_or_default()
}

// Re-export types from tau-agent-base for backward compatibility
pub use crate::subscription_usage::{ExtraUsage, SubscriptionUsage, UsageBucket, is_oauth_token};

// ---------------------------------------------------------------------------
// Subscription usage
// ---------------------------------------------------------------------------

const USAGE_URL: &str = "https://api.anthropic.com/api/oauth/usage";

/// Raw API response from the usage endpoint.
#[derive(Debug, Clone, Deserialize, Default)]
struct UsageApiResponse {
    five_hour: Option<UsageBucket>,
    seven_day: Option<UsageBucket>,
    seven_day_sonnet: Option<UsageBucket>,
    seven_day_opus: Option<UsageBucket>,
    extra_usage: Option<ExtraUsage>,
}

/// Fetch subscription usage from the Anthropic API.
/// `token` must be a valid OAuth access token.
///
/// Non-2xx responses are surfaced as [`crate::Error::HttpStatus`] so the
/// caller can distinguish 429s (and read `Retry-After`) from generic
/// transport failures. Transport-level failures (DNS, TLS, connection
/// reset, …) still surface as [`crate::Error::Http`].
pub fn fetch_subscription_usage(token: &str) -> crate::Result<SubscriptionUsage> {
    let mut resp = ureq::get(USAGE_URL)
        .header("authorization", &format!("Bearer {}", token))
        .header("anthropic-beta", "oauth-2025-04-20")
        .header("content-type", "application/json")
        .config()
        .http_status_as_error(false)
        .build()
        .call()
        .map_err(|e| crate::Error::Http(format!("usage API: {}", e)))?;

    let status = resp.status().as_u16();
    if status >= 400 {
        let retry_after = resp
            .headers()
            .get("retry-after")
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.parse::<u64>().ok());
        use std::io::Read;
        let mut body_text = String::new();
        let _ = resp.body_mut().as_reader().read_to_string(&mut body_text);
        return Err(crate::Error::HttpStatus {
            status,
            message: format!("usage API: {}", body_text),
            retry_after,
        });
    }

    let resp: UsageApiResponse = resp
        .body_mut()
        .read_json()
        .map_err(|e| crate::Error::Parse(format!("usage response: {}", e)))?;

    Ok(SubscriptionUsage {
        five_hour: resp.five_hour,
        seven_day: resp.seven_day,
        seven_day_sonnet: resp.seven_day_sonnet,
        seven_day_opus: resp.seven_day_opus,
        extra_usage: resp.extra_usage,
    })
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::{Arc, Barrier, Mutex, OnceLock};

    // ---------------------------------------------------------------------
    // Test-only refresh hook
    // ---------------------------------------------------------------------

    type RefreshHook = Box<dyn Fn(&str) -> crate::Result<OAuthCredentials> + Send + Sync>;

    fn hook_slot() -> &'static Mutex<Option<RefreshHook>> {
        static SLOT: OnceLock<Mutex<Option<RefreshHook>>> = OnceLock::new();
        SLOT.get_or_init(|| Mutex::new(None))
    }

    /// Install a test-only refresh hook.  Returned guard restores the
    /// previous hook on drop.
    pub(super) struct HookGuard {
        prev: Option<RefreshHook>,
    }

    impl Drop for HookGuard {
        fn drop(&mut self) {
            let mut slot = hook_slot().lock().expect("hook slot poisoned");
            *slot = self.prev.take();
        }
    }

    pub(super) fn install_refresh_hook<F>(f: F) -> HookGuard
    where
        F: Fn(&str) -> crate::Result<OAuthCredentials> + Send + Sync + 'static,
    {
        let mut slot = hook_slot().lock().expect("hook slot poisoned");
        let prev = slot.take();
        *slot = Some(Box::new(f));
        HookGuard { prev }
    }

    /// Called by `do_refresh` under `#[cfg(test)]`.
    pub(super) fn test_refresh_hook(refresh_tok: &str) -> crate::Result<OAuthCredentials> {
        let slot = hook_slot().lock().expect("hook slot poisoned");
        match slot.as_ref() {
            Some(hook) => hook(refresh_tok),
            None => Err(crate::Error::Http(
                "test_refresh_hook invoked without an installed hook".into(),
            )),
        }
    }

    // Global mutex so concurrency tests that share the process-global hook
    // slot don't stomp on each other when `cargo test` runs them in
    // parallel.  Poison is ignored: a panicked test still releases the
    // lock, we just don't want cascade failures.
    fn test_lock() -> std::sync::MutexGuard<'static, ()> {
        static LK: OnceLock<Mutex<()>> = OnceLock::new();
        let m = LK.get_or_init(|| Mutex::new(()));
        match m.lock() {
            Ok(g) => g,
            Err(poisoned) => poisoned.into_inner(),
        }
    }

    // ---------------------------------------------------------------------
    // Helpers
    // ---------------------------------------------------------------------

    fn tmp_storage() -> (AuthStorage, tempfile::TempDir) {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("auth.json");
        (AuthStorage::new(path), dir)
    }

    fn future_expiry() -> u64 {
        crate::types::timestamp_ms() + 60 * 60 * 1000
    }

    fn past_expiry() -> u64 {
        // Already past, well beyond the 5-minute EXPIRY_BUFFER_MS.
        crate::types::timestamp_ms().saturating_sub(24 * 60 * 60 * 1000)
    }

    fn set_oauth(storage: &AuthStorage, provider: &str, access: &str, refresh: &str, expires: u64) {
        storage
            .set(
                provider,
                AuthCredential::Oauth(OAuthCredentials {
                    refresh: refresh.to_string(),
                    access: access.to_string(),
                    expires,
                }),
            )
            .expect("set oauth");
    }

    // ---------------------------------------------------------------------
    // Unit tests: get_api_key_excluding without touching the refresh path.
    // ---------------------------------------------------------------------

    #[test]
    fn stale_differs_from_stored_returns_stored_without_refresh() {
        let _g = test_lock();
        let (storage, _tmp) = tmp_storage();
        set_oauth(&storage, "anthropic", "A1", "R1", future_expiry());

        // If the hook were called, this would panic the test.
        let _h = install_refresh_hook(|_| {
            panic!("refresh must not be called on the fast path");
        });

        let got = storage
            .get_api_key_excluding("anthropic", Some("A0"))
            .expect("get key");
        assert_eq!(got.as_deref(), Some("A1"));
    }

    #[test]
    fn stale_equals_stored_and_not_expired_returns_same() {
        let _g = test_lock();
        let (storage, _tmp) = tmp_storage();
        set_oauth(&storage, "anthropic", "A1", "R1", future_expiry());

        // When `stale == stored` but the stored token has not expired
        // according to our clock, we fall into `refresh_locked`, which
        // re-checks under the exclusive lock and returns the stored
        // (non-expired) token without hitting the refresh endpoint.  The
        // agent loop's "new_key == stale" guard then stops the retry
        // loop, so we don't spin forever on a server that has invalidated
        // a locally-fresh token.
        let _h = install_refresh_hook(|_| {
            panic!(
                "refresh_locked must not hit the OAuth endpoint when the stored token is non-expired"
            );
        });

        let got = storage
            .get_api_key_excluding("anthropic", Some("A1"))
            .expect("get key");
        assert_eq!(got.as_deref(), Some("A1"));
    }

    #[test]
    fn stale_none_and_not_expired_returns_stored() {
        let _g = test_lock();
        let (storage, _tmp) = tmp_storage();
        set_oauth(&storage, "anthropic", "A1", "R1", future_expiry());

        let _h = install_refresh_hook(|_| {
            panic!("refresh must not be called when token is fresh");
        });

        let got = storage.get_api_key("anthropic").expect("get key");
        assert_eq!(got.as_deref(), Some("A1"));
    }

    #[test]
    fn no_credential_falls_through_to_env() {
        let _g = test_lock();
        let (storage, _tmp) = tmp_storage();
        // Use a provider slug for which env_api_key consults a dedicated
        // variable so this test is deterministic.
        // SAFETY: tests run single-threaded w.r.t. this env var thanks to
        // `test_lock()`.
        unsafe {
            std::env::set_var("ANTHROPIC_API_KEY", "env-key");
        }
        let got = storage
            .get_api_key_excluding("anthropic", Some("whatever"))
            .expect("get key");
        unsafe {
            std::env::remove_var("ANTHROPIC_API_KEY");
        }
        assert_eq!(got.as_deref(), Some("env-key"));
    }

    #[test]
    fn api_key_credential_ignores_stale() {
        let _g = test_lock();
        let (storage, _tmp) = tmp_storage();
        storage
            .set(
                "custom",
                AuthCredential::ApiKey {
                    key: "K".to_string(),
                },
            )
            .expect("set api key");

        // `stale == stored` still returns stored — ApiKey creds are not
        // rotated, so there is nothing to refresh.
        let got = storage
            .get_api_key_excluding("custom", Some("K"))
            .expect("get key");
        assert_eq!(got.as_deref(), Some("K"));
        let got = storage
            .get_api_key_excluding("custom", Some("other"))
            .expect("get key");
        assert_eq!(got.as_deref(), Some("K"));
    }

    // ---------------------------------------------------------------------
    // Concurrency regression test: N sessions wake up with the same stale
    // token, exactly one refresh HTTP call happens.
    // ---------------------------------------------------------------------

    #[test]
    fn concurrent_wake_triggers_exactly_one_refresh() {
        let _g = test_lock();
        let (storage, _tmp) = tmp_storage();
        // Pre-populate an EXPIRED credential (like after a multi-day 429
        // sleep).  Every session's local `options.api_key` is \"A0\".
        set_oauth(&storage, "anthropic", "A0", "R0", past_expiry());

        let calls = Arc::new(AtomicUsize::new(0));
        let calls_c = calls.clone();
        // The hook simulates Anthropic's refresh endpoint: sleep briefly
        // so racing threads have time to pile up against the exclusive
        // lock, then hand back a fresh, non-expired credential.
        let _h = install_refresh_hook(move |_| {
            calls_c.fetch_add(1, Ordering::SeqCst);
            std::thread::sleep(std::time::Duration::from_millis(50));
            Ok(OAuthCredentials {
                refresh: "R1".into(),
                access: "A1".into(),
                expires: future_expiry(),
            })
        });

        let n = 10;
        let storage = Arc::new(storage);
        let barrier = Arc::new(Barrier::new(n));
        let mut handles = Vec::new();
        for _ in 0..n {
            let s = storage.clone();
            let b = barrier.clone();
            handles.push(std::thread::spawn(move || {
                b.wait();
                s.get_api_key_excluding("anthropic", Some("A0"))
                    .expect("get key")
            }));
        }

        let mut results = Vec::new();
        for h in handles {
            results.push(h.join().expect("join"));
        }

        // All callers got the same new token.
        for r in &results {
            assert_eq!(r.as_deref(), Some("A1"));
        }
        // Exactly one refresh HTTP call.
        assert_eq!(
            calls.load(Ordering::SeqCst),
            1,
            "expected exactly one refresh call, got {}",
            calls.load(Ordering::SeqCst),
        );

        // A lagging session wakes up later, still holding the ancient
        // stale token.  The store now has a fresh non-expired credential,
        // so it must be returned WITHOUT an additional refresh.
        let late = storage
            .get_api_key_excluding("anthropic", Some("A0"))
            .expect("get key");
        assert_eq!(late.as_deref(), Some("A1"));
        assert_eq!(
            calls.load(Ordering::SeqCst),
            1,
            "lagging session must not trigger a second refresh",
        );
    }

    // ---------------------------------------------------------------------
    // Concurrency hardening (task 889)
    // ---------------------------------------------------------------------

    /// `write_locked` truncates under the exclusive lock now, so a
    /// concurrent `read_locked` should never observe a 0-byte file and
    /// fail to parse.  Previously, `OpenOptions::truncate(true)` opened
    /// the file 0-byte before the flock was taken; a reader could slip
    /// in during that window and see EOF.
    #[test]
    fn concurrent_read_during_write_never_parse_errors() {
        let _g = test_lock();
        let (storage, _tmp) = tmp_storage();
        // Seed with a non-trivial value so reads have something to parse.
        storage
            .set("anthropic", AuthCredential::ApiKey { key: "K0".into() })
            .expect("seed");

        let storage = Arc::new(storage);
        let stop = Arc::new(std::sync::atomic::AtomicBool::new(false));

        // Writer: rewrite the file in a tight loop.
        let writer = {
            let s = storage.clone();
            let stop = stop.clone();
            std::thread::spawn(move || {
                let mut i = 0u64;
                while !stop.load(Ordering::SeqCst) {
                    let cred = AuthCredential::ApiKey {
                        key: format!("K{}", i),
                    };
                    s.set("anthropic", cred).expect("set");
                    i += 1;
                }
            })
        };

        // Readers: hammer reads for a fixed iteration count.  Any
        // `Parse` error is a regression.
        let n_readers = 4;
        let iters_per_reader = 200;
        let mut readers = Vec::new();
        for _ in 0..n_readers {
            let s = storage.clone();
            readers.push(std::thread::spawn(move || {
                for _ in 0..iters_per_reader {
                    match s.get("anthropic") {
                        Ok(_) => {}
                        Err(crate::Error::Parse(msg)) => {
                            panic!("concurrent read produced Parse error (regression): {}", msg);
                        }
                        Err(e) => panic!("unexpected error: {:?}", e),
                    }
                }
            }));
        }

        for r in readers {
            r.join().expect("reader join");
        }
        stop.store(true, Ordering::SeqCst);
        writer.join().expect("writer join");
    }

    /// A non-empty but malformed auth.json must produce a clear
    /// user-actionable Parse error from `refresh_locked`, not a silent
    /// `unwrap_or_default()` that drops every other provider's creds.
    #[test]
    fn refresh_locked_malformed_file_errors_loudly() {
        let _g = test_lock();
        let (storage, _tmp) = tmp_storage();
        // Hand-write garbage into auth.json.
        std::fs::write(&storage.path, b"corrupt!").expect("write garbage");

        // The hook would only run *after* parsing succeeded; if we ever
        // reach it, the test is broken.
        let _h = install_refresh_hook(|_| {
            panic!("do_refresh must not be called when the auth file is malformed");
        });

        let err = storage
            .refresh_locked("anthropic", "R0")
            .expect_err("refresh on malformed file must error");
        let msg = format!("{:?}", err);
        assert!(
            msg.contains("malformed"),
            "error message must contain 'malformed': {}",
            msg
        );
        assert!(
            msg.contains("tau login"),
            "error message must point user at `tau login`: {}",
            msg
        );

        // The on-disk file must NOT have been overwritten with a
        // partial AuthData (the bug we are fixing).
        let on_disk = std::fs::read(&storage.path).expect("reread");
        assert_eq!(
            on_disk, b"corrupt!",
            "refresh_locked must not rewrite a malformed file"
        );
    }

    /// Two concurrent in-process writers each adding a *different*
    /// provider must both survive in the final file.  With only flock
    /// (and same-process fds) this could degenerate to last-writer-wins;
    /// the in-process mutex pins the read-modify-write cycle.
    #[test]
    fn in_process_mutex_serialises_disjoint_writes() {
        let _g = test_lock();
        let (storage, _tmp) = tmp_storage();
        let storage = Arc::new(storage);

        let barrier = Arc::new(Barrier::new(2));

        let s1 = storage.clone();
        let b1 = barrier.clone();
        let t1 = std::thread::spawn(move || {
            b1.wait();
            for i in 0..50 {
                s1.set(
                    "providerA",
                    AuthCredential::ApiKey {
                        key: format!("A{}", i),
                    },
                )
                .expect("set A");
            }
        });
        let s2 = storage.clone();
        let b2 = barrier.clone();
        let t2 = std::thread::spawn(move || {
            b2.wait();
            for i in 0..50 {
                s2.set(
                    "providerB",
                    AuthCredential::ApiKey {
                        key: format!("B{}", i),
                    },
                )
                .expect("set B");
            }
        });

        t1.join().expect("t1");
        t2.join().expect("t2");

        // Both providers' final entries must be present.
        let data = storage.read_locked().expect("read final");
        assert!(
            data.contains_key("providerA"),
            "providerA missing after concurrent writes (last-writer-wins regression)"
        );
        assert!(
            data.contains_key("providerB"),
            "providerB missing after concurrent writes (last-writer-wins regression)"
        );
    }

    /// A long-running `refresh_locked` call (with the OAuth endpoint
    /// blocked on a barrier so we can guarantee interleaving) must
    /// serialise against a concurrent `set` for a *different* provider.
    /// Without holding the in-process mutex across the refresh, the
    /// refresh's eventual write would clobber the `set`'s entry — the
    /// exact "new credential silently disappears" failure pattern this
    /// task hardens against.
    #[test]
    fn refresh_locked_serialises_against_concurrent_set() {
        let _g = test_lock();
        let (storage, _tmp) = tmp_storage();
        // Pre-populate an EXPIRED OAuth credential so get_api_key_excluding
        // takes the refresh branch.
        set_oauth(&storage, "anthropic", "A0", "R0", past_expiry());

        let storage = Arc::new(storage);

        // Coordination:
        // - `refresh_started`: signalled by the refresh hook once it has
        //   entered the OAuth endpoint stub.  This guarantees the refresh
        //   thread is *inside* refresh_locked's critical section.
        // - `let_refresh_finish`: the writer thread releases this *after*
        //   it attempts its `set`.  If the in-process mutex is held by
        //   the refresh, the `set` will be blocked and we know it
        //   couldn't have raced; if the mutex is NOT held, the `set` will
        //   complete first, then the refresh's later write will clobber
        //   it.
        let refresh_started = Arc::new(Barrier::new(2));
        let let_refresh_finish = Arc::new(std::sync::atomic::AtomicBool::new(false));

        let rs = refresh_started.clone();
        let lrf = let_refresh_finish.clone();
        let _h = install_refresh_hook(move |_| {
            rs.wait();
            // Spin until the writer thread has had a chance to attempt
            // its `set`.  With the in-process mutex held, the writer is
            // blocked here; without it, the writer races ahead.
            while !lrf.load(Ordering::SeqCst) {
                std::thread::sleep(std::time::Duration::from_millis(5));
            }
            Ok(OAuthCredentials {
                refresh: "R1".into(),
                access: "A1".into(),
                expires: future_expiry(),
            })
        });

        // Thread 1: trigger the refresh.
        let s1 = storage.clone();
        let t_refresh = std::thread::spawn(move || {
            s1.get_api_key_excluding("anthropic", Some("A0"))
                .expect("refresh")
        });

        // Wait until the refresh is genuinely inside its critical
        // section before we start the writer.
        refresh_started.wait();

        // Thread 2: write a credential for a DIFFERENT provider while
        // the refresh is mid-flight.  This must not be lost.
        let s2 = storage.clone();
        let t_writer = std::thread::spawn(move || {
            s2.set(
                "openai",
                AuthCredential::ApiKey {
                    key: "OPENAI_KEY".into(),
                },
            )
            .expect("set openai");
        });

        // Give the writer a beat to actually attempt acquiring the
        // in-process mutex (it should now be blocked behind the refresh).
        std::thread::sleep(std::time::Duration::from_millis(50));
        // Now release the refresh hook so refresh_locked can complete.
        let_refresh_finish.store(true, Ordering::SeqCst);

        let refreshed_key = t_refresh.join().expect("refresh join");
        t_writer.join().expect("writer join");

        assert_eq!(refreshed_key.as_deref(), Some("A1"));

        // Both providers must be present in the final file.  If the
        // mutex were not held across the refresh, the refresh would have
        // written its data (read before the `set`) on top of the
        // `set`'s update, dropping the openai entry.
        let data = storage.read_locked().expect("read final");
        assert!(
            data.contains_key("anthropic"),
            "anthropic missing after concurrent refresh+set"
        );
        assert!(
            data.contains_key("openai"),
            "openai missing - refresh_locked clobbered concurrent set (regression)"
        );
        // And specifically the openai entry must be the one the writer
        // produced, not some stale variant.
        match data.get("openai").expect("openai entry") {
            AuthCredential::ApiKey { key } => {
                assert_eq!(key, "OPENAI_KEY", "openai entry was overwritten");
            }
            other => panic!("unexpected openai credential variant: {:?}", other),
        }
    }
}
