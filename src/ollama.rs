//! A client for a local [Ollama](https://ollama.com) server.
//!
//! This is the one deliberate exception to "nothing on the network after the
//! model download": a user may run an LLM on their own machine, and the daemon
//! should be able to ask it to rewrite a transcript. The client therefore
//! refuses any base URL whose host is not loopback; there is no configuration
//! that turns it into a cloud client.
//!
//! `curl` is the transport, for the same reason [`crate::model`] uses it for
//! downloads: it is present wherever this program runs, and it keeps an HTTP and
//! TLS dependency out of a daemon that otherwise has neither. Ollama's API is
//! JSON over plain HTTP, so this is a thin wrapper over four endpoints:
//!
//! | Method | Endpoint | What it is for |
//! | --- | --- | --- |
//! | [`Client::version`] | `GET /api/version` | is a server there, and which version |
//! | [`Client::models`] | `GET /api/tags` | the models installed locally |
//! | [`Client::generate`] | `POST /api/generate` | one prompt in, one completion out |
//! | [`Client::chat`] | `POST /api/chat` | a message history in, one reply out |
//!
//! Every request asks for `"stream": false`, so the response is one JSON
//! document rather than a token stream. Streaming belongs to the feature that
//! consumes the text, not to the transport.

use std::io::Write;
use std::process::{Command, Stdio};
use std::time::Duration;

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};

/// Where a default Ollama install listens.
pub const DEFAULT_BASE: &str = "http://127.0.0.1:11434";

/// How long a request waits before curl gives up.
pub const DEFAULT_TIMEOUT_SECONDS: u64 = 30;

/// A request against a local Ollama server.
#[derive(Debug, Clone)]
pub struct Client {
    base: String,
    timeout: Duration,
}

/// One message in a chat history.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Message {
    pub role: String,
    pub content: String,
}

impl Message {
    pub fn system(content: impl Into<String>) -> Self {
        Self {
            role: "system".to_string(),
            content: content.into(),
        }
    }

    pub fn user(content: impl Into<String>) -> Self {
        Self {
            role: "user".to_string(),
            content: content.into(),
        }
    }

    pub fn assistant(content: impl Into<String>) -> Self {
        Self {
            role: "assistant".to_string(),
            content: content.into(),
        }
    }
}

impl Client {
    /// A client for `base`, with the default timeout.
    ///
    /// `base` is validated here rather than at request time: a host that is
    /// not this machine is a configuration error, and finding it on the first
    /// dictation is too late.
    pub fn new(base: impl Into<String>) -> Result<Self> {
        Self::with_timeout(base, Duration::from_secs(DEFAULT_TIMEOUT_SECONDS))
    }

    /// A client for the default install on this machine.
    pub fn local() -> Self {
        Self {
            base: DEFAULT_BASE.to_string(),
            timeout: Duration::from_secs(DEFAULT_TIMEOUT_SECONDS),
        }
    }

    pub fn with_timeout(base: impl Into<String>, timeout: Duration) -> Result<Self> {
        let base = base.into();
        let base = base.trim().trim_end_matches('/').to_string();
        if base.is_empty() {
            bail!("ollama: empty base URL");
        }
        if !is_loopback(&base) {
            bail!(
                "ollama: {base} is not loopback; this app only talks to a server on this machine"
            );
        }
        Ok(Self {
            base,
            timeout: timeout.max(Duration::from_secs(1)),
        })
    }

    pub fn base(&self) -> &str {
        &self.base
    }

    /// The server's version, which doubles as a reachability check.
    pub fn version(&self) -> Result<String> {
        #[derive(Deserialize)]
        struct Reply {
            version: String,
        }

        let body = self.request("/api/version", None)?;
        let reply: Reply = serde_json::from_str(&body)
            .with_context(|| format!("ollama: unexpected /api/version response: {body}"))?;
        Ok(reply.version)
    }

