//! ChatGPT dictation: the private `/backend-api/transcribe` endpoint behind the
//! Dictate button in the ChatGPT and Codex apps, signed in with the login Codex
//! already saved.
//!
//! The login is strictly read-only here. Refresh tokens rotate, so a second
//! program refreshing them can sign Codex out or race its writes; when the
//! token has expired WispX asks the user to let Codex refresh it instead. The
//! token reaches curl through a pipe (never argv, never a file) and curl runs
//! with `--disable` so a user's `.curlrc` cannot redirect or log the request.

use super::TranscribeError;
use crate::config::ChatGptConfig;
use crate::process::{self, RunError, Spec, Toolbox};
use serde_json::Value;
use std::ffi::OsString;
use std::fs;
use std::path::Path;
use std::sync::atomic::AtomicBool;
use std::thread;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

const ENDPOINT: &str = "https://chatgpt.com/backend-api/transcribe";
#[cfg(unix)]
const USER_AGENT: &str = concat!("WispX/", env!("CARGO_PKG_VERSION"), " (Linux)");
#[cfg(windows)]
const USER_AGENT: &str = concat!("WispX/", env!("CARGO_PKG_VERSION"), " (Windows)");
const ORIGINATOR: &str = "codex_desktop";
const EXPIRY_MARGIN_SECS: u64 = 60;
const CONNECT_TIMEOUT_SECS: u64 = 10;
const SIGN_IN: &str = "Sign in to Codex with your ChatGPT account first, then try again";
const EXPIRED: &str = "Codex login expired. Open Codex or run `codex login` to refresh it, \
                       then try again";

/// Where requests go. Only tests use anything but [`Endpoint::production`]; the
/// URL is deliberately not a config setting, since it receives the login token.
pub struct Endpoint {
    url: String,
    https_only: bool,
}

impl Endpoint {
    pub fn production() -> Self {
        Endpoint {
            url: ENDPOINT.to_string(),
            https_only: true,
        }
    }
}

#[derive(Clone, PartialEq, Eq)]
pub struct Auth {
    access_token: String,
    account_id: Option<String>,
}

// Hand-written so a stray `{:?}` can never print the token.
impl std::fmt::Debug for Auth {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Auth { .. }")
    }
}

pub fn precheck(config: &ChatGptConfig, tools: &Toolbox) -> Result<(), String> {
    if !tools.has("curl") {
        return Err("The ChatGPT backend needs curl. Install curl and try again".to_string());
    }
    let auth = read_auth(&config.auth_file)?;
    if token_expired(&auth.access_token, now_secs()) {
        return Err(EXPIRED.to_string());
    }
    Ok(())
}

/// Describes the login for `wispx doctor` without revealing any part of it.
pub fn login_summary(config: &ChatGptConfig) -> Result<String, String> {
    let auth = read_auth(&config.auth_file)?;
    if token_expired(&auth.access_token, now_secs()) {
        return Err(EXPIRED.to_string());
    }
    match jwt_expiry(&auth.access_token) {
        Some(expiry) => Ok(format!(
            "Codex login found; token valid for another {} min",
            expiry.saturating_sub(now_secs()) / 60
        )),
        None => Ok("Codex login found; token expiry unknown".to_string()),
    }
}

/// Reads the login, retrying briefly since Codex may be replacing the file.
pub fn read_auth(path: &Path) -> Result<Auth, String> {
    let mut last_error = String::new();
    for attempt in 0..3 {
        if attempt > 0 {
            thread::sleep(Duration::from_millis(50));
        }
        match fs::read_to_string(path) {
            Ok(text) => match parse_auth(&text) {
                Ok(auth) => return Ok(auth),
                Err(ParseAuthError::NoLogin) => return Err(SIGN_IN.to_string()),
                Err(ParseAuthError::Malformed(message)) => last_error = message,
            },
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                return Err(format!(
                    "{SIGN_IN} ({} not found. If Codex keeps its login in the system \
                     keyring, switch it to file storage or use the local backend)",
                    path.display()
                ));
            }
            Err(error) => return Err(format!("cannot read {}: {error}", path.display())),
        }
    }
    Err(format!("{}: {last_error}", path.display()))
}

#[derive(Debug, PartialEq, Eq)]
enum ParseAuthError {
    NoLogin,
    Malformed(String),
}

