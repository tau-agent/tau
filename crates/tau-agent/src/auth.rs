//! OAuth credential management and storage.
//!
//! Implements the Anthropic OAuth flow (PKCE authorization code) and
//! persistent credential storage with file locking for safe multi-process access.

use std::fs;
use std::io::{BufRead, BufReader, Write};
use std::net::TcpListener;
use std::path::{Path, PathBuf};

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
// Credential storage (auth.json with file locking)
// ---------------------------------------------------------------------------

/// Persistent credential store backed by a JSON file.
pub struct AuthStorage {
    path: PathBuf,
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
        Self { path }
    }

    pub fn open_default() -> Self {
        Self::new(Self::default_path())
    }

    /// Read all credentials (under shared lock).
    fn read_locked(&self) -> crate::Result<AuthData> {
        if !self.path.exists() {
            return Ok(AuthData::new());
        }
        let file = fs::File::open(&self.path).map_err(|e| crate::Error::Io(e.to_string()))?;
        file.lock_shared()
            .map_err(|e| crate::Error::Io(format!("lock {}: {}", self.path.display(), e)))?;
        let data: AuthData =
            serde_json::from_reader(&file).map_err(|e| crate::Error::Parse(e.to_string()))?;
        file.unlock().map_err(|e| crate::Error::Io(e.to_string()))?;
        Ok(data)
    }

    /// Write all credentials (under exclusive lock).
    fn write_locked(&self, data: &AuthData) -> crate::Result<()> {
        ensure_parent(&self.path)?;
        let file = fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .open(&self.path)
            .map_err(|e| crate::Error::Io(e.to_string()))?;
        file.lock_exclusive()
            .map_err(|e| crate::Error::Io(format!("lock {}: {}", self.path.display(), e)))?;
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
        let mut data = self.read_locked().unwrap_or_default();
        data.insert(provider.to_string(), cred);
        self.write_locked(&data)
    }

    /// Remove a credential.
    pub fn remove(&self, provider: &str) -> crate::Result<()> {
        let mut data = self.read_locked().unwrap_or_default();
        data.remove(provider);
        self.write_locked(&data)
    }

    /// Get credential for a provider.
    pub fn get(&self, provider: &str) -> crate::Result<Option<AuthCredential>> {
        let data = self.read_locked()?;
        Ok(data.get(provider).cloned())
    }

    /// List all providers with credentials.
    pub fn list(&self) -> crate::Result<Vec<String>> {
        let data = self.read_locked().unwrap_or_default();
        Ok(data.keys().cloned().collect())
    }

    /// Get API key for a provider, auto-refreshing OAuth tokens if needed.
    /// Performs refresh under exclusive file lock to prevent races.
    pub fn get_api_key(&self, provider: &str) -> crate::Result<Option<String>> {
        let cred = match self.get(provider)? {
            Some(c) => c,
            None => {
                // Fallback to env var
                return Ok(env_api_key(provider));
            }
        };

        match cred {
            AuthCredential::ApiKey { key } => Ok(Some(key)),
            AuthCredential::Oauth(oauth) => {
                if !oauth.is_expired() {
                    return Ok(Some(oauth.access));
                }
                // Need refresh — do it under exclusive lock
                self.refresh_locked(provider, &oauth.refresh)
            }
        }
    }

    /// Refresh token under exclusive file lock, re-reading first in case
    /// another process already refreshed.
    fn refresh_locked(&self, provider: &str, stale_refresh: &str) -> crate::Result<Option<String>> {
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
        let mut data: AuthData = if file.metadata().map(|m| m.len()).unwrap_or(0) > 0 {
            serde_json::from_reader(&file).unwrap_or_default()
        } else {
            AuthData::new()
        };

        if let Some(AuthCredential::Oauth(existing)) = data.get(provider)
            && !existing.is_expired()
        {
            // Another process already refreshed
            let key = existing.access.clone();
            file.unlock().map_err(|e| crate::Error::Io(e.to_string()))?;
            return Ok(Some(key));
        }

        // Actually refresh
        let refresh_tok = if let Some(AuthCredential::Oauth(existing)) = data.get(provider) {
            existing.refresh.clone()
        } else {
            stale_refresh.to_string()
        };

        let new_creds = match refresh_token(&refresh_tok) {
            Ok(c) => c,
            Err(e) => {
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

        Ok(Some(key))
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

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

/// Check if an API key is an OAuth token (starts with `sk-ant-oat`).
pub fn is_oauth_token(key: &str) -> bool {
    key.starts_with("sk-ant-oat")
}

// ---------------------------------------------------------------------------
// Subscription usage
// ---------------------------------------------------------------------------

const USAGE_URL: &str = "https://api.anthropic.com/api/oauth/usage";

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct UsageBucket {
    pub utilization: Option<f64>,
    pub resets_at: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct ExtraUsage {
    #[serde(default)]
    pub is_enabled: bool,
    pub monthly_limit: Option<f64>,
    pub used_credits: Option<f64>,
}

/// Raw API response from the usage endpoint.
#[derive(Debug, Clone, Deserialize, Default)]
struct UsageApiResponse {
    five_hour: Option<UsageBucket>,
    seven_day: Option<UsageBucket>,
    seven_day_sonnet: Option<UsageBucket>,
    seven_day_opus: Option<UsageBucket>,
    extra_usage: Option<ExtraUsage>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct SubscriptionUsage {
    pub five_hour: Option<UsageBucket>,
    pub seven_day: Option<UsageBucket>,
    pub seven_day_sonnet: Option<UsageBucket>,
    pub seven_day_opus: Option<UsageBucket>,
    pub extra_usage: Option<ExtraUsage>,
}

/// Fetch subscription usage from the Anthropic API.
/// `token` must be a valid OAuth access token.
pub fn fetch_subscription_usage(token: &str) -> crate::Result<SubscriptionUsage> {
    let resp: UsageApiResponse = ureq::get(USAGE_URL)
        .header("authorization", &format!("Bearer {}", token))
        .header("anthropic-beta", "oauth-2025-04-20")
        .header("content-type", "application/json")
        .call()
        .map_err(|e| crate::Error::Http(format!("usage API: {}", e)))?
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
