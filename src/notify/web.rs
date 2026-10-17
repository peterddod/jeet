//! What the backends that send over HTTP (`slack`, `ntfy`) share: one
//! JSON call through `ureq`, and what each keeps of how its service has
//! answered, beside the outbox.

use std::fs::OpenOptions;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{anyhow, Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::{dir, open_owned, Down, Said};

/// How long one call is given.
const TIMEOUT: Duration = Duration::from_secs(20);

/// One call to a service: `body` POSTed as JSON to `path`, with `token`
/// as a bearer token if there is one, answered by the service's JSON.
pub trait Http {
    fn call(&self, path: &str, token: Option<&str>, body: &Value) -> Result<Value>;
}

/// A service itself, through `ureq`, at `base`: `service` names it in
/// errors.
pub struct Ureq {
    pub base: String,
    pub service: &'static str,
}

/// An answer that is not a success: an error [`Ureq`] returns, for a
/// backend to tell a refused token from a refused message.
#[derive(Debug)]
pub struct Status {
    pub code: u16,
    /// The `error` of a JSON answer, if it has one.
    pub error: Option<String>,
    /// A 429's `Retry-After`.
    pub retry_after: Option<String>,
}

impl std::fmt::Display for Status {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self.code {
            429 => write!(
                f,
                "rate limited (retry after {}s)",
                self.retry_after.as_deref().unwrap_or("?")
            )?,
            code => write!(f, "HTTP {code}")?,
        }
        match &self.error {
            Some(error) => write!(f, ": {error}"),
            None => Ok(()),
        }
    }
}

impl std::error::Error for Status {}

impl Http for Ureq {
    fn call(&self, path: &str, token: Option<&str>, body: &Value) -> Result<Value> {
        let what = match path {
            "" => self.service.to_string(),
            path => format!("{}'s {path}", self.service),
        };
        let agent: ureq::Agent = ureq::Agent::config_builder()
            .timeout_global(Some(TIMEOUT))
            .http_status_as_error(false)
            .build()
            .into();
        let mut request = agent.post(format!("{}{path}", self.base));
        if let Some(token) = token {
            request = request.header("Authorization", format!("Bearer {token}"));
        }
        let mut response = request
            .send_json(body)
            .with_context(|| format!("call {what}"))?;
        let status = response.status();
        if !status.is_success() {
            let retry_after = response
                .headers()
                .get("retry-after")
                .and_then(|v| v.to_str().ok())
                .map(str::to_string);
            let error = response
                .body_mut()
                .read_json::<Value>()
                .ok()
                .and_then(|v| v["error"].as_str().map(str::to_string));
            return Err(anyhow!(Status {
                code: status.as_u16(),
                error,
                retry_after,
            })
            .context(what));
        }
        response
            .body_mut()
            .read_json()
            .with_context(|| format!("read {what}'s answer"))
    }
}

/// No network: what the test binary has in place of [`Ureq`].
#[cfg(test)]
pub struct Offline;

#[cfg(test)]
impl Http for Offline {
    fn call(&self, path: &str, _: Option<&str>, _: &Value) -> Result<Value> {
        anyhow::bail!("the tests call no service ({path})")
    }
}

// ---------------------------------------------------------------------------
// What is kept
// ---------------------------------------------------------------------------

/// What the sender has learnt of a backend's service.
#[derive(Debug, Default, Serialize, Deserialize)]
pub struct State {
    /// `false` once the service refused a message for its `jeet://` link:
    /// until then messages go with one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub jeet_links: Option<bool>,
    /// A missing or refused token has been logged, and is not again until
    /// a send goes through.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub bad_token_said: bool,
}

/// `$JEET_HOME/run/notify/<backend>.json`.
fn state_path(home: &Path, backend: &str) -> PathBuf {
    dir(home).join(format!("{backend}.json"))
}

pub fn load(home: &Path, backend: &str) -> State {
    let Ok(mut file) = open_owned(&state_path(home, backend), OpenOptions::new().read(true)) else {
        return State::default();
    };
    let mut text = String::new();
    if file.read_to_string(&mut text).is_err() {
        return State::default();
    }
    serde_json::from_str(&text).unwrap_or_default()
}

pub fn save(home: &Path, backend: &str, state: &State) -> Result<()> {
    let tmp = dir(home).join(format!("{backend}.json.tmp"));
    let _ = std::fs::remove_file(&tmp);
    let mut file = open_owned(&tmp, OpenOptions::new().create_new(true).write(true))?;
    file.write_all(serde_json::to_string(&state)?.as_bytes())?;
    let path = state_path(home, backend);
    std::fs::rename(&tmp, &path).with_context(|| format!("replace {}", path.display()))
}

/// The error for `backend`'s token that is missing or refused: logged the
/// first time, then [`Said`], which is not, until a send goes through
/// ([`token_works`]).
pub fn bad_token(home: &Path, backend: &str, message: &str) -> anyhow::Error {
    let mut state = load(home, backend);
    if state.bad_token_said {
        return anyhow!(Said);
    }
    state.bad_token_said = true;
    if let Err(e) = save(home, backend, &state) {
        eprintln!("jeet: could not save that the {backend} token was refused: {e:#}");
    }
    anyhow!(Down(anyhow!("{message}")))
}

