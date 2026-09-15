//! Signing in: API keys, subscriptions' OAuth sign-ins and token refresh, and
//! browser sign-ins that issue an ordinary API key.
//!
//! Browser sign-in uses PKCE and a loopback redirect, as each provider's
//! entry describes. The address the browser ends on can also be pasted, for a
//! browser on another machine.

use std::io::{BufRead, BufReader, Read, Write};
use std::net::{Ipv4Addr, SocketAddr, TcpListener, TcpStream};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::thread;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use percent_encoding::{NON_ALPHANUMERIC, percent_decode_str, utf8_percent_encode};
use ring::digest::{SHA256, digest};
use ring::rand::{SecureRandom, SystemRandom};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::config;
use crate::llm::{self, ExchangeError};
use crate::provider::{Access, OAuth, Provider, Redirect};

/// An access token this close to expiring, in seconds, is refreshed first.
const REFRESH_MARGIN: u64 = 300;

/// A subscription sign-in.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub(crate) struct Tokens {
    pub(crate) access: String,
    pub(crate) refresh: String,
    /// Unix time at which `access` expires.
    pub(crate) expires: u64,
    /// The account, for providers whose tokens name one.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) account: Option<String>,
}

impl Tokens {
    /// Tokens from an OAuth token response. A refresh response may leave out
    /// the refresh token, which then stays `previous`.
    fn issued(oauth: &OAuth, response: &Value, previous: Option<&str>) -> Result<Self, String> {
        let access =
            response["access_token"].as_str().ok_or("the token response has no access token")?;
        let refresh = response["refresh_token"]
            .as_str()
            .or(previous)
            .ok_or("the token response has no refresh token")?;
        let account = oauth
            .account
            .as_ref()
            .map(|account| {
                account_id(access, account.claim).ok_or("the access token names no account")
            })
            .transpose()?;
        Ok(Self {
            access: access.to_owned(),
            refresh: refresh.to_owned(),
            expires: now().saturating_add(response["expires_in"].as_u64().unwrap_or(3600)),
            account,
        })
    }
}

/// What signing in produces, as `auth.json` stores it: a key as a string, or
/// tokens as an object.
#[derive(Debug, Deserialize, PartialEq, Serialize)]
#[serde(untagged)]
pub(crate) enum Credential {
    Key(String),
    Tokens(Tokens),
}

/// How requests authenticate.
#[derive(Clone)]
pub(crate) enum Auth {
    None,
    Key(String),
    Subscription(Arc<Subscription>),
}

/// Credentials are equal when they are the same key or the same sign-in.
impl PartialEq for Auth {
    fn eq(&self, other: &Self) -> bool {
        match (self, other) {
            (Self::None, Self::None) => true,
            (Self::Key(key), Self::Key(other)) => key == other,
            (Self::Subscription(sign_in), Self::Subscription(other)) => Arc::ptr_eq(sign_in, other),
            _ => false,
        }
    }
}

impl Auth {
    /// The headers that authenticate a request. A subscription's access token
    /// is refreshed first when it is about to expire, on the calling thread.
    pub(crate) fn headers(
        &self,
        http: &ureq::Agent,
    ) -> Result<Vec<(&'static str, String)>, Unavailable> {
        match self {
            Self::None => Ok(Vec::new()),
            Self::Key(key) => Ok(vec![("authorization", format!("Bearer {key}"))]),
            Self::Subscription(subscription) => {
                let tokens = subscription.tokens(http)?;
                let mut headers = vec![("authorization", format!("Bearer {}", tokens.access))];
                if let (Some(account), Some(id)) = (&subscription.oauth.account, tokens.account) {
                    headers.push((account.header, id));
                }
                Ok(headers)
            }
        }
    }
}

/// A subscription sign-in shared by the requests of one process.
pub(crate) struct Subscription {
    provider: Provider,
    oauth: &'static OAuth,
    tokens: Mutex<Tokens>,
    home: PathBuf,
    /// Set once the backend rejected the access token.
    rejected: AtomicBool,
}

/// Why a subscription request has no access token.
#[derive(Debug)]
pub(crate) enum Unavailable {
    /// Only signing in again helps.
    SignIn(String),
    Transient(String),
}

