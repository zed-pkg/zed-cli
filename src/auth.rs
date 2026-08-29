//! Human authentication for the CLI.
//!
//! shared-auth is the session authority. When Supabase is configured, the CLI
//! signs in with Supabase Auth first and exchanges that access token at
//! shared-auth, retaining both independently refreshable sessions. This
//! mirrors the dual-authority contract used by downstream services: prefer the
//! shared-auth JWT, but keep a valid Supabase JWT available during an outage.

use std::collections::BTreeMap;
use std::fs;
use std::io::{BufRead as _, Read as _, Write as _};
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, anyhow, bail};
use reqwest::StatusCode;
use reqwest::blocking::{Client, Response};
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use zed_lock::{LockClass, LockManager, LockRequest};

use crate::cli::AuthProvider;
use crate::config::Config;

const REFRESH_SKEW_SECS: u64 = 60;
const MAX_AUTH_RESPONSE_BYTES: u64 = 1024 * 1024;

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct TokenPair {
    pub access_token: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub refresh_token: Option<String>,
    pub expires_at: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub refresh_expires_at: Option<u64>,
}

impl TokenPair {
    fn usable_at(&self, now: u64) -> bool {
        !self.access_token.is_empty() && self.expires_at > now
    }

    fn needs_refresh_at(&self, now: u64) -> bool {
        self.expires_at <= now.saturating_add(REFRESH_SKEW_SECS)
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct AuthSession {
    pub provider: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub email: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub shared_user_id: Option<String>,
    #[serde(default)]
    pub roles: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub shared_auth: Option<TokenPair>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub supabase: Option<TokenPair>,
}

impl AuthSession {
    fn bearer(&self, now: u64) -> Option<&str> {
        self.shared_auth
            .as_ref()
            .filter(|pair| pair.usable_at(now))
            .or_else(|| self.supabase.as_ref().filter(|pair| pair.usable_at(now)))
            .map(|pair| pair.access_token.as_str())
    }
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct AuthStore {
    #[serde(default)]
    sessions: BTreeMap<String, AuthSession>,
}

#[derive(Debug, Deserialize)]
struct SharedAuthResponse {
    access_token: String,
    #[serde(default)]
    refresh_token: Option<String>,
    expires_at: u64,
    #[serde(default)]
    refresh_expires_at: Option<u64>,
    #[serde(default)]
    shared_user_id: Option<String>,
    #[serde(default)]
    provider: Option<String>,
    #[serde(default)]
    roles: Vec<String>,
}

#[derive(Debug, Deserialize)]
struct SupabaseResponse {
    #[serde(default)]
    access_token: Option<String>,
    #[serde(default)]
    refresh_token: Option<String>,
    #[serde(default)]
    expires_in: Option<u64>,
    #[serde(default)]
    expires_at: Option<u64>,
    #[serde(default)]
    user: Option<SupabaseUser>,
}

#[derive(Debug, Deserialize)]
struct SupabaseUser {
    #[serde(default)]
    id: Option<String>,
    #[serde(default)]
    email: Option<String>,
}

struct AuthClient {
    http: Client,
    auth_url: String,
    supabase_url: Option<String>,
    supabase_key: Option<String>,
}

impl AuthClient {
    fn new(cfg: &Config) -> Result<Self> {
        let auth_url = validate_base_url(&cfg.auth_url, "shared-auth")?;
        let supabase_url = cfg
            .supabase_url
            .as_deref()
            .map(|url| validate_base_url(url, "Supabase"))
            .transpose()?;
        if supabase_url.is_some() != cfg.supabase_key.is_some() {
            bail!(
                "Supabase auth requires both --supabase-url and --supabase-key \
                 (or ZED_PKG_SUPABASE_URL and ZED_PKG_SUPABASE_KEY)"
            );
        }
        let http = Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .timeout(Duration::from_secs(15))
            .user_agent(concat!("zed-cli/", env!("CARGO_PKG_VERSION")))
            .build()
            .context("building auth HTTP client")?;
        Ok(Self {
            http,
            auth_url,
            supabase_url,
            supabase_key: cfg.supabase_key.clone(),
        })
    }

    fn shared_login(&self, email: &str, password: &str) -> Result<SharedAuthResponse> {
        self.post_json(
            &format!("{}/auth/login", self.auth_url),
            &serde_json::json!({ "email": email, "password": password }),
            &[],
        )
    }

    fn shared_signup(
        &self,
        email: &str,
        password: &str,
        display_name: Option<&str>,
    ) -> Result<SharedAuthResponse> {
        self.post_json(
            &format!("{}/auth/register", self.auth_url),
            &serde_json::json!({
                "email": email,
                "password": password,
                "display_name": display_name,
            }),
            &[],
        )
    }

    fn shared_exchange(&self, provider_token: &str) -> Result<SharedAuthResponse> {
        self.post_json(
            &format!("{}/auth/exchange", self.auth_url),
            &serde_json::json!({ "access_token": provider_token }),
            &[("authorization", format!("Bearer {provider_token}"))],
        )
    }

    fn shared_refresh(&self, refresh_token: &str) -> Result<SharedAuthResponse> {
        self.post_json(
            &format!("{}/auth/refresh", self.auth_url),
            &serde_json::json!({ "refresh_token": refresh_token }),
            &[],
        )
    }

    fn shared_logout(&self, refresh_token: &str) -> Result<()> {
        self.post_empty(
            &format!("{}/auth/logout", self.auth_url),
            Some(&serde_json::json!({ "refresh_token": refresh_token })),
            &[],
        )
    }

    fn supabase_login(&self, email: &str, password: &str) -> Result<SupabaseResponse> {
        let (base, key) = self.require_supabase()?;
        self.post_json(
            &format!("{base}/auth/v1/token?grant_type=password"),
            &serde_json::json!({ "email": email, "password": password }),
            &supabase_headers(key),
        )
    }

    fn supabase_signup(&self, email: &str, password: &str) -> Result<SupabaseResponse> {
        let (base, key) = self.require_supabase()?;
        self.post_json(
            &format!("{base}/auth/v1/signup"),
            &serde_json::json!({ "email": email, "password": password }),
            &supabase_headers(key),
        )
    }

    fn supabase_refresh(&self, refresh_token: &str) -> Result<SupabaseResponse> {
        let (base, key) = self.require_supabase()?;
        self.post_json(
            &format!("{base}/auth/v1/token?grant_type=refresh_token"),
            &serde_json::json!({ "refresh_token": refresh_token }),
            &supabase_headers(key),
        )
    }

    fn supabase_logout(&self, access_token: &str) -> Result<()> {
        let (base, key) = self.require_supabase()?;
        self.post_empty(
            &format!("{base}/auth/v1/logout"),
            None,
            &[
                ("apikey", key.to_string()),
                ("authorization", format!("Bearer {access_token}")),
            ],
        )
    }

    fn require_supabase(&self) -> Result<(&str, &str)> {
        match (&self.supabase_url, &self.supabase_key) {
            (Some(url), Some(key)) => Ok((url, key)),
            _ => bail!(
                "Supabase auth is not configured; set --supabase-url and \
                 --supabase-key (the public publishable/anon key)"
            ),
        }
    }

    fn post_json<T: DeserializeOwned>(
        &self,
        url: &str,
        body: &serde_json::Value,
        headers: &[(&str, String)],
    ) -> Result<T> {
        let mut request = self.http.post(url).json(body);
        for (name, value) in headers {
            request = request.header(*name, value);
        }
        let response = request
            .send()
            .with_context(|| format!("auth service at {url} is unavailable"))?;
        let (status, bytes) = bounded_body(response)?;
        if !status.is_success() {
            return Err(auth_http_error(status));
        }
        serde_json::from_slice(&bytes).context("auth service returned an invalid response")
    }

    fn post_empty(
        &self,
        url: &str,
        body: Option<&serde_json::Value>,
        headers: &[(&str, String)],
    ) -> Result<()> {
        let mut request = self.http.post(url);
        if let Some(body) = body {
            request = request.json(body);
        }
        for (name, value) in headers {
            request = request.header(*name, value);
        }
        let response = request
            .send()
            .with_context(|| format!("auth service at {url} is unavailable"))?;
        let (status, _) = bounded_body(response)?;
        if status.is_success() {
            Ok(())
        } else {
            Err(auth_http_error(status))
        }
    }
}

pub fn login(
    cfg: &Config,
    email: Option<&str>,
    provider: AuthProvider,
    password_stdin: bool,
) -> Result<()> {
    let email = read_email(email)?;
    let password = read_password(password_stdin, false)?;
    let client = AuthClient::new(cfg)?;
    let provider = resolve_provider(&client, provider);
    let session = match provider {
        AuthProvider::SharedAuth => {
            session_from_shared(client.shared_login(&email, &password)?, Some(email.clone()))
        }
        AuthProvider::Supabase => {
            let response = client.supabase_login(&email, &password)?;
            session_from_supabase(&client, response, Some(email.clone()))?
                .ok_or_else(|| anyhow!("Supabase login returned no session"))?
        }
        AuthProvider::Auto => unreachable!("auto provider is resolved above"),
    };
    save_session(cfg, session)?;
    println!("signed in as {email}");
    Ok(())
}

pub fn signup(
    cfg: &Config,
    email: Option<&str>,
    provider: AuthProvider,
    display_name: Option<&str>,
    password_stdin: bool,
) -> Result<()> {
    let email = read_email(email)?;
    let password = read_password(password_stdin, true)?;
    let client = AuthClient::new(cfg)?;
    let provider = resolve_provider(&client, provider);
    let session = match provider {
        AuthProvider::SharedAuth => session_from_shared(
            client.shared_signup(&email, &password, display_name)?,
            Some(email.clone()),
        ),
        AuthProvider::Supabase => {
            let response = client.supabase_signup(&email, &password)?;
            let Some(session) = session_from_supabase(&client, response, Some(email.clone()))?
            else {
                println!(
                    "account created for {email}; confirm the email, then run `zed auth login`"
                );
                return Ok(());
            };
            session
        }
        AuthProvider::Auto => unreachable!("auto provider is resolved above"),
    };
    save_session(cfg, session)?;
    println!("account created; signed in as {email}");
    Ok(())
}

pub fn signout(cfg: &Config) -> Result<()> {
    let path = store_path(&cfg.home);
    if !path.exists() {
        println!("not signed in");
        return Ok(());
    }
    with_locked_store(&cfg.home, |store| {
        let Some(session) = store.sessions.remove(&session_key(cfg)) else {
            println!("not signed in");
            return Ok(());
        };
        match AuthClient::new(cfg) {
            Ok(client) => {
                if let Some(refresh_token) = session
                    .shared_auth
                    .as_ref()
                    .and_then(|pair| pair.refresh_token.as_deref())
                    && let Err(error) = client.shared_logout(refresh_token)
                {
                    eprintln!("warning: shared-auth session revocation failed: {error}");
                }
                if let Some(access_token) = session
                    .supabase
                    .as_ref()
                    .map(|pair| pair.access_token.as_str())
                    && let Err(error) = client.supabase_logout(access_token)
                {
                    eprintln!("warning: Supabase session revocation failed: {error}");
                }
            }
            Err(error) => {
                eprintln!("warning: remote session revocation skipped: {error}");
            }
        }
        println!("signed out; local tokens removed");
        Ok(())
    })
}

pub fn status(cfg: &Config) -> Result<()> {
    let path = store_path(&cfg.home);
    if !path.exists() {
        println!("not signed in");
        return Ok(());
    }
    // This also performs the normal refresh-before-expiry behavior.
    let _ = resolve_bearer(cfg)?;
    let store = load_store(&cfg.home)?;
    let Some(session) = store.sessions.get(&session_key(cfg)) else {
        println!("not signed in");
        return Ok(());
    };
    println!(
        "signed in{} via {}",
        session
            .email
            .as_deref()
            .map(|email| format!(" as {email}"))
            .unwrap_or_default(),
        session.provider
    );
    if let Some(id) = &session.shared_user_id {
        println!("shared user: {id}");
    }
    if let Some(pair) = &session.shared_auth {
        println!("shared-auth JWT expires at {}", pair.expires_at);
    }
    if let Some(pair) = &session.supabase {
        println!("Supabase JWT expires at {}", pair.expires_at);
    }
    println!("session file: {}", store_path(&cfg.home).display());
    Ok(())
}

pub fn refresh(cfg: &Config) -> Result<()> {
    require_session_file(cfg)?;
    with_locked_store(&cfg.home, |store| {
        let session = store
            .sessions
            .get_mut(&session_key(cfg))
            .ok_or_else(|| anyhow!("not signed in; run `zed auth login`"))?;
        let client = AuthClient::new(cfg)?;
        refresh_session(&client, session, true)?;
        println!("refreshed authentication tokens");
        Ok(())
    })
}

pub fn print_token(cfg: &Config) -> Result<()> {
    let token =
        resolve_bearer(cfg)?.ok_or_else(|| anyhow!("not signed in; run `zed auth login`"))?;
    println!("{token}");
    Ok(())
}

/// Return the best current bearer token for registry requests. The shared-auth
/// JWT wins; the Supabase JWT is the outage fallback. Refresh-token rotation is
/// serialized with a local file lock so concurrent CLI commands cannot replay
/// the same one-time refresh token.
pub fn resolve_bearer(cfg: &Config) -> Result<Option<String>> {
    if !store_path(&cfg.home).exists() {
        return Ok(None);
    }
    with_locked_store(&cfg.home, |store| {
        let Some(session) = store.sessions.get_mut(&session_key(cfg)) else {
            return Ok(None);
        };
        let now = unix_now();
        let refresh_needed = session
            .shared_auth
            .as_ref()
            .is_some_and(|pair| pair.needs_refresh_at(now))
            || session
                .supabase
                .as_ref()
                .is_some_and(|pair| pair.needs_refresh_at(now));
        if refresh_needed {
            let client = AuthClient::new(cfg)?;
            refresh_session(&client, session, false)?;
        }
        Ok(session.bearer(unix_now()).map(str::to_owned))
    })
}

fn refresh_session(client: &AuthClient, session: &mut AuthSession, force: bool) -> Result<()> {
    let now = unix_now();
    let mut failures = Vec::new();

    let refresh_shared = session
        .shared_auth
        .as_ref()
        .is_some_and(|pair| force || pair.needs_refresh_at(now));
    if refresh_shared {
        let refresh_token = session
            .shared_auth
            .as_ref()
            .and_then(|pair| pair.refresh_token.clone());
        match refresh_token {
            Some(token) => match client.shared_refresh(&token) {
                Ok(response) => {
                    let email = session.email.clone();
                    let refreshed = session_from_shared(response, email);
                    session.shared_auth = refreshed.shared_auth;
                    session.shared_user_id = refreshed.shared_user_id;
                    session.roles = refreshed.roles;
                }
                Err(error) => {
                    failures.push(format!("shared-auth refresh: {error}"));
                    if session
                        .shared_auth
                        .as_ref()
                        .is_some_and(|pair| !pair.usable_at(now))
                    {
                        session.shared_auth = None;
                    }
                }
            },
            None => {
                if session
                    .shared_auth
                    .as_ref()
                    .is_some_and(|pair| !pair.usable_at(now))
                {
                    session.shared_auth = None;
                }
            }
        }
    }

    let refresh_supabase = session
        .supabase
        .as_ref()
        .is_some_and(|pair| force || pair.needs_refresh_at(now));
    if refresh_supabase {
        let refresh_token = session
            .supabase
            .as_ref()
            .and_then(|pair| pair.refresh_token.clone());
        match refresh_token {
            Some(token) => match client.supabase_refresh(&token) {
                Ok(response) => match supabase_pair(&response) {
                    Ok(Some(pair)) => session.supabase = Some(pair),
                    Ok(None) => failures.push("Supabase refresh returned no session".into()),
                    Err(error) => failures.push(format!("Supabase refresh: {error}")),
                },
                Err(error) => {
                    failures.push(format!("Supabase refresh: {error}"));
                    if session
                        .supabase
                        .as_ref()
                        .is_some_and(|pair| !pair.usable_at(now))
                    {
                        session.supabase = None;
                    }
                }
            },
            None => {
                if session
                    .supabase
                    .as_ref()
                    .is_some_and(|pair| !pair.usable_at(now))
                {
                    session.supabase = None;
                }
            }
        }
    }

    // If shared-auth could not refresh but Supabase remains healthy, mint a
    // replacement shared-auth session from the provider JWT.
    if session.shared_auth.is_none()
        && let Some(provider_token) = session
            .supabase
            .as_ref()
            .filter(|pair| pair.usable_at(unix_now()))
            .map(|pair| pair.access_token.clone())
    {
        match client.shared_exchange(&provider_token) {
            Ok(response) => {
                let email = session.email.clone();
                let exchanged = session_from_shared(response, email);
                session.shared_auth = exchanged.shared_auth;
                session.shared_user_id = exchanged.shared_user_id;
                session.roles = exchanged.roles;
            }
            Err(error) => failures.push(format!("shared-auth exchange: {error}")),
        }
    }

    if session.bearer(unix_now()).is_some() {
        for failure in failures {
            eprintln!("warning: {failure}; using the other valid authority");
        }
        Ok(())
    } else if failures.is_empty() {
        bail!("authentication session expired; run `zed auth login`")
    } else {
        bail!(
            "authentication session could not be refreshed: {}",
            failures.join("; ")
        )
    }
}

fn session_from_shared(response: SharedAuthResponse, email: Option<String>) -> AuthSession {
    AuthSession {
        provider: response
            .provider
            .clone()
            .unwrap_or_else(|| "shared-auth".into()),
        email,
        shared_user_id: response.shared_user_id,
        roles: response.roles,
        shared_auth: Some(TokenPair {
            access_token: response.access_token,
            refresh_token: response.refresh_token,
            expires_at: response.expires_at,
            refresh_expires_at: response.refresh_expires_at,
        }),
        supabase: None,
    }
}

fn session_from_supabase(
    client: &AuthClient,
    response: SupabaseResponse,
    fallback_email: Option<String>,
) -> Result<Option<AuthSession>> {
    let Some(pair) = supabase_pair(&response)? else {
        return Ok(None);
    };
    let email = response
        .user
        .as_ref()
        .and_then(|user| user.email.clone())
        .or(fallback_email);
    let provider_user_id = response.user.as_ref().and_then(|user| user.id.clone());
    let mut session = AuthSession {
        provider: "supabase".into(),
        email: email.clone(),
        shared_user_id: provider_user_id,
        roles: Vec::new(),
        shared_auth: None,
        supabase: Some(pair.clone()),
    };
    match client.shared_exchange(&pair.access_token) {
        Ok(shared) => {
            let exchanged = session_from_shared(shared, email);
            session.provider = "supabase+shared-auth".into();
            session.shared_auth = exchanged.shared_auth;
            session.shared_user_id = exchanged.shared_user_id;
            session.roles = exchanged.roles;
        }
        Err(error) => {
            eprintln!(
                "warning: shared-auth exchange failed ({error}); \
                 saved the Supabase authority and will retry later"
            );
        }
    }
    Ok(Some(session))
}

fn supabase_pair(response: &SupabaseResponse) -> Result<Option<TokenPair>> {
    let Some(access_token) = response.access_token.as_deref() else {
        return Ok(None);
    };
    if access_token.is_empty() || access_token.len() > 16 * 1024 {
        bail!("Supabase returned an invalid access token");
    }
    let expires_at = response
        .expires_at
        .or_else(|| {
            response
                .expires_in
                .map(|seconds| unix_now().saturating_add(seconds))
        })
        .ok_or_else(|| anyhow!("Supabase returned no token expiry"))?;
    Ok(Some(TokenPair {
        access_token: access_token.to_string(),
        refresh_token: response.refresh_token.clone(),
        expires_at,
        refresh_expires_at: None,
    }))
}

fn resolve_provider(client: &AuthClient, requested: AuthProvider) -> AuthProvider {
    match requested {
        AuthProvider::Auto if client.supabase_url.is_some() => AuthProvider::Supabase,
        AuthProvider::Auto => AuthProvider::SharedAuth,
        other => other,
    }
}

fn read_email(given: Option<&str>) -> Result<String> {
    let email = match given {
        Some(email) => email.trim().to_string(),
        None => {
            eprint!("Email: ");
            std::io::stderr().flush()?;
            let mut line = String::new();
            std::io::stdin().lock().read_line(&mut line)?;
            line.trim().to_string()
        }
    };
    if email.is_empty() || email.len() > 320 || !email.contains('@') {
        bail!("a valid email is required (pass --email or set ZED_PKG_AUTH_EMAIL)");
    }
    Ok(email)
}

fn read_password(from_stdin: bool, confirm: bool) -> Result<String> {
    if let Ok(password) = std::env::var("ZED_PKG_AUTH_PASSWORD") {
        if password.is_empty() {
            bail!("ZED_PKG_AUTH_PASSWORD is empty");
        }
        return Ok(password);
    }
    if from_stdin {
        let mut line = String::new();
        std::io::stdin().lock().read_line(&mut line)?;
        let password = line.trim_end_matches(['\r', '\n']).to_string();
        if password.is_empty() {
            bail!("password read from stdin is empty");
        }
        return Ok(password);
    }
    let password = rpassword::prompt_password("Password: ")?;
    if password.is_empty() {
        bail!("password is empty");
    }
    if confirm {
        let repeated = rpassword::prompt_password("Confirm password: ")?;
        if password != repeated {
            bail!("passwords do not match");
        }
    }
    Ok(password)
}

fn validate_base_url(raw: &str, label: &str) -> Result<String> {
    let mut url = reqwest::Url::parse(raw)
        .with_context(|| format!("{label} base URL is invalid: {raw:?}"))?;
    if !url.username().is_empty() || url.password().is_some() {
        bail!("{label} base URL must not contain credentials");
    }
    let loopback = url.host_str().is_some_and(|host| {
        host.eq_ignore_ascii_case("localhost")
            || host
                .parse::<std::net::IpAddr>()
                .is_ok_and(|ip| ip.is_loopback())
    });
    if url.scheme() != "https" && !(url.scheme() == "http" && loopback) {
        bail!("{label} base URL must use HTTPS (HTTP is allowed only on loopback)");
    }
    url.set_query(None);
    url.set_fragment(None);
    Ok(url.as_str().trim_end_matches('/').to_string())
}

fn supabase_headers(key: &str) -> Vec<(&'static str, String)> {
    vec![
        ("apikey", key.to_string()),
        ("authorization", format!("Bearer {key}")),
    ]
}

fn bounded_body(response: Response) -> Result<(StatusCode, Vec<u8>)> {
    let status = response.status();
    if response
        .content_length()
        .is_some_and(|length| length > MAX_AUTH_RESPONSE_BYTES)
    {
        bail!("auth service response exceeded 1 MiB");
    }
    let mut bytes = Vec::new();
    response
        .take(MAX_AUTH_RESPONSE_BYTES + 1)
        .read_to_end(&mut bytes)
        .context("reading auth service response")?;
    if bytes.len() as u64 > MAX_AUTH_RESPONSE_BYTES {
        bail!("auth service response exceeded 1 MiB");
    }
    Ok((status, bytes))
}

fn auth_http_error(status: StatusCode) -> anyhow::Error {
    if matches!(
        status,
        StatusCode::BAD_REQUEST
            | StatusCode::UNAUTHORIZED
            | StatusCode::FORBIDDEN
            | StatusCode::UNPROCESSABLE_ENTITY
    ) {
        anyhow!("authentication was rejected ({status})")
    } else if status == StatusCode::TOO_MANY_REQUESTS {
        anyhow!("authentication rate limit exceeded; try again later")
    } else {
        anyhow!("auth service returned {status}")
    }
}

fn save_session(cfg: &Config, session: AuthSession) -> Result<()> {
    with_locked_store(&cfg.home, |store| {
        store.sessions.insert(session_key(cfg), session);
        Ok(())
    })
}

fn session_key(cfg: &Config) -> String {
    cfg.auth_url.trim_end_matches('/').to_string()
}

fn auth_dir(home: &Path) -> PathBuf {
    home.join("auth")
}

fn store_path(home: &Path) -> PathBuf {
    auth_dir(home).join("sessions.toml")
}

fn require_session_file(cfg: &Config) -> Result<()> {
    if store_path(&cfg.home).exists() {
        Ok(())
    } else {
        bail!("not signed in; run `zed auth login`")
    }
}

fn with_locked_store<T>(
    home: &Path,
    operation: impl FnOnce(&mut AuthStore) -> Result<T>,
) -> Result<T> {
    secure_create_dir(&auth_dir(home))?;
    let lock_path = auth_dir(home).join(".lock");
    let _lock = LockManager::global()
        .acquire_blocking(
            LockRequest::exclusive(&lock_path)
                .operation("auth session store")
                .class(LockClass::Custom(5))
                .queue_same_process(),
        )
        .with_context(|| format!("locking {}", lock_path.display()))?;
    let mut store = load_store(home)?;
    let result = operation(&mut store);
    if result.is_ok() {
        save_store(home, &store)?;
    }
    result
}

fn load_store(home: &Path) -> Result<AuthStore> {
    let path = store_path(home);
    if !path.exists() {
        return Ok(AuthStore::default());
    }
    let text = fs::read_to_string(&path)
        .with_context(|| format!("reading auth session file {}", path.display()))?;
    toml::from_str(&text).with_context(|| format!("parsing auth session file {}", path.display()))
}

fn save_store(home: &Path, store: &AuthStore) -> Result<()> {
    let dir = auth_dir(home);
    secure_create_dir(&dir)?;
    let path = store_path(home);
    let temporary = dir.join(format!(".sessions.toml.tmp-{}", std::process::id()));
    let text = toml::to_string_pretty(store)?;
    {
        let mut file = secure_open(&temporary, true)?;
        file.write_all(text.as_bytes())?;
        file.sync_all()?;
    }
    fs::rename(&temporary, &path)
        .with_context(|| format!("installing auth session file {}", path.display()))?;
    secure_file_mode(&path)?;
    Ok(())
}

fn secure_create_dir(path: &Path) -> Result<()> {
    fs::create_dir_all(path)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        fs::set_permissions(path, fs::Permissions::from_mode(0o700))?;
    }
    Ok(())
}

fn secure_open(path: &Path, truncate: bool) -> Result<fs::File> {
    let mut options = fs::OpenOptions::new();
    options
        .create(true)
        .read(true)
        .write(true)
        .truncate(truncate);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt as _;
        options.mode(0o600);
    }
    let file = options.open(path)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        file.set_permissions(fs::Permissions::from_mode(0o600))?;
    }
    Ok(file)
}

