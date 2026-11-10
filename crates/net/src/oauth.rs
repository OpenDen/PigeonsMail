//! Loopback OAuth2 (authorization code + PKCE) for Gmail and Outlook.
//!
//! Only the mail scopes needed for IMAP/SMTP are requested. Client IDs are supplied by the
//! caller; none are embedded in the source.
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use anyhow::{anyhow, bail, Context, Result};
use oauth2::basic::BasicClient;
use oauth2::{
    AuthType, AuthUrl, AuthorizationCode, ClientId, ClientSecret, CsrfToken, EndpointNotSet,
    EndpointSet, PkceCodeChallenge, PkceCodeVerifier, RedirectUrl, RefreshToken, Scope,
    TokenResponse, TokenUrl,
};
use url::Url;
use zeroize::Zeroizing;

#[derive(Debug, Clone)]
pub struct OAuthProvider {
    pub name: &'static str,
    pub auth_url: &'static str,
    pub token_url: &'static str,
    pub scopes: &'static [&'static str],
    /// Host used in the redirect URI. The listener always binds to 127.0.0.1.
    pub loopback_host: &'static str,
    pub imap_host: &'static str,
    pub smtp_host: &'static str,
    /// Environment variable holding the registered client ID (see README.md).
    pub client_id_env: &'static str,
    /// Public client ID built into the app. The environment variable overrides it.
    pub default_client_id: Option<&'static str>,
    /// Client secret built into the app at compile time, if one was supplied. The environment variable overrides it.
    pub default_client_secret: Option<&'static str>,
    /// Environment variable holding the client secret, for providers whose desktop clients use one.
    pub client_secret_env: Option<&'static str>,
    extra_auth_params: &'static [(&'static str, &'static str)],
}

pub static GMAIL: OAuthProvider = OAuthProvider {
    name: "gmail",
    auth_url: "https://accounts.google.com/o/oauth2/v2/auth",
    token_url: "https://oauth2.googleapis.com/token",
    scopes: &["https://mail.google.com/"],
    loopback_host: "127.0.0.1",
    imap_host: "imap.gmail.com",
    smtp_host: "smtp.gmail.com",
    client_id_env: "MAILTHIN_GOOGLE_CLIENT_ID",
    default_client_id: Some("456159786850-ep4ps88vqb7rbkhkcv1ukr31h443ofif.apps.googleusercontent.com"),
    default_client_secret: match option_env!("MAILTHIN_BUILTIN_GOOGLE_CLIENT_SECRET") {
        Some(s) if !s.is_empty() => Some(s),
        _ => None,
    },
    client_secret_env: Some("MAILTHIN_GOOGLE_CLIENT_SECRET"),
    extra_auth_params: &[("access_type", "offline"), ("prompt", "consent")],
};

pub static OUTLOOK: OAuthProvider = OAuthProvider {
    name: "outlook",
    auth_url: "https://login.microsoftonline.com/common/oauth2/v2.0/authorize",
    token_url: "https://login.microsoftonline.com/common/oauth2/v2.0/token",
    scopes: &[
        "https://outlook.office.com/IMAP.AccessAsUser.All",
        "https://outlook.office.com/SMTP.Send",
        "offline_access",
    ],
    loopback_host: "localhost",
    imap_host: "outlook.office365.com",
    smtp_host: "smtp.office365.com",
    client_id_env: "MAILTHIN_MICROSOFT_CLIENT_ID",
    default_client_id: None,
    default_client_secret: None,
    client_secret_env: None,
    extra_auth_params: &[],
};

/// Looks up a provider by the name stored in the account's `auth_type` (`oauth:<name>`).
pub fn provider_for_auth_type(auth_type: &str) -> Option<&'static OAuthProvider> {
    match auth_type.strip_prefix("oauth:")? {
        "gmail" => Some(&GMAIL),
        "outlook" => Some(&OUTLOOK),
        _ => None,
    }
}

impl OAuthProvider {
    /// The `auth_type` value stored with accounts that sign in through this provider.
    pub fn auth_type(&self) -> String {
        format!("oauth:{}", self.name)
    }

    /// Reads the client registration from the environment. Missing values are reported by name.
    pub fn client_config_from_env(&self) -> Result<OAuthClientConfig> {
        let client_id = std::env::var(self.client_id_env)
            .ok()
            .filter(|v| !v.trim().is_empty())
            .or_else(|| self.default_client_id.map(str::to_owned))
            .ok_or_else(|| anyhow!("set {} to sign in with {}", self.client_id_env, self.name))?;
        let client_secret = match self.client_secret_env {
            Some(var) => Some(
                std::env::var(var)
                    .ok()
                    .filter(|v| !v.trim().is_empty())
                    .or_else(|| self.default_client_secret.map(str::to_owned))
                    .ok_or_else(|| anyhow!("sign-in with {} is not configured in this build", self.name))?,
            ),
            None => None,
        };
        Ok(OAuthClientConfig { client_id, client_secret })
    }
}

#[derive(Debug, Clone)]
pub struct OAuthClientConfig {
    pub client_id: String,
    /// Google desktop clients require one; Microsoft public clients do not.
    pub client_secret: Option<String>,
}