impl Subscription {
    /// A sign-in to `provider`, or `None` for a provider without subscriptions.
    pub(crate) fn new(provider: Provider, tokens: Tokens, home: PathBuf) -> Option<Self> {
        let Access::Subscription(oauth) = &provider.spec().access else {
            return None;
        };
        Some(Self {
            provider,
            oauth,
            tokens: Mutex::new(tokens),
            home,
            rejected: AtomicBool::new(false),
        })
    }

    /// Makes the next request refresh the access token.
    pub(crate) fn reject(&self) {
        self.rejected.store(true, Ordering::Relaxed);
    }

    /// The tokens to send, refreshed first when about to expire. Runs on request
    /// threads; the lock makes concurrent requests share one refresh.
    fn tokens(&self, http: &ureq::Agent) -> Result<Tokens, Unavailable> {
        let mut tokens = self.tokens.lock().unwrap_or_else(PoisonError::into_inner);
        let rejected = self.rejected.swap(false, Ordering::Relaxed);
        if !rejected && tokens.expires > now() + REFRESH_MARGIN {
            return Ok(tokens.clone());
        }
        // A refresh token may work only once, so agt processes sharing a
        // sign-in take turns, and one that waited adopts the tokens the other
        // saved.
        let _lock = config::lock_credentials(&self.home);
        if let Some(saved) = config::saved_tokens(&self.home, self.provider)
            && saved.access != tokens.access
            && saved.expires > now() + REFRESH_MARGIN
        {
            *tokens = saved;
            return Ok(tokens.clone());
        }
        let response = http.post(self.oauth.token).send_form([
            ("grant_type", "refresh_token"),
            ("refresh_token", tokens.refresh.as_str()),
            ("client_id", self.oauth.client),
        ]);
        let name = self.provider.spec().name;
        let fresh = match llm::json_body(response) {
            Ok(body) => Tokens::issued(self.oauth, &body, Some(&tokens.refresh))
                .map_err(Unavailable::SignIn)?,
            Err(ExchangeError { status: Some(400 | 401), .. }) => {
                return Err(Unavailable::SignIn(format!(
                    "the {name} sign-in has expired; sign in again with /login in agt"
                )));
            }
            Err(error) => {
                return Err(Unavailable::Transient(format!(
                    "cannot refresh the {name} sign-in: {}",
                    error.message
                )));
            }
        };
        // A process that cannot save still works; others then sign in again.
        let _ =
            config::save_credential(&self.home, self.provider, &Credential::Tokens(fresh.clone()));
        *tokens = fresh.clone();
        Ok(fresh)
    }
}

/// Reports how a sign-in ended, from any thread.
pub(crate) type Done = Arc<dyn Fn(Result<Credential, String>) + Send + Sync>;

/// A browser sign-in, finished by the redirect reaching agt or by pasting the
/// address the browser ended on. Dropping it stops listening.
pub(crate) struct Login {
    /// The page to sign in on.
    pub(crate) url: String,
    flow: Arc<Flow>,
    address: SocketAddr,
    cancelled: Arc<AtomicBool>,
    done: Done,
}

struct Flow {
    provider: Provider,
    verifier: String,
    state: String,
    redirect: String,
    path: &'static str,
}