fn secure_file_mode(_path: &Path) -> Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        fs::set_permissions(_path, fs::Permissions::from_mode(0o600))?;
    }
    Ok(())
}

fn unix_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config(home: &Path) -> Config {
        Config {
            registry: "https://registry.example.com".into(),
            home: home.to_path_buf(),
            token: None,
            auth_url: "https://registry.example.com/shared-auth".into(),
            supabase_url: None,
            supabase_key: None,
            interactive: false,
            mirrors: Vec::new(),
            fallback: crate::mirrored_registry::FallbackPolicy::Disabled,
        }
    }

    fn session(shared_expiry: u64, supabase_expiry: u64) -> AuthSession {
        AuthSession {
            provider: "supabase+shared-auth".into(),
            email: Some("person@example.com".into()),
            shared_user_id: Some("shared-1".into()),
            roles: vec!["publisher".into()],
            shared_auth: Some(TokenPair {
                access_token: "shared-jwt".into(),
                refresh_token: Some("sat_refresh_example".into()),
                expires_at: shared_expiry,
                refresh_expires_at: None,
            }),
            supabase: Some(TokenPair {
                access_token: "supabase-jwt".into(),
                refresh_token: Some("supabase-refresh".into()),
                expires_at: supabase_expiry,
                refresh_expires_at: None,
            }),
        }
    }

    #[test]
    fn shared_auth_bearer_wins_and_supabase_is_fallback() {
        let now = unix_now();
        assert_eq!(session(now + 60, now + 60).bearer(now), Some("shared-jwt"));
        assert_eq!(session(now - 1, now + 60).bearer(now), Some("supabase-jwt"));
        assert_eq!(session(now - 1, now - 1).bearer(now), None);
    }

    #[test]
    fn store_roundtrip_is_scoped_by_auth_authority() {
        let temp = tempfile::tempdir().unwrap();
        let cfg = config(temp.path());
        let now = unix_now();
        save_session(&cfg, session(now + 3600, now + 3600)).unwrap();
        let loaded = load_store(temp.path()).unwrap();
        let stored = loaded.sessions.get(&cfg.auth_url).unwrap();
        assert_eq!(stored.email.as_deref(), Some("person@example.com"));
        assert_eq!(stored.bearer(now), Some("shared-jwt"));
    }

    #[cfg(unix)]
    #[test]
    fn auth_directory_and_session_file_have_private_modes() {
        use std::os::unix::fs::PermissionsExt as _;

        let temp = tempfile::tempdir().unwrap();
        let cfg = config(temp.path());
        let now = unix_now();
        save_session(&cfg, session(now + 3600, now + 3600)).unwrap();

        let dir_mode = fs::metadata(auth_dir(temp.path()))
            .unwrap()
            .permissions()
            .mode()
            & 0o777;
        let file_mode = fs::metadata(store_path(temp.path()))
            .unwrap()
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(dir_mode, 0o700);
        assert_eq!(file_mode, 0o600);
    }

    #[test]
    fn base_urls_require_https_except_for_loopback() {
        assert!(validate_base_url("https://auth.example.com/x", "auth").is_ok());
        assert!(validate_base_url("http://localhost:8120", "auth").is_ok());
        assert!(validate_base_url("http://127.0.0.1:8120", "auth").is_ok());
        assert!(validate_base_url("http://auth.example.com", "auth").is_err());
        assert!(validate_base_url("https://user:pass@auth.example.com", "auth").is_err());
    }

    #[test]
    fn supabase_confirmation_response_has_no_session() {
        let response: SupabaseResponse =
            serde_json::from_str(r#"{"user":{"id":"u1","email":"p@example.com"}}"#).unwrap();
        assert!(supabase_pair(&response).unwrap().is_none());
    }
}