pub struct OAuthTokens {
    pub access_token: Zeroizing<String>,
    pub refresh_token: Option<Zeroizing<String>>,
    /// Unix timestamp (seconds) at which the access token expires, if the server said so.
    pub expires_at: Option<i64>,
}

type Client = BasicClient<EndpointSet, EndpointNotSet, EndpointNotSet, EndpointNotSet, EndpointSet>;

fn build_client(p: &OAuthProvider, cfg: &OAuthClientConfig, redirect: Option<String>) -> Result<Client> {
    let mut client = BasicClient::new(ClientId::new(cfg.client_id.clone()))
        .set_auth_uri(AuthUrl::new(p.auth_url.to_string())?)
        .set_token_uri(TokenUrl::new(p.token_url.to_string())?)
        .set_auth_type(AuthType::RequestBody);
    if let Some(secret) = &cfg.client_secret {
        client = client.set_client_secret(ClientSecret::new(secret.clone()));
    }
    if let Some(r) = redirect {
        client = client.set_redirect_uri(RedirectUrl::new(r)?);
    }
    Ok(client)
}

fn http_client() -> Result<oauth2::reqwest::Client> {
    // Redirects are disabled to avoid SSRF via the token endpoint.
    Ok(oauth2::reqwest::ClientBuilder::new()
        .redirect(oauth2::reqwest::redirect::Policy::none())
        .build()?)
}

fn now_ts() -> i64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs() as i64).unwrap_or(0)
}

fn to_tokens<T: TokenResponse>(resp: &T) -> OAuthTokens {
    OAuthTokens {
        access_token: Zeroizing::new(resp.access_token().secret().clone()),
        refresh_token: resp.refresh_token().map(|r| Zeroizing::new(r.secret().clone())),
        expires_at: resp.expires_in().map(|d| now_ts() + d.as_secs() as i64),
    }
}

/// An authorization in progress: open [`PendingAuth::auth_url`] in the user's browser, then
/// call [`PendingAuth::complete`].
pub struct PendingAuth {
    pub auth_url: Url,
    provider: OAuthProvider,
    cfg: OAuthClientConfig,
    redirect: String,
    server: tiny_http::Server,
    csrf: CsrfToken,
    verifier: PkceCodeVerifier,
}

pub fn begin_authorization(provider: &OAuthProvider, cfg: &OAuthClientConfig) -> Result<PendingAuth> {
    let server = tiny_http::Server::http("127.0.0.1:0")
        .map_err(|e| anyhow!("cannot start loopback listener: {e}"))?;
    let port = server
        .server_addr()
        .to_ip()
        .context("loopback listener has no IP address")?
        .port();
    let redirect = format!("http://{}:{}/callback", provider.loopback_host, port);

    let client = build_client(provider, cfg, Some(redirect.clone()))?;
    let (challenge, verifier) = PkceCodeChallenge::new_random_sha256();
    let mut req = client.authorize_url(CsrfToken::new_random).set_pkce_challenge(challenge);
    for s in provider.scopes {
        req = req.add_scope(Scope::new((*s).to_string()));
    }
    for (k, v) in provider.extra_auth_params {
        req = req.add_extra_param(*k, *v);
    }
    let (auth_url, csrf) = req.url();

    Ok(PendingAuth { auth_url, provider: provider.clone(), cfg: cfg.clone(), redirect, server, csrf, verifier })
}

impl PendingAuth {
    pub async fn complete(self, timeout: Duration) -> Result<OAuthTokens> {
        let Self { provider, cfg, redirect, server, csrf, verifier, .. } = self;
        let expected_state = csrf.secret().clone();
        let code = tokio::task::spawn_blocking(move || wait_for_code(&server, &expected_state, timeout))
            .await??;

        let client = build_client(&provider, &cfg, Some(redirect))?;
        let resp = client
            .exchange_code(AuthorizationCode::new(code))
            .set_pkce_verifier(verifier)
            .request_async(&http_client()?)
            .await
            .map_err(|e| anyhow!("token exchange failed: {e}"))?;
        Ok(to_tokens(&resp))
    }
}

pub async fn refresh_access_token(
    provider: &OAuthProvider,
    cfg: &OAuthClientConfig,
    refresh_token: &str,
) -> Result<OAuthTokens> {
    let client = build_client(provider, cfg, None)?;
    let resp = client
        .exchange_refresh_token(&RefreshToken::new(refresh_token.to_string()))
        .request_async(&http_client()?)
        .await
        .map_err(|e| anyhow!("token refresh failed: {e}"))?;
    // Providers may omit the refresh token on refresh; the caller then keeps the old one.
    Ok(to_tokens(&resp))
}

/// SASL XOAUTH2 initial response (the IMAP/SMTP layer base64-encodes it).
pub fn xoauth2_string(user: &str, access_token: &str) -> String {
    format!("user={user}\x01auth=Bearer {access_token}\x01\x01")
}

enum Callback {
    Code(String),
    Rejected,
    ProviderError(String),
}