impl Login {
    /// Starts signing in to `provider`, listening for the browser the caller
    /// sends to `url`.
    pub(crate) fn start(provider: Provider, done: Done) -> Result<Self, String> {
        let redirect = match &provider.spec().access {
            Access::Subscription(oauth) => oauth.redirect,
            Access::Key { .. } => Redirect::LOOPBACK,
        };
        let port = redirect.port;
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, port))
            .map_err(|error| format!("cannot listen for the sign-in on port {port}: {error}"))?;
        let address = listener.local_addr().map_err(|error| error.to_string())?;
        let callback = redirect.url(address.port());
        let verifier = random(32)?;
        let state = random(16)?;
        let url = page(provider, &callback, &challenge(&verifier), &state)
            .ok_or_else(|| format!("{} has no browser sign-in", provider.spec().name))?;
        let flow =
            Arc::new(Flow { provider, verifier, state, redirect: callback, path: redirect.path });
        let cancelled = Arc::new(AtomicBool::new(false));
        let waiter = {
            let (flow, cancelled, done) =
                (Arc::clone(&flow), Arc::clone(&cancelled), Arc::clone(&done));
            move || {
                for stream in listener.incoming() {
                    if cancelled.load(Ordering::Relaxed) {
                        return;
                    }
                    if let Ok(stream) = stream
                        && let Some(code) = flow.callback(stream)
                    {
                        return done(code.and_then(|code| flow.exchange(&code)));
                    }
                }
            }
        };
        thread::Builder::new()
            .name("agt-sign-in".into())
            .spawn(waiter)
            .map_err(|error| error.to_string())?;
        Ok(Self { url, flow, address, cancelled, done })
    }

    /// Finishes with the address the browser ended on, or just its code.
    pub(crate) fn paste(&self, text: &str) {
        let (flow, done) = (Arc::clone(&self.flow), Arc::clone(&self.done));
        let code = pasted_code(text, &flow.state);
        let spawned = thread::Builder::new()
            .name("agt-sign-in".into())
            .spawn(move || done(code.and_then(|code| flow.exchange(&code))));
        if let Err(error) = spawned {
            (self.done)(Err(format!("cannot finish signing in: {error}")));
        }
    }
}

impl Drop for Login {
    fn drop(&mut self) {
        self.cancelled.store(true, Ordering::Relaxed);
        // Wakes the listener so it closes the port.
        let _ = TcpStream::connect_timeout(&self.address, Duration::from_secs(1));
    }
}

impl Flow {
    /// Answers a request to the loopback server. Returns the code, or why
    /// signing in failed, once the redirect arrives.
    fn callback(&self, stream: TcpStream) -> Option<Result<String, String>> {
        let _ = stream.set_read_timeout(Some(Duration::from_secs(5)));
        let mut reader = BufReader::new((&stream).take(16 * 1024));
        let mut line = String::new();
        reader.read_line(&mut line).ok()?;
        // The rest of the request is read, since closing a socket with unread
        // input can reset it before the browser gets the page.
        let mut header = String::new();
        while reader.read_line(&mut header).is_ok_and(|read| read > 2) {
            header.clear();
        }
        let target = line.split(' ').nth(1).unwrap_or_default();
        let (path, query) = target.split_once('?').unwrap_or((target, ""));
        let mut writer = &stream;
        if path != self.path {
            let _ = writer.write_all(
                b"HTTP/1.1 404 Not Found\r\ncontent-length: 0\r\nconnection: close\r\n\r\n",
            );
            return None;
        }
        let code = code(query, &self.state);
        let page = match &code {
            Ok(_) => "Signed in. You can close this tab and return to agt.",
            Err(error) => error.as_str(),
        };
        let _ = write!(
            writer,
            "HTTP/1.1 200 OK\r\ncontent-type: text/plain; charset=utf-8\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{page}",
            page.len()
        );
        Some(code)
    }

    /// Trades an authorization code for the credential.
    fn exchange(&self, code: &str) -> Result<Credential, String> {
        let http = llm::http(Duration::from_secs(30));
        let failed = |error: ExchangeError| format!("sign-in failed: {}", error.message);
        let spec = self.provider.spec();
        let keys = match &spec.access {
            Access::Subscription(oauth) => {
                let response = http.post(oauth.token).send_form([
                    ("grant_type", "authorization_code"),
                    ("client_id", oauth.client),
                    ("code", code),
                    ("code_verifier", self.verifier.as_str()),
                    ("redirect_uri", self.redirect.as_str()),
                ]);
                let body = llm::json_body(response).map_err(failed)?;
                return Tokens::issued(oauth, &body, None).map(Credential::Tokens);
            }
            Access::Key { sign_in, .. } => {
                sign_in
                    .as_ref()
                    .ok_or_else(|| format!("{} has no browser sign-in", spec.name))?
                    .keys
            }
        };
        let body = json!({
            "code": code,
            "code_verifier": self.verifier,
            "code_challenge_method": "S256",
        });
        let response =
            http.post(keys).header("content-type", "application/json").send(body.to_string());
        let body = llm::json_body(response).map_err(failed)?;
        body["key"]
            .as_str()
            .filter(|key| !key.is_empty())
            .map(|key| Credential::Key(key.to_owned()))
            .ok_or_else(|| format!("{} issued no key", spec.name))
    }
}