fn parse_auth(text: &str) -> Result<Auth, ParseAuthError> {
    // serde_json's messages quote no input values, but keep them out regardless.
    let root: Value = serde_json::from_str(text)
        .map_err(|_| ParseAuthError::Malformed("not valid JSON".to_string()))?;
    let tokens = root.get("tokens").filter(|tokens| !tokens.is_null());
    let access_token = tokens
        .and_then(|t| t.get("access_token"))
        .and_then(Value::as_str)
        .filter(|token| !token.is_empty())
        .ok_or(ParseAuthError::NoLogin)?;
    if !is_header_safe(access_token) {
        return Err(ParseAuthError::Malformed(
            "the access token has an unexpected format".to_string(),
        ));
    }
    let account_id = tokens
        .and_then(|t| t.get("account_id"))
        .and_then(Value::as_str)
        .filter(|id| !id.is_empty())
        .map(str::to_string)
        .or_else(|| jwt_account_id(access_token));
    if account_id.as_deref().is_some_and(|id| !is_header_safe(id)) {
        return Err(ParseAuthError::Malformed(
            "the account id has an unexpected format".to_string(),
        ));
    }
    Ok(Auth {
        access_token: access_token.to_string(),
        account_id,
    })
}

/// Bearer tokens and account ids are single printable ASCII words. Anything else
/// would risk smuggling headers or curl directives, so it is refused.
fn is_header_safe(value: &str) -> bool {
    !value.is_empty() && value.bytes().all(|b| b.is_ascii_graphic())
}

fn base64url_decode(input: &str) -> Option<Vec<u8>> {
    let mut out = Vec::with_capacity(input.len() * 3 / 4);
    let mut buffer = 0u32;
    let mut bits = 0u32;
    for byte in input.bytes().take_while(|b| *b != b'=') {
        let value = match byte {
            b'A'..=b'Z' => byte - b'A',
            b'a'..=b'z' => byte - b'a' + 26,
            b'0'..=b'9' => byte - b'0' + 52,
            b'-' | b'+' => 62,
            b'_' | b'/' => 63,
            _ => return None,
        };
        buffer = (buffer << 6) | u32::from(value);
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push((buffer >> bits) as u8);
            buffer &= (1 << bits) - 1;
        }
    }
    Some(out)
}

fn jwt_claims(token: &str) -> Option<Value> {
    let payload = token.split('.').nth(1)?;
    serde_json::from_slice(&base64url_decode(payload)?).ok()
}

fn jwt_expiry(token: &str) -> Option<u64> {
    jwt_claims(token)?.get("exp")?.as_u64()
}

fn jwt_account_id(token: &str) -> Option<String> {
    jwt_claims(token)?
        .get("https://api.openai.com/auth")?
        .get("chatgpt_account_id")?
        .as_str()
        .map(str::to_string)
}

/// A token whose expiry cannot be read is allowed through, and the server
/// decides. Claims are only used to avoid a doomed upload, never to trust it.
fn token_expired(token: &str, now: u64) -> bool {
    jwt_expiry(token).is_some_and(|expiry| expiry < now + EXPIRY_MARGIN_SECS)
}

fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_secs())
}

