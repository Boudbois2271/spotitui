//! Browser login (OAuth authorization code + PKCE) and the stored refresh token.
//!
//! Adapted from fusor-rs/spotify-tui (MIT).

use super::{
    Error,
    secrets::{self, Unlock},
};
use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use rand::{Rng, distr::Alphanumeric};
use reqwest::Url;
use serde::Deserialize;
use sha2::{Digest, Sha256};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::{TcpListener, TcpStream},
};

/// librespot's client ID: Spotify only lets privileged client IDs stream audio.
const CLIENT_ID: &str = "65b708073fc0480ea92a077233ca87bd";
const CALLBACK_ADDRESS: &str = "127.0.0.1:8898";
const CALLBACK_PATH: &str = "/login";
const AUTHORIZE_URL: &str = "https://accounts.spotify.com/authorize";
const TOKEN_URL: &str = "https://accounts.spotify.com/api/token";
const SCOPES: &str = "streaming user-read-private playlist-read-private \
                      playlist-read-collaborative user-library-read user-library-modify";
const VERIFIER_LENGTH: usize = 64;
const STATE_LENGTH: usize = 16;
const MAX_REQUEST_BYTES: usize = 8192;
/// How long a browser connection may take to send its request. Browsers open
/// connections ahead of time that may never carry one.
const REQUEST_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10);
/// How long the login waits for the browser before giving up the port.
const LOGIN_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(600);

pub struct Authorization {
    pub url: String,
    verifier: String,
    state: String,
}

pub struct Token {
    pub access: String,
    refresh: String,
}

#[derive(Deserialize)]
struct TokenResponse {
    access_token: String,
    refresh_token: Option<String>,
}

impl Authorization {
    pub fn new() -> Self {
        let verifier = random_text(VERIFIER_LENGTH);
        let state = random_text(STATE_LENGTH);
        let challenge = URL_SAFE_NO_PAD.encode(Sha256::digest(&verifier));
        let url = Url::parse_with_params(
            AUTHORIZE_URL,
            [
                ("client_id", CLIENT_ID),
                ("response_type", "code"),
                ("redirect_uri", &redirect_uri()),
                ("scope", SCOPES),
                ("state", &state),
                ("code_challenge_method", "S256"),
                ("code_challenge", &challenge),
            ],
        )
        .expect("the authorize URL constant is a valid URL");
        Self {
            url: url.into(),
            verifier,
            state,
        }
    }

    /// Waits for the browser to return to the local callback, then trades the code for a token.
    /// `page` is the HTML the browser shows once it is back.
    pub async fn complete(self, http: &reqwest::Client, page: &str) -> Result<Token, Error> {
        let listener = TcpListener::bind(CALLBACK_ADDRESS).await.map_err(|error| {
            Error::Callback(format!("cannot listen on {CALLBACK_ADDRESS}: {error}"))
        })?;
        let callback = tokio::time::timeout(LOGIN_TIMEOUT, wait_for_callback(&listener))
            .await
            .map_err(|_| {
                Error::Callback("the browser didn't come back in 10 minutes; try again".into())
            })??;
        let (mut stream, callback) = callback;
        respond(&mut stream, page).await?;
        let code = self.code(&callback)?;
        let redirect = redirect_uri();
        Token::request(
            http,
            &[
                ("grant_type", "authorization_code"),
                ("code", &code),
                ("redirect_uri", &redirect),
                ("client_id", CLIENT_ID),
                ("code_verifier", &self.verifier),
            ],
            None,
        )
        .await
        .map_err(|error| match error {
            Error::Unauthorized => Error::Callback("Spotify rejected the login; try again".into()),
            other => other,
        })
    }

    fn code(&self, callback: &Url) -> Result<String, Error> {
        let parameter = |name| {
            callback
                .query_pairs()
                .find(|(key, _)| key == name)
                .map(|(_, value)| value.into_owned())
        };
        if let Some(reason) = parameter("error") {
            return Err(Error::Callback(format!(
                "Spotify declined the login ({reason})"
            )));
        }
        if parameter("state").as_deref() != Some(self.state.as_str()) {
            return Err(Error::Callback(
                "the login response did not match this session".into(),
            ));
        }
        parameter("code").ok_or_else(|| Error::Callback("the login response had no code".into()))
    }
}

impl Token {
    pub async fn refresh(http: &reqwest::Client, refresh_token: &str) -> Result<Self, Error> {
        Self::request(
            http,
            &[
                ("grant_type", "refresh_token"),
                ("refresh_token", refresh_token),
                ("client_id", CLIENT_ID),
            ],
            Some(refresh_token),
        )
        .await
    }

    async fn request(
        http: &reqwest::Client,
        form: &[(&str, &str)],
        previous_refresh: Option<&str>,
    ) -> Result<Self, Error> {
        let response = http.post(TOKEN_URL).form(form).send().await?;
        if let Some(error) = refusal(response.status()) {
            return Err(error);
        }
        let token: TokenResponse = response.json().await?;
        let refresh = token
            .refresh_token
            .or_else(|| previous_refresh.map(str::to_owned))
            .ok_or(Error::Unauthorized)?;
        Ok(Self {
            access: token.access_token,
            refresh,
        })
    }

    /// The refresh token to keep: Spotify may hand out a new one with each refresh.
    pub fn refresh_token(&self) -> &str {
        &self.refresh
    }

    pub async fn store(&self, unlock: Unlock) -> Result<(), Error> {
        secrets::store(&self.refresh, unlock).await
    }
}