/// The PKCE challenge for `verifier` (RFC 7636, S256).
fn challenge(verifier: &str) -> String {
    URL_SAFE_NO_PAD.encode(digest(&SHA256, verifier.as_bytes()))
}

/// A URL-safe random token with `bytes` bytes of entropy.
fn random(bytes: usize) -> Result<String, String> {
    let mut buffer = vec![0; bytes];
    SystemRandom::new()
        .fill(&mut buffer)
        .map_err(|_| "no secure random numbers are available".to_owned())?;
    Ok(URL_SAFE_NO_PAD.encode(buffer))
}

/// The page that signs in to `provider` and returns to `callback`, when the
/// provider has a browser sign-in.
fn page(provider: Provider, callback: &str, challenge: &str, state: &str) -> Option<String> {
    let url = match &provider.spec().access {
        Access::Subscription(oauth) => {
            let mut query = vec![
                ("response_type", "code"),
                ("client_id", oauth.client),
                ("redirect_uri", callback),
                ("scope", oauth.scope),
                ("code_challenge", challenge),
                ("code_challenge_method", "S256"),
                ("state", state),
            ];
            query.extend_from_slice(oauth.params);
            with_query(oauth.authorize, &query)
        }
        Access::Key { sign_in, .. } => with_query(
            sign_in.as_ref()?.authorize,
            &[
                ("callback_url", callback),
                ("code_challenge", challenge),
                ("code_challenge_method", "S256"),
            ],
        ),
    };
    Some(url)
}

fn with_query(base: &str, pairs: &[(&str, &str)]) -> String {
    let query: Vec<String> = pairs
        .iter()
        .map(|(key, value)| format!("{key}={}", utf8_percent_encode(value, NON_ALPHANUMERIC)))
        .collect();
    format!("{base}?{}", query.join("&"))
}

/// The authorization code in a redirect's query, checked against `state`.
fn code(query: &str, state: &str) -> Result<String, String> {
    let mut code = None;
    let mut error = None;
    for pair in query.split('&') {
        let (key, value) = pair.split_once('=').unwrap_or((pair, ""));
        let value = percent_decode_str(&value.replace('+', " ")).decode_utf8_lossy().into_owned();
        match key {
            "code" => code = Some(value),
            "state" if value != state => {
                return Err("the sign-in belongs to another attempt; try again".into());
            }
            "error_description" => error = Some(value),
            "error" => {
                error.get_or_insert(value);
            }
            _ => {}
        }
    }
    if let Some(error) = error {
        return Err(format!("sign-in failed: {error}"));
    }
    code.filter(|code| !code.is_empty())
        .ok_or_else(|| "the address has no authorization code".into())
}

/// The code in pasted text: the address the browser ended on, or the code.
fn pasted_code(text: &str, state: &str) -> Result<String, String> {
    let text = text.trim();
    if text.contains("code=") {
        let query = text.split_once('?').map_or(text, |(_, query)| query);
        return code(query.split('#').next().unwrap_or_default(), state);
    }
    if text.is_empty() || text.contains(char::is_whitespace) {
        return Err("paste the address the browser ended on".into());
    }
    Ok(text.to_owned())
}

/// The account an access token names, at the JSON pointer `claim` among its
/// JWT claims.
fn account_id(token: &str, claim: &str) -> Option<String> {
    let claims = token.split('.').nth(1)?;
    let claims: Value = serde_json::from_slice(&URL_SAFE_NO_PAD.decode(claims).ok()?).ok()?;
    claims.pointer(claim)?.as_str().filter(|id| !id.is_empty()).map(str::to_owned)
}