/// A send through `backend` went through: a bad token is logged again.
pub fn token_works(home: &Path, backend: &str) {
    let mut state = load(home, backend);
    if state.bad_token_said {
        state.bad_token_said = false;
        if let Err(e) = save(home, backend, &state) {
            eprintln!("jeet: could not save that the {backend} token works: {e:#}");
        }
    }
}

/// `backend`'s service refused a `jeet://` link: none is sent again.
pub fn refuse_jeet_links(home: &Path, backend: &str) {
    let mut state = load(home, backend);
    state.jeet_links = Some(false);
    if let Err(e) = save(home, backend, &state) {
        eprintln!("jeet: could not save that: {e:#}");
    }
}

#[cfg(test)]
pub mod tests {
    use super::*;
    use serde_json::json;
    use std::io::BufRead;

    /// One HTTP exchange on a local port: its base url, and, once it is
    /// over, what was asked.
    pub fn serve_once(answer: &'static str) -> (String, std::thread::JoinHandle<String>) {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let base = format!("http://{}/api/", listener.local_addr().unwrap());
        let handle = std::thread::spawn(move || {
            let (stream, _) = listener.accept().unwrap();
            let mut reader = std::io::BufReader::new(stream.try_clone().unwrap());
            let mut head = String::new();
            let mut length = 0;
            loop {
                let mut line = String::new();
                reader.read_line(&mut line).unwrap();
                if let Some(n) = line.to_ascii_lowercase().strip_prefix("content-length:") {
                    length = n.trim().parse().unwrap();
                }
                head.push_str(&line);
                if line == "\r\n" {
                    break;
                }
            }
            let mut body = vec![0; length];
            reader.read_exact(&mut body).unwrap();
            let mut stream = stream;
            stream.write_all(answer.as_bytes()).unwrap();
            head + &String::from_utf8(body).unwrap()
        });
        (base, handle)
    }

    fn ureq(base: String) -> Ureq {
        Ureq {
            base,
            service: "Slack",
        }
    }

    #[test]
    fn ureq_posts_json_with_the_bearer_token() {
        let (base, asked) = serve_once(
            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: 11\r\nConnection: close\r\n\r\n{\"ok\":true}",
        );
        let answer = ureq(base)
            .call(
                "chat.postMessage",
                Some("xoxb-9"),
                &json!({ "channel": "D1" }),
            )
            .unwrap();
        assert_eq!(answer, json!({ "ok": true }));
        let asked = asked.join().unwrap();
        assert!(asked.starts_with("POST /api/chat.postMessage "), "{asked}");
        let lower = asked.to_ascii_lowercase();
        assert!(
            lower.contains("authorization: bearer xoxb-9\r\n"),
            "{asked}"
        );
        assert!(lower.contains("content-type: application/json"), "{asked}");
        let body = &asked[asked.find("\r\n\r\n").unwrap() + 4..];
        assert_eq!(
            serde_json::from_str::<Value>(body).unwrap(),
            json!({ "channel": "D1" })
        );
    }

    #[test]
    fn ureq_sends_no_authorization_without_a_token() {
        let (base, asked) = serve_once(
            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: 2\r\nConnection: close\r\n\r\n{}",
        );
        ureq(base).call("", None, &json!({})).unwrap();
        let asked = asked.join().unwrap();
        assert!(asked.starts_with("POST /api/ "), "{asked}");
        assert!(
            !asked.to_ascii_lowercase().contains("authorization"),
            "{asked}"
        );
    }

    #[test]
    fn ureq_fails_on_rate_limits_and_http_errors() {
        let (base, asked) = serve_once(
            "HTTP/1.1 429 Too Many Requests\r\nRetry-After: 30\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
        );
        let e = ureq(base)
            .call("conversations.open", Some("t"), &json!({}))
            .unwrap_err();
        assert!(
            format!("{e:#}").contains("Slack's conversations.open: rate limited (retry after 30s)"),
            "{e:#}"
        );
        assert_eq!(e.downcast_ref::<Status>().unwrap().code, 429);
        asked.join().unwrap();
        let (base, asked) = serve_once(
            "HTTP/1.1 503 Service Unavailable\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
        );
        let e = ureq(base)
            .call("conversations.open", Some("t"), &json!({}))
            .unwrap_err();
        assert!(format!("{e:#}").contains("HTTP 503"), "{e:#}");
        asked.join().unwrap();
        // A JSON answer's `error` is kept.
        let (base, asked) = serve_once(
            "HTTP/1.1 403 Forbidden\r\nContent-Type: application/json\r\nContent-Length: 39\r\nConnection: close\r\n\r\n{\"code\":40301,\"http\":403,\"error\":\"no\"}\n",
        );
        let e = ureq(base).call("", None, &json!({})).unwrap_err();
        let status = e.downcast_ref::<Status>().unwrap();
        assert_eq!((status.code, status.error.as_deref()), (403, Some("no")));
        assert!(format!("{e:#}").contains("Slack: HTTP 403: no"), "{e:#}");
        asked.join().unwrap();
    }
}