    /// The names of the models installed on the server.
    pub fn models(&self) -> Result<Vec<String>> {
        #[derive(Deserialize)]
        struct Reply {
            #[serde(default)]
            models: Vec<Entry>,
        }
        #[derive(Deserialize)]
        struct Entry {
            name: String,
        }

        let body = self.request("/api/tags", None)?;
        let reply: Reply = serde_json::from_str(&body)
            .with_context(|| format!("ollama: unexpected /api/tags response: {body}"))?;
        Ok(reply.models.into_iter().map(|entry| entry.name).collect())
    }

    /// Run one prompt through a model and return the completion.
    pub fn generate(&self, model: &str, prompt: &str) -> Result<String> {
        self.generate_with(model, prompt, None)
    }

    /// Run one prompt and require the completion to be JSON.
    ///
    /// `format: "json"` is Ollama's own constraint, not a request in the prompt:
    /// the server keeps the model inside a JSON grammar, which is the difference
    /// between a reply that parses every time and one that parses most of the
    /// time. The caller still parses defensively.
    pub fn generate_json(&self, model: &str, prompt: &str) -> Result<String> {
        self.generate_with(model, prompt, Some("json"))
    }

    fn generate_with(&self, model: &str, prompt: &str, format: Option<&str>) -> Result<String> {
        #[derive(Serialize)]
        struct Request<'a> {
            model: &'a str,
            prompt: &'a str,
            stream: bool,
            #[serde(skip_serializing_if = "Option::is_none")]
            format: Option<&'a str>,
        }
        #[derive(Deserialize)]
        struct Reply {
            #[serde(default)]
            response: String,
        }

        let request = serde_json::to_string(&Request {
            model,
            prompt,
            stream: false,
            format,
        })
        .context("ollama: encode /api/generate request")?;
        let body = self.request("/api/generate", Some(&request))?;
        let reply: Reply = serde_json::from_str(&body)
            .with_context(|| format!("ollama: unexpected /api/generate response: {body}"))?;
        Ok(reply.response)
    }

    /// Continue a chat history and return the assistant's next message.
    pub fn chat(&self, model: &str, messages: &[Message]) -> Result<String> {
        #[derive(Serialize)]
        struct Request<'a> {
            model: &'a str,
            messages: &'a [Message],
            stream: bool,
        }
        #[derive(Deserialize)]
        struct Reply {
            message: Option<Message>,
        }

        let request = serde_json::to_string(&Request {
            model,
            messages,
            stream: false,
        })
        .context("ollama: encode /api/chat request")?;
        let body = self.request("/api/chat", Some(&request))?;
        let reply: Reply = serde_json::from_str(&body)
            .with_context(|| format!("ollama: unexpected /api/chat response: {body}"))?;
        let message = reply
            .message
            .context("ollama: /api/chat response had no message")?;
        Ok(message.content)
    }

    fn request(&self, path: &str, body: Option<&str>) -> Result<String> {
        let url = format!("{}{path}", self.base);
        let mut curl = Command::new("curl");
        curl.args([
            "--silent",
            "--show-error",
            "--fail",
            "--connect-timeout",
            "2",
        ])
        .args(["--max-time", &self.timeout.as_secs().to_string()]);
        if body.is_some() {
            curl.args(["-H", "Content-Type: application/json", "-d", "@-"]);
        }
        curl.arg(&url)
            .stdin(if body.is_some() {
                Stdio::piped()
            } else {
                Stdio::null()
            })
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());

        let mut child = curl
            .spawn()
            .with_context(|| format!("ollama: start curl for {url}"))?;
        if let Some(body) = body {
            let mut stdin = child.stdin.take().context("ollama: curl has no stdin")?;
            stdin
                .write_all(body.as_bytes())
                .context("ollama: write request body to curl")?;
        }

        let output = child
            .wait_with_output()
            .with_context(|| format!("ollama: wait for curl for {url}"))?;
        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            let stderr = stderr.trim();
            let detail = if stderr.is_empty() {
                format!("curl exited with {}", output.status)
            } else {
                stderr.to_string()
            };
            bail!("ollama: {path} failed: {detail}");
        }
        String::from_utf8(output.stdout).context("ollama: response was not UTF-8")
    }
}