fn now() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |elapsed| elapsed.as_secs())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The OAuth sign-in of a subscription provider.
    fn oauth(provider: Provider) -> &'static OAuth {
        match &provider.spec().access {
            Access::Subscription(oauth) => oauth,
            Access::Key { .. } => panic!("{provider:?} has no subscription"),
        }
    }

    #[test]
    fn pkce_challenges_match_the_rfc_example() {
        assert_eq!(
            challenge("dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk"),
            "E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM"
        );
        let token = random(32).expect("random");
        assert_eq!(token.len(), 43, "32 bytes in unpadded base64: {token}");
        assert_ne!(token, random(32).expect("random"));
    }

    #[test]
    fn sign_in_pages_carry_each_providers_parameters() {
        // Each page carries exactly the parameters its provider's sign-in takes.
        let chatgpt = oauth(Provider::Codex).redirect.url(1455);
        assert_eq!(chatgpt, "http://localhost:1455/auth/callback");
        assert_eq!(
            page(Provider::Codex, &chatgpt, "abc", "xyz").as_deref(),
            Some(concat!(
                "https://auth.openai.com/oauth/authorize?response_type=code",
                "&client_id=app%5FEMoamEEZ73f0CkXaXp7hrann",
                "&redirect_uri=http%3A%2F%2Flocalhost%3A1455%2Fauth%2Fcallback",
                "&scope=openid%20profile%20email%20offline%5Faccess",
                "&code_challenge=abc&code_challenge_method=S256&state=xyz",
                "&id_token_add_organizations=true&codex_cli_simplified_flow=true&originator=agt",
            ))
        );
        let loopback = Redirect::LOOPBACK.url(5555);
        assert_eq!(loopback, "http://127.0.0.1:5555/callback");
        assert_eq!(
            page(Provider::Grok, &loopback, "abc", "xyz").as_deref(),
            Some(concat!(
                "https://auth.x.ai/oauth2/authorize?response_type=code",
                "&client_id=b1a00492%2D073a%2D47ea%2D816f%2D4c329264a828",
                "&redirect_uri=http%3A%2F%2F127%2E0%2E0%2E1%3A5555%2Fcallback",
                "&scope=openid%20profile%20email%20offline%5Faccess%20grok%2Dcli%3Aaccess%20api%3Aaccess",
                "&code_challenge=abc&code_challenge_method=S256&state=xyz&referrer=agt",
            ))
        );
        assert_eq!(
            page(Provider::OpenRouter, &loopback, "abc", "xyz").as_deref(),
            Some(concat!(
                "https://openrouter.ai/auth?callback_url=http%3A%2F%2F127%2E0%2E0%2E1%3A5555%2Fcallback",
                "&code_challenge=abc&code_challenge_method=S256",
            ))
        );
        assert_eq!(page(Provider::Vercel, &loopback, "abc", "xyz"), None);
    }

    #[test]
    fn redirects_give_their_code_or_their_error() {
        assert_eq!(code("code=ab%2Fc&state=s1", "s1"), Ok("ab/c".into()));
        assert_eq!(
            code("code=x&state=other", "s1"),
            Err("the sign-in belongs to another attempt; try again".into())
        );
        assert_eq!(
            code("error=access_denied&error_description=User+denied", "s1"),
            Err("sign-in failed: User denied".into())
        );
        assert_eq!(code("state=s1", "s1"), Err("the address has no authorization code".into()));
        assert_eq!(
            pasted_code(" http://localhost:1455/auth/callback?code=c1&state=s1#done ", "s1"),
            Ok("c1".into())
        );
        assert_eq!(pasted_code("c2", "s1"), Ok("c2".into()));
        for text in ["", "two words"] {
            let error = Err("paste the address the browser ended on".into());
            assert_eq!(pasted_code(text, "s1"), error, "{text:?}");
        }
    }

    #[test]
    fn token_responses_become_tokens() {
        let chatgpt = oauth(Provider::Codex);
        let claims = URL_SAFE_NO_PAD
            .encode(r#"{"https://api.openai.com/auth":{"chatgpt_account_id":"acct-7"}}"#);
        let access = format!("header.{claims}.signature");
        let response = json!({ "access_token": access, "refresh_token": "r1", "expires_in": 600 });
        let tokens = Tokens::issued(chatgpt, &response, None).expect("tokens");
        assert_eq!((tokens.refresh.as_str(), tokens.account.as_deref()), ("r1", Some("acct-7")));
        assert!(tokens.expires >= now() + 599, "expires {} is not 600 s away", tokens.expires);

        // A refresh that leaves out the refresh token keeps the one it used.
        let refresh = json!({ "access_token": access });
        let refreshed = Tokens::issued(chatgpt, &refresh, Some("r0")).expect("refreshed");
        assert_eq!(refreshed.refresh, "r0");
        let error = Tokens::issued(chatgpt, &refresh, None).err();
        assert_eq!(error.as_deref(), Some("the token response has no refresh token"));
        let opaque = json!({ "access_token": "a.b.c", "refresh_token": "r" });
        let error = Tokens::issued(chatgpt, &opaque, None).err();
        assert_eq!(error.as_deref(), Some("the access token names no account"));

        // Grok's tokens name no account, and each refresh brings a new refresh token.
        let response = json!({ "access_token": "a.b.c", "refresh_token": "r2", "expires_in": 60 });
        let grok = Tokens::issued(oauth(Provider::Grok), &response, Some("r1")).expect("tokens");
        assert_eq!((grok.refresh.as_str(), grok.account), ("r2", None));
    }

    #[test]
    fn requests_carry_the_access_token_and_account() {
        let tokens = Tokens {
            access: "a1".into(),
            refresh: "r".into(),
            expires: now() + 3600,
            account: Some("acct-7".into()),
        };
        let subscription = Subscription::new(Provider::Codex, tokens, PathBuf::from("/home/.agt"))
            .expect("a subscription");
        let http = ureq::Agent::new_with_defaults();
        let headers = Auth::Subscription(Arc::new(subscription)).headers(&http).expect("headers");
        let expected = [
            ("authorization", "Bearer a1".to_owned()),
            ("chatgpt-account-id", "acct-7".to_owned()),
        ];
        assert_eq!(headers, expected);
        let headers = Auth::Key("sk-1".into()).headers(&http).expect("headers");
        assert_eq!(headers, [("authorization", "Bearer sk-1".to_owned())]);
    }

    #[test]
    fn credentials_round_trip_through_auth_json() {
        let tokens = Tokens { access: "a".into(), refresh: "r".into(), expires: 9, account: None };
        let saved = serde_json::to_value(Credential::Tokens(tokens.clone())).expect("tokens");
        assert_eq!(saved, json!({ "access": "a", "refresh": "r", "expires": 9 }));
        assert_eq!(Credential::deserialize(&saved).ok(), Some(Credential::Tokens(tokens)));
        let key = Credential::deserialize(&json!("sk-1")).ok();
        assert_eq!(key, Some(Credential::Key("sk-1".into())));
        let partial = Credential::deserialize(&json!({ "access": "a" }));
        assert!(partial.is_err(), "incomplete tokens are no credential: {partial:?}");
    }

    #[test]
    fn the_loopback_server_answers_the_browser() {
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).expect("listen");
        let address = listener.local_addr().expect("address");
        let flow = Flow {
            provider: Provider::OpenRouter,
            verifier: "v".into(),
            state: "s1".into(),
            redirect: String::new(),
            path: "/callback",
        };
        let browser = thread::spawn(move || {
            ["/favicon.ico", "/callback?code=c9&state=s1"].map(|target| {
                let mut stream = TcpStream::connect(address).expect("connect");
                write!(stream, "GET {target} HTTP/1.1\r\nHost: localhost\r\n\r\n").expect("send");
                let mut response = String::new();
                stream.read_to_string(&mut response).expect("response");
                response
            })
        });
        let mut incoming = listener.incoming();
        let mut next = || incoming.next().expect("request").expect("stream");
        assert_eq!(flow.callback(next()), None);
        assert_eq!(flow.callback(next()), Some(Ok("c9".into())));
        let [missing, signed_in] = browser.join().expect("browser");
        assert!(missing.starts_with("HTTP/1.1 404"), "{missing}");
        assert!(signed_in.ends_with("return to agt."), "{signed_in}");
    }

    #[test]
    fn a_sign_in_another_process_refreshed_is_adopted() {
        let home = tempfile::tempdir().expect("temp dir");
        let expired =
            Tokens { access: "old".into(), refresh: "r0".into(), expires: 0, account: None };
        let current = Tokens {
            access: "new".into(),
            refresh: "r1".into(),
            expires: now() + 3600,
            account: None,
        };
        config::save_credential(home.path(), Provider::Grok, &Credential::Tokens(current.clone()))
            .expect("save");
        let session = Subscription::new(Provider::Grok, expired, home.path().to_path_buf())
            .expect("a subscription");
        let http = ureq::Agent::new_with_defaults();
        assert_eq!(session.tokens(&http).ok(), Some(current));
    }
}