/// What a failed answer from the login service means. Only a refused login
/// (400 `invalid_grant`, 401) needs the user; anything else is Spotify having
/// trouble, and trying again later may work.
fn refusal(status: reqwest::StatusCode) -> Option<Error> {
    match status.as_u16() {
        200..=299 => None,
        400 | 401 => Some(Error::Unauthorized),
        code => Some(Error::Accounts(code)),
    }
}

pub async fn store_refresh_token(token: &str, unlock: Unlock) -> Result<(), Error> {
    secrets::store(token, unlock).await
}

pub async fn stored_refresh_token(unlock: Unlock) -> Result<Option<String>, Error> {
    secrets::load(unlock).await
}

pub async fn forget() -> Result<(), Error> {
    secrets::forget().await
}

fn redirect_uri() -> String {
    format!("http://{CALLBACK_ADDRESS}{CALLBACK_PATH}")
}

fn random_text(length: usize) -> String {
    rand::rng()
        .sample_iter(Alphanumeric)
        .take(length)
        .map(char::from)
        .collect()
}

const NOT_FOUND: &[u8] =
    b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n";

/// Accepts browser connections until one brings the login callback. Each is read
/// on its own, with a time limit, so an idle one can't hold up the rest.
async fn wait_for_callback(listener: &TcpListener) -> Result<(TcpStream, Url), Error> {
    let mut reading = tokio::task::JoinSet::new();
    loop {
        tokio::select! {
            accepted = listener.accept() => {
                let (mut stream, _) = accepted?;
                reading.spawn(async move {
                    let callback = tokio::time::timeout(REQUEST_TIMEOUT, read_callback(&mut stream))
                        .await
                        .ok()
                        .and_then(Result::ok)
                        .flatten();
                    (stream, callback)
                });
            }
            Some(Ok((mut stream, callback))) = reading.join_next() => match callback {
                Some(callback) => return Ok((stream, callback)),
                None => {
                    let _ = stream.write_all(NOT_FOUND).await;
                }
            },
        }
    }
}

/// Returns the callback URL, or `None` for unrelated requests such as a favicon.
async fn read_callback(stream: &mut TcpStream) -> Result<Option<Url>, Error> {
    let mut request = Vec::new();
    let mut buffer = [0; 1024];
    while !request.windows(4).any(|window| window == b"\r\n\r\n") {
        let read = stream.read(&mut buffer).await?;
        if read == 0 || request.len() > MAX_REQUEST_BYTES {
            return Ok(None);
        }
        request.extend_from_slice(&buffer[..read]);
    }
    let request = String::from_utf8_lossy(&request);
    let Some(target) = request
        .lines()
        .next()
        .and_then(|line| line.strip_prefix("GET "))
        .and_then(|line| line.split(' ').next())
    else {
        return Ok(None);
    };
    let url = Url::parse(&format!("http://{CALLBACK_ADDRESS}{target}"))
        .map_err(|error| Error::Callback(error.to_string()))?;
    Ok((url.path() == CALLBACK_PATH).then_some(url))
}

async fn respond(stream: &mut TcpStream, page: &str) -> Result<(), Error> {
    let response = format!(
        "HTTP/1.1 200 OK\r\nContent-Type: text/html; charset=utf-8\r\n\
         Content-Length: {}\r\nConnection: close\r\n\r\n{page}",
        page.len()
    );
    stream.write_all(response.as_bytes()).await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{Error, refusal, wait_for_callback};
    use reqwest::StatusCode;
    use std::time::{Duration, Instant};
    use tokio::{
        io::{AsyncReadExt, AsyncWriteExt},
        net::{TcpListener, TcpStream},
    };

    #[tokio::test]
    async fn an_idle_browser_connection_doesnt_hold_up_the_login() {
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("binds");
        let address = listener.local_addr().expect("an address");
        let started = Instant::now();
        let browser = async {
            // A connection opened ahead of time that never sends anything.
            let _idle = TcpStream::connect(address).await.expect("connects");
            let mut favicon = TcpStream::connect(address).await.expect("connects");
            favicon
                .write_all(b"GET /favicon.ico HTTP/1.1\r\n\r\n")
                .await
                .expect("writes");
            let mut answer = Vec::new();
            favicon.read_to_end(&mut answer).await.expect("reads");
            assert!(answer.starts_with(b"HTTP/1.1 404"));
            let mut login = TcpStream::connect(address).await.expect("connects");
            login
                .write_all(b"GET /login?code=c&state=s HTTP/1.1\r\n\r\n")
                .await
                .expect("writes");
            tokio::time::sleep(Duration::from_secs(2)).await;
        };
        let (found, ()) = tokio::join!(wait_for_callback(&listener), browser);
        let (_, url) = found.expect("the callback");
        assert_eq!(url.query(), Some("code=c&state=s"));
        assert!(started.elapsed() < Duration::from_secs(5), "not held up by the idle one");
    }

    #[test]
    fn only_a_refused_login_needs_the_user() {
        assert!(refusal(StatusCode::OK).is_none());
        assert!(matches!(refusal(StatusCode::BAD_REQUEST), Some(Error::Unauthorized)));
        assert!(matches!(refusal(StatusCode::UNAUTHORIZED), Some(Error::Unauthorized)));
        assert!(matches!(
            refusal(StatusCode::TOO_MANY_REQUESTS),
            Some(Error::Accounts(429))
        ));
        assert!(matches!(
            refusal(StatusCode::SERVICE_UNAVAILABLE),
            Some(Error::Accounts(503))
        ));
    }
}