/// Whether a base URL names this machine.
///
/// Only `127.0.0.1`, `localhost` and `::1` count. An Ollama install can be
/// bound to `0.0.0.0`, but reaching it by a LAN address would break the
/// promise that a transcript never leaves the host, so that is refused even
/// though it would work.
fn is_loopback(url: &str) -> bool {
    let rest = url
        .strip_prefix("http://")
        .or_else(|| url.strip_prefix("https://"));
    let Some(rest) = rest else {
        return false;
    };
    let authority = rest.split(['/', '?', '#']).next().unwrap_or_default();
    // `user:pass@host` is parsed by curl as a different host, so an `@` that
    // survived this far would let the check pass while the request leaves the
    // machine. Only a bare host and an optional numeric port get through.
    if authority.is_empty() || authority.contains('@') {
        return false;
    }
    let host = if let Some(bracketed) = authority.strip_prefix('[') {
        let Some(end) = bracketed.find(']') else {
            return false;
        };
        let after = &bracketed[end + 1..];
        if !after.is_empty()
            && !(after.starts_with(':') && after[1..].chars().all(|c| c.is_ascii_digit()))
        {
            return false;
        }
        &bracketed[..end]
    } else {
        match authority.split_once(':') {
            Some((host, port)) => {
                if !port.chars().all(|c| c.is_ascii_digit()) {
                    return false;
                }
                host
            }
            None => authority,
        }
    };
    matches!(host, "127.0.0.1" | "localhost" | "::1")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Read;
    use std::net::TcpListener;
    use std::thread;

    #[test]
    fn only_loopback_hosts_are_accepted() {
        for base in [
            "http://127.0.0.1:11434",
            "http://localhost:11434/",
            "https://[::1]:11434",
        ] {
            assert!(Client::new(base).is_ok(), "{base} should be accepted");
        }
        for base in [
            "http://192.168.1.10:11434",
            "http://ollama.example:11434",
            "http://0.0.0.0:11434",
            "ftp://127.0.0.1:11434",
            "",
            // A userinfo field would make curl connect somewhere else.
            "http://127.0.0.1:11434@evil.example",
            "http://127.0.0.1.evil.example:11434",
            "http://[::1]evil:11434",
            "http://localhost.example:11434",
        ] {
            assert!(Client::new(base).is_err(), "{base} should be refused");
        }
    }

    #[test]
    fn a_version_is_read_from_the_server() {
        let base = serve_once(r#"{"version":"0.3.14"}"#);
        let client = Client::new(base).expect("client");
        assert_eq!(client.version().expect("version"), "0.3.14");
    }

    #[test]
    fn a_prompt_returns_the_completion() {
        let base = serve_once(r#"{"model":"tiny","response":"hello","done":true}"#);
        let client = Client::new(base).expect("client");
        assert_eq!(client.generate("tiny", "say hello").expect("text"), "hello");
    }

    #[test]
    fn a_chat_returns_the_assistant_message() {
        let base = serve_once(r#"{"model":"tiny","message":{"role":"assistant","content":"ok"}}"#);
        let client = Client::new(base).expect("client");
        let messages = [Message::user("say ok")];
        assert_eq!(client.chat("tiny", &messages).expect("text"), "ok");
    }

    /// Serve exactly one HTTP request on a loopback port and answer it.
    ///
    /// The client spawns curl, so the test needs a real socket rather than a
    /// mock. The handler does not care what was asked: every call here is
    /// checked by the caller's parser, and a wrong method would still parse the
    /// canned body and fail that check.
    fn serve_once(body: &'static str) -> String {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
        let port = listener.local_addr().expect("addr").port();
        thread::spawn(move || {
            let (mut stream, _) = listener.accept().expect("accept");
            let mut request = [0u8; 8192];
            let _ = stream.read(&mut request);
            let response = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                body.len(),
                body
            );
            let _ = stream.write_all(response.as_bytes());
        });
        format!("http://127.0.0.1:{port}")
    }
}