/// Quotes a value for curl's config-file syntax.
fn config_quote(value: &str) -> String {
    let mut out = String::with_capacity(value.len() + 2);
    out.push('"');
    for c in value.chars() {
        match c {
            '\\' => out.push_str("\\\\"),
            '"' => out.push_str("\\\""),
            '\t' => out.push_str("\\t"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            other => out.push(other),
        }
    }
    out.push('"');
    out
}

/// Quotes a file name for curl's `--form` syntax, where `;` and `,` end it.
fn form_quote(path: &str) -> String {
    let mut out = String::with_capacity(path.len() + 2);
    out.push('"');
    for c in path.chars() {
        if c == '\\' || c == '"' {
            out.push('\\');
        }
        out.push(c);
    }
    out.push('"');
    out
}

/// The whole request as a curl config document, passed over stdin so that
/// neither the token nor the header reaches a process list.
fn curl_config(
    endpoint: &Endpoint,
    auth: &Auth,
    wav: &Path,
    duration_ms: u64,
    language: &str,
    timeout_secs: u64,
) -> Result<String, TranscribeError> {
    let wav = wav
        .to_str()
        .ok_or_else(|| TranscribeError::Failed("the audio path is not valid UTF-8".to_string()))?;
    let mut lines = vec![
        format!("url = {}", config_quote(&endpoint.url)),
        "request = \"POST\"".to_string(),
        "silent".to_string(),
        "show-error".to_string(),
        "max-redirs = 0".to_string(),
        format!("connect-timeout = {CONNECT_TIMEOUT_SECS}"),
        format!("max-time = {timeout_secs}"),
        format!("user-agent = {}", config_quote(USER_AGENT)),
        format!(
            "header = {}",
            config_quote(&format!("Authorization: Bearer {}", auth.access_token))
        ),
        format!(
            "header = {}",
            config_quote(&format!("originator: {ORIGINATOR}"))
        ),
        format!(
            "form = {}",
            config_quote(&format!(
                "file=@{};type=audio/wav;filename=wispx.wav",
                form_quote(wav)
            ))
        ),
        format!(
            "form-string = {}",
            config_quote(&format!("duration_ms={duration_ms}"))
        ),
        // Curl appends this after the body, so the last line is always ours.
        "write-out = \"\\n%{http_code}\"".to_string(),
    ];
    if endpoint.https_only {
        lines.push("proto = \"=https\"".to_string());
    }
    if let Some(account) = &auth.account_id {
        lines.push(format!(
            "header = {}",
            config_quote(&format!("chatgpt-account-id: {account}"))
        ));
    }
    if !matches!(language, "" | "auto") {
        lines.push(format!(
            "form-string = {}",
            config_quote(&format!("language={language}"))
        ));
    }
    let mut document = lines.join("\n");
    document.push('\n');
    Ok(document)
}

/// Splits curl's stdout into the response body and the status it wrote last.
fn split_status(stdout: &[u8]) -> Option<(u16, &[u8])> {
    let newline = stdout.iter().rposition(|b| *b == b'\n')?;
    let status = std::str::from_utf8(&stdout[newline + 1..]).ok()?;
    Some((status.trim().parse().ok()?, &stdout[..newline]))
}

fn interpret_response(status: u16, body: &[u8]) -> Result<String, TranscribeError> {
    let failed = |message: &str| Err(TranscribeError::Failed(message.to_string()));
    match status {
        200 => {
            let text = serde_json::from_slice::<Value>(body)
                .ok()
                .and_then(|reply| reply.get("text")?.as_str().map(str::to_string));
            match text {
                Some(text) => Ok(text.trim().to_string()),
                None => failed("Unexpected reply from ChatGPT"),
            }
        }
        400 if contains(body, b"audio_too_short") => failed("Too short. Hold a bit longer"),
        401 => failed(EXPIRED),
        403 => failed("ChatGPT refused the request (403). The private endpoint may have changed"),
        413 => failed("Recording too long for ChatGPT"),
        429 => failed("ChatGPT rate limit. Try again soon"),
        other => Err(TranscribeError::Failed(format!("ChatGPT error {other}"))),
    }
}

fn contains(haystack: &[u8], needle: &[u8]) -> bool {
    haystack.windows(needle.len()).any(|part| part == needle)
}

fn explain_curl_failure(code: Option<i32>, stderr: &[u8]) -> String {
    match code {
        Some(5 | 6 | 7 | 35) => "No connection to ChatGPT".to_string(),
        Some(28) => "ChatGPT took too long".to_string(),
        _ => match process::last_error_line(stderr) {
            Some(line) => format!("Request to ChatGPT failed: {line}"),
            None => "Request to ChatGPT failed".to_string(),
        },
    }
}

fn post(
    program: &Path,
    document: &str,
    timeout_secs: u64,
    cancel: &AtomicBool,
) -> Result<(u16, Vec<u8>), TranscribeError> {
    // --disable must come first or curl has already read ~/.curlrc.
    let args = [
        OsString::from("--disable"),
        OsString::from("--config"),
        OsString::from("-"),
    ];
    let mut spec = Spec::new(
        program,
        &args,
        Duration::from_secs(timeout_secs + CONNECT_TIMEOUT_SECS + 5),
    );
    spec.stdin = Some(document.as_bytes());
    spec.cancel = Some(cancel);
    spec.stdout_limit = 1024 * 1024;

    let output = process::run(spec).map_err(|error| match error {
        RunError::Cancelled => TranscribeError::Cancelled,
        RunError::TimedOut(..) => TranscribeError::Failed("ChatGPT took too long".to_string()),
        other => TranscribeError::Failed(other.to_string()),
    })?;
    if !output.status.success() {
        return Err(TranscribeError::Failed(explain_curl_failure(
            output.status.code(),
            &output.stderr,
        )));
    }
    match split_status(&output.stdout) {
        Some((status, body)) => Ok((status, body.to_vec())),
        None => Err(TranscribeError::Failed(
            "Unexpected reply from curl".to_string(),
        )),
    }
}

pub fn transcribe(
    config: &ChatGptConfig,
    endpoint: &Endpoint,
    language: &str,
    tools: &Toolbox,
    wav: &Path,
    duration_ms: u64,
    cancel: &AtomicBool,
) -> Result<String, TranscribeError> {
    let curl = tools.find("curl").ok_or_else(|| {
        TranscribeError::Failed("The ChatGPT backend needs curl. Install curl".to_string())
    })?;
    let auth = read_auth(&config.auth_file).map_err(TranscribeError::Failed)?;
    if token_expired(&auth.access_token, now_secs()) {
        return Err(TranscribeError::Failed(EXPIRED.to_string()));
    }

    let send = |auth: &Auth| {
        let document = curl_config(
            endpoint,
            auth,
            wav,
            duration_ms,
            language,
            config.timeout_secs,
        )?;
        post(&curl, &document, config.timeout_secs, cancel)
    };

    let (mut status, mut body) = send(&auth)?;
    if status == 401 {
        // Codex may have refreshed the file in the meantime; never refresh it
        // ourselves. Retry only if a different, unexpired token is there now.
        if let Ok(fresh) = read_auth(&config.auth_file)
            && fresh != auth
            && !token_expired(&fresh.access_token, now_secs())
        {
            (status, body) = send(&fresh)?;
        }
    }
    interpret_response(status, &body)
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use crate::testutil::TempDir;
    use std::io::{BufRead, BufReader, Read, Write};
    use std::net::{TcpListener, TcpStream};
    use std::path::PathBuf;
    use std::sync::{Arc, Mutex};

    const TOKEN: &str = "header.payload-SECRET-TOKEN.signature";

    fn auth() -> Auth {
        Auth {
            access_token: TOKEN.to_string(),
            account_id: Some("acct-123".to_string()),
        }
    }

    fn base64url(bytes: &[u8]) -> String {
        const ALPHABET: &[u8; 64] =
            b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";
        let mut out = String::new();
        for chunk in bytes.chunks(3) {
            let word = (u32::from(chunk[0]) << 16)
                | (u32::from(*chunk.get(1).unwrap_or(&0)) << 8)
                | u32::from(*chunk.get(2).unwrap_or(&0));
            for index in 0..=chunk.len() {
                out.push(ALPHABET[((word >> (18 - 6 * index)) & 63) as usize] as char);
            }
        }
        out
    }

    fn jwt(claims: &str) -> String {
        format!("h.{}.s", base64url(claims.as_bytes()))
    }

    fn test_endpoint(url: String) -> Endpoint {
        Endpoint {
            url,
            https_only: false,
        }
    }

    fn write_auth(temp: &TempDir, token: &str) -> PathBuf {
        let path = temp.path().join("auth.json");
        fs::write(
            &path,
            format!(r#"{{"tokens":{{"access_token":"{token}","account_id":"acct-123"}}}}"#),
        )
        .unwrap();
        path
    }

    #[test]
    fn reads_the_codex_login_shape() {
        let parsed = parse_auth(
            r#"{"auth_mode":"chatgpt","tokens":{"access_token":"tok","account_id":"acct"}}"#,
        )
        .unwrap();
        assert_eq!(parsed.access_token, "tok");
        assert_eq!(parsed.account_id.as_deref(), Some("acct"));
    }

    #[test]
    fn account_id_falls_back_to_the_token_claim() {
        let token = jwt(r#"{"https://api.openai.com/auth":{"chatgpt_account_id":"from-jwt"}}"#);
        let text = format!(r#"{{"tokens":{{"access_token":"{token}"}}}}"#);
        assert_eq!(
            parse_auth(&text).unwrap().account_id.as_deref(),
            Some("from-jwt")
        );
    }

    #[test]
    fn an_api_key_login_is_not_treated_as_a_chatgpt_login() {
        // This backend means the ChatGPT login, never a paid API key.
        for text in [
            r#"{"auth_mode":"apikey","OPENAI_API_KEY":"sk-test","tokens":null}"#,
            r#"{"OPENAI_API_KEY":"sk-test"}"#,
            r#"{"tokens":{"access_token":""}}"#,
            r#"{}"#,
        ] {
            assert_eq!(parse_auth(text), Err(ParseAuthError::NoLogin), "{text}");
        }
    }

    #[test]
    fn hostile_token_values_are_refused() {
        for token in ["tok\\nurl = \\\"http://evil\\\"", "two words", "é"] {
            let text = format!(r#"{{"tokens":{{"access_token":"{token}"}}}}"#);
            assert!(
                matches!(parse_auth(&text), Err(ParseAuthError::Malformed(_))),
                "{token}"
            );
        }
        let text = r#"{"tokens":{"access_token":"ok","account_id":"a\nb"}}"#;
        assert!(matches!(
            parse_auth(text),
            Err(ParseAuthError::Malformed(_))
        ));
    }

    #[test]
    fn auth_errors_never_echo_file_contents() {
        let temp = TempDir::new();
        let path = temp.path().join("auth.json");
        fs::write(&path, format!("{{not json {TOKEN}")).unwrap();
        assert!(!read_auth(&path).unwrap_err().contains("SECRET"));
        assert!(!format!("{:?}", auth()).contains("SECRET"));
    }

    #[test]
    fn missing_login_explains_how_to_sign_in() {
        let temp = TempDir::new();
        let error = read_auth(&temp.path().join("auth.json")).unwrap_err();
        assert!(error.contains("Sign in to Codex"), "{error}");
        assert!(error.contains("keyring"), "{error}");
    }

    #[test]
    fn expiry_uses_the_jwt_claim_with_a_margin() {
        let token = jwt(r#"{"exp":1000}"#);
        assert!(!token_expired(&token, 900));
        assert!(token_expired(&token, 950));
        assert!(token_expired(&token, 2000));
        // Unreadable claims are the server's call.
        assert!(!token_expired("opaque-token", 2000));
        assert!(!token_expired(&jwt(r#"{"sub":"x"}"#), 2000));
    }

    #[test]
    fn base64url_handles_padding_and_both_alphabets() {
        assert_eq!(base64url_decode("aGk").unwrap(), b"hi");
        assert_eq!(base64url_decode("aGk=").unwrap(), b"hi");
        assert_eq!(
            base64url_decode("-_-_").unwrap(),
            base64url_decode("+/+/").unwrap()
        );
        assert!(base64url_decode("a*b").is_none());
    }

    #[test]
    fn config_document_keeps_awkward_paths_in_one_field() {
        let document = curl_config(
            &Endpoint::production(),
            &auth(),
            Path::new("/run/a \"b\"; type=evil,\\c.wav"),
            1500,
            "de",
            60,
        )
        .unwrap();
        assert!(document.contains(r#"url = "https://chatgpt.com/backend-api/transcribe""#));
        assert!(document.contains(r#"proto = "=https""#));
        assert!(document.contains("max-redirs = 0"));
        assert!(document.contains(r#"form-string = "duration_ms=1500""#));
        assert!(document.contains(r#"form-string = "language=de""#));
        assert!(document.contains(r#"header = "originator: codex_desktop""#));
        assert!(document.contains(r#"header = "chatgpt-account-id: acct-123""#));
        assert!(document.contains(
            r#"form = "file=@\"/run/a \\\"b\\\"; type=evil,\\\\c.wav\";type=audio/wav;filename=wispx.wav""#
        ));
    }

    #[test]
    fn auto_language_is_omitted() {
        for language in ["", "auto"] {
            let document = curl_config(
                &Endpoint::production(),
                &auth(),
                Path::new("/a.wav"),
                1,
                language,
                60,
            )
            .unwrap();
            assert!(!document.contains("language="), "{document}");
        }
    }

    #[test]
    fn response_mapping_matches_the_windows_app() {
        assert_eq!(
            interpret_response(200, br#"{"text":" hi "}"#).unwrap(),
            "hi"
        );
        assert_eq!(interpret_response(200, br#"{"text":""}"#).unwrap(), "");
        let message =
            |status, body: &[u8]| interpret_response(status, body).unwrap_err().to_string();
        assert!(message(200, b"<html>").contains("Unexpected reply"));
        assert!(message(200, br#"{"other":1}"#).contains("Unexpected reply"));
        assert!(message(400, b"audio_too_short").contains("Too short"));
        assert!(message(400, b"bad").contains("ChatGPT error 400"));
        assert!(message(401, b"").contains("codex login"));
        assert!(message(403, b"").contains("403"));
        assert!(message(413, b"").contains("too long"));
        assert!(message(429, b"").contains("rate limit"));
        assert!(message(503, b"").contains("503"));
    }

    #[test]
    fn status_is_taken_from_the_last_line_only() {
        let (status, body) = split_status(b"{\"text\":\"a\n200\"}\n403").unwrap();
        assert_eq!(status, 403);
        assert_eq!(body, b"{\"text\":\"a\n200\"}");
        assert!(split_status(b"no newline").is_none());
        assert!(split_status(b"x\nnot-a-status").is_none());
    }

    #[test]
    fn curl_network_failures_become_plain_messages() {
        assert_eq!(
            explain_curl_failure(Some(6), b""),
            "No connection to ChatGPT"
        );
        assert_eq!(
            explain_curl_failure(Some(7), b""),
            "No connection to ChatGPT"
        );
        assert_eq!(explain_curl_failure(Some(28), b""), "ChatGPT took too long");
        assert!(explain_curl_failure(Some(99), b"curl: (99) odd\n").contains("odd"));
    }

    /// A fake `curl`: records argv and the stdin document, then replies.
    fn fake_curl(temp: &TempDir, reply: &str) -> (Toolbox, PathBuf, PathBuf) {
        let argv = temp.path().join("argv");
        let stdin = temp.path().join("stdin");
        temp.script(
            "curl",
            &format!(
                "printf '%s\\n' \"$@\" > {argv}\ncat > {stdin}\nprintf '{reply}'",
                argv = argv.display(),
                stdin = stdin.display(),
            ),
        );
        (Toolbox::with_path(temp.path()), argv, stdin)
    }

    #[test]
    fn the_token_travels_over_stdin_never_argv() {
        let temp = TempDir::new();
        let (tools, argv, stdin) = fake_curl(&temp, "{\"text\":\"hello\"}\\n200");
        let path = write_auth(&temp, TOKEN);
        let config = ChatGptConfig {
            auth_file: path,
            timeout_secs: 5,
        };
        let cancel = AtomicBool::new(false);
        let text = transcribe(
            &config,
            &Endpoint::production(),
            "",
            &tools,
            Path::new("/a.wav"),
            100,
            &cancel,
        )
        .unwrap();
        assert_eq!(text, "hello");

        let argv = fs::read_to_string(argv).unwrap();
        assert_eq!(argv, "--disable\n--config\n-\n");
        assert!(!argv.contains("SECRET"));
        let document = fs::read_to_string(stdin).unwrap();
        assert!(document.contains(&format!("Authorization: Bearer {TOKEN}")));
    }

    #[test]
    fn the_login_file_is_never_written() {
        let temp = TempDir::new();
        let (tools, _, _) = fake_curl(&temp, "\\n401");
        let path = write_auth(&temp, TOKEN);
        let before = fs::read(&path).unwrap();
        let modified = fs::metadata(&path).unwrap().modified().unwrap();
        let config = ChatGptConfig {
            auth_file: path.clone(),
            timeout_secs: 5,
        };
        let cancel = AtomicBool::new(false);
        let error = transcribe(
            &config,
            &Endpoint::production(),
            "",
            &tools,
            Path::new("/a.wav"),
            100,
            &cancel,
        )
        .unwrap_err();
        assert!(error.to_string().contains("codex login"), "{error}");
        assert_eq!(fs::read(&path).unwrap(), before);
        assert_eq!(fs::metadata(&path).unwrap().modified().unwrap(), modified);
    }

    #[test]
    fn an_expired_token_fails_before_any_upload() {
        let temp = TempDir::new();
        let (tools, argv, _) = fake_curl(&temp, "\\n200");
        let path = write_auth(&temp, &jwt(r#"{"exp":1000}"#));
        let config = ChatGptConfig {
            auth_file: path,
            timeout_secs: 5,
        };
        let cancel = AtomicBool::new(false);
        let error = transcribe(
            &config,
            &Endpoint::production(),
            "",
            &tools,
            Path::new("/a.wav"),
            100,
            &cancel,
        )
        .unwrap_err();
        assert!(error.to_string().contains("expired"), "{error}");
        assert!(!argv.exists(), "curl ran with an expired token");
        assert_eq!(precheck(&config, &tools).unwrap_err(), EXPIRED);
    }

    #[test]
    fn a_rejected_token_is_retried_once_if_codex_refreshed_the_file() {
        let temp = TempDir::new();
        let path = write_auth(&temp, "stale-token");
        let counter = temp.path().join("calls");
        temp.script(
            "curl",
            &format!(
                r#"cat > /dev/null
echo x >> {counter}
if [ "$(wc -l < {counter})" -eq 1 ]; then
  printf '{{"tokens":{{"access_token":"fresh-token"}}}}' > {auth}
  printf '\n401'
else
  printf '{{"text":"second try"}}\n200'
fi"#,
                counter = counter.display(),
                auth = path.display(),
            ),
        );
        let tools = Toolbox::with_path(temp.path());
        let config = ChatGptConfig {
            auth_file: path,
            timeout_secs: 5,
        };
        let cancel = AtomicBool::new(false);
        let text = transcribe(
            &config,
            &Endpoint::production(),
            "",
            &tools,
            Path::new("/a.wav"),
            100,
            &cancel,
        )
        .unwrap();
        assert_eq!(text, "second try");
        assert_eq!(fs::read_to_string(counter).unwrap().lines().count(), 2);
    }

    #[derive(Default)]
    struct Captured {
        request_line: String,
        headers: Vec<String>,
        body: Vec<u8>,
    }

    fn handle_http(mut stream: TcpStream, response: &str, sink: &Mutex<Captured>) {
        let mut reader = BufReader::new(stream.try_clone().unwrap());
        let mut line = String::new();
        reader.read_line(&mut line).unwrap();
        let mut captured = Captured {
            request_line: line.trim().to_string(),
            ..Captured::default()
        };
        let mut length = 0usize;
        loop {
            let mut header = String::new();
            reader.read_line(&mut header).unwrap();
            let header = header.trim().to_string();
            if header.is_empty() {
                break;
            }
            if let Some(value) = header.to_ascii_lowercase().strip_prefix("content-length:") {
                length = value.trim().parse().unwrap();
            }
            captured.headers.push(header);
        }
        let mut body = vec![0; length];
        reader.read_exact(&mut body).unwrap();
        captured.body = body;
        *sink.lock().unwrap() = captured;
        write!(
            stream,
            "HTTP/1.1 {response}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
        )
        .unwrap();
    }

    /// Real curl against a loopback server; no external network and no account.
    /// Skipped only where curl is not installed.
    #[test]
    fn real_curl_sends_the_exact_multipart_request() {
        let tools = Toolbox::from_env();
        if !tools.has("curl") {
            eprintln!("skipped: curl not installed");
            return;
        }
        let temp = TempDir::new();
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}/transcribe", listener.local_addr().unwrap());
        let sink = Arc::new(Mutex::new(Captured::default()));
        let server_sink = Arc::clone(&sink);
        thread::spawn(move || {
            if let Ok((stream, _)) = listener.accept() {
                handle_http(stream, "401 Unauthorized", &server_sink);
            }
        });

        let wav = temp.path().join("a \"tricky\"; name,x.wav");
        fs::write(&wav, b"RIFFdata").unwrap();
        let auth_path = write_auth(&temp, TOKEN);
        let config = ChatGptConfig {
            auth_file: auth_path,
            timeout_secs: 10,
        };
        let cancel = AtomicBool::new(false);
        let error = transcribe(
            &config,
            &test_endpoint(url),
            "de",
            &tools,
            &wav,
            1234,
            &cancel,
        )
        .unwrap_err();
        assert!(error.to_string().contains("codex login"), "{error}");

        let captured = sink.lock().unwrap();
        assert_eq!(captured.request_line, "POST /transcribe HTTP/1.1");
        let has_header = |expected: &str| {
            captured
                .headers
                .iter()
                .any(|header| header.eq_ignore_ascii_case(expected))
        };
        assert!(has_header(&format!("Authorization: Bearer {TOKEN}")));
        assert!(has_header("originator: codex_desktop"));
        assert!(has_header("chatgpt-account-id: acct-123"));
        let body = String::from_utf8_lossy(&captured.body);
        assert!(
            body.contains(r#"name="file"; filename="wispx.wav""#),
            "{body}"
        );
        assert!(body.contains("Content-Type: audio/wav"), "{body}");
        assert!(body.contains("RIFFdata"), "{body}");
        assert!(body.contains(r#"name="duration_ms""#), "{body}");
        assert!(body.contains("1234"), "{body}");
        assert!(body.contains(r#"name="language""#), "{body}");
    }
}