/// Validates a callback request target such as `/callback?code=..&state=..`.
fn parse_callback(target: &str, expected_state: &str) -> Callback {
    let Ok(url) = Url::parse("http://localhost").and_then(|b| b.join(target)) else {
        return Callback::Rejected;
    };
    if url.path() != "/callback" {
        return Callback::Rejected;
    }
    let (mut code, mut state, mut error) = (None, None, None);
    for (k, v) in url.query_pairs() {
        match k.as_ref() {
            "code" => code = Some(v.into_owned()),
            "state" => state = Some(v.into_owned()),
            "error" => error = Some(v.into_owned()),
            _ => {}
        }
    }
    if state.as_deref() != Some(expected_state) {
        return Callback::Rejected;
    }
    if let Some(e) = error {
        return Callback::ProviderError(e);
    }
    match code {
        Some(c) if !c.is_empty() => Callback::Code(c),
        _ => Callback::Rejected,
    }
}

fn wait_for_code(server: &tiny_http::Server, expected_state: &str, timeout: Duration) -> Result<String> {
    let deadline = Instant::now() + timeout;
    loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            bail!("timed out waiting for the browser sign-in to finish");
        }
        let Some(request) = server.recv_timeout(remaining)? else {
            continue;
        };
        let outcome = parse_callback(request.url(), expected_state);
        let (status, body) = match &outcome {
            Callback::Code(_) => (200, "Sign-in complete. You can close this tab and return to PigeonMail."),
            _ => (400, "Sign-in failed. You can close this tab and try again from PigeonMail."),
        };
        let _ = request.respond(tiny_http::Response::from_string(body).with_status_code(status));
        match outcome {
            Callback::Code(c) => return Ok(c),
            Callback::ProviderError(e) => bail!("authorization denied by provider: {e}"),
            Callback::Rejected => continue,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn auth_type_maps_back_to_provider() {
        assert_eq!(provider_for_auth_type(&GMAIL.auth_type()).map(|p| p.name), Some("gmail"));
        assert_eq!(provider_for_auth_type(&OUTLOOK.auth_type()).map(|p| p.name), Some("outlook"));
        assert!(provider_for_auth_type("password").is_none());
    }

    #[test]
    fn xoauth2_format() {
        assert_eq!(
            xoauth2_string("a@b.c", "tok"),
            "user=a@b.c\x01auth=Bearer tok\x01\x01"
        );
    }

    #[test]
    fn callback_accepts_matching_state() {
        match parse_callback("/callback?code=abc%20d&state=s1", "s1") {
            Callback::Code(c) => assert_eq!(c, "abc d"),
            _ => panic!("expected code"),
        }
    }

    #[test]
    fn callback_rejects_bad_state_path_and_missing_code() {
        assert!(matches!(parse_callback("/callback?code=x&state=evil", "s1"), Callback::Rejected));
        assert!(matches!(parse_callback("/other?code=x&state=s1", "s1"), Callback::Rejected));
        assert!(matches!(parse_callback("/callback?state=s1", "s1"), Callback::Rejected));
    }

    #[test]
    fn callback_reports_provider_error() {
        assert!(matches!(
            parse_callback("/callback?error=access_denied&state=s1", "s1"),
            Callback::ProviderError(e) if e == "access_denied"
        ));
    }

    #[test]
    fn auth_url_has_pkce_state_and_only_mail_scopes() {
        let cfg = OAuthClientConfig { client_id: "cid".into(), client_secret: None };
        let pending = begin_authorization(&GMAIL, &cfg).unwrap();
        let q: std::collections::HashMap<_, _> = pending.auth_url.query_pairs().into_owned().collect();
        assert_eq!(q["client_id"], "cid");
        assert_eq!(q["code_challenge_method"], "S256");
        assert!(q.contains_key("code_challenge") && q.contains_key("state"));
        assert_eq!(q["scope"], "https://mail.google.com/");
        assert!(q["redirect_uri"].starts_with("http://127.0.0.1:"));
    }

    #[tokio::test]
    async fn loopback_listener_returns_code_via_real_http() {
        let cfg = OAuthClientConfig { client_id: "cid".into(), client_secret: None };
        let pending = begin_authorization(&GMAIL, &cfg).unwrap();
        let port = pending.server.server_addr().to_ip().unwrap().port();
        let state = pending.csrf.secret().clone();
        let server = pending.server;

        let h = tokio::task::spawn_blocking(move || wait_for_code(&server, &state, Duration::from_secs(5)));
        let s2 = pending.csrf.secret().clone();
        tokio::task::spawn_blocking(move || {
            use std::io::{Read, Write};
            for target in [format!("/callback?code=bad&state=wrong"), format!("/callback?code=good&state={s2}")] {
                let mut sock = std::net::TcpStream::connect(("127.0.0.1", port)).unwrap();
                write!(sock, "GET {target} HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n").unwrap();
                let mut buf = String::new();
                let _ = sock.read_to_string(&mut buf);
            }
        })
        .await
        .unwrap();
        assert_eq!(h.await.unwrap().unwrap(), "good");
    }
}
