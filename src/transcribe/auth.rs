//! Codex authentication is shared with its app; refresh writes rotated tokens back.
use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use serde_json::{Value, json};
use std::{
    io,
    path::PathBuf,
    sync::Mutex,
    time::{SystemTime, UNIX_EPOCH},
};
use ureq::Agent;
static REFRESH_LOCK: Mutex<()> = Mutex::new(());

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AuthMode {
    ChatGpt,
    ApiKey,
    Unknown,
}
pub struct Login {
    pub mode: AuthMode,
    pub access: String,
    pub account: String,
    refresh: Option<String>,
}
pub fn path() -> io::Result<PathBuf> {
    std::env::var_os("USERPROFILE")
        .or_else(|| std::env::var_os("HOME"))
        .map(|home| PathBuf::from(home).join(".codex/auth.json"))
        .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "User profile is unavailable"))
}
fn read() -> io::Result<Value> {
    #[cfg(windows)]
    let text = crate::platform::windows::files::read_shared(&path()?)?;
    #[cfg(not(windows))]
    let text = std::fs::read_to_string(path()?)?;
    Ok(serde_json::from_str(&text)?)
}
pub fn login() -> Option<Login> {
    let root = read().ok()?;
    let tokens = &root["tokens"];
    let access = tokens["access_token"].as_str()?.to_owned();
    if access.is_empty() {
        return None;
    }
    let account = tokens["account_id"]
        .as_str()
        .map(str::to_owned)
        .or_else(|| {
            payload(&access)?["https://api.openai.com/auth"]["chatgpt_account_id"]
                .as_str()
                .map(str::to_owned)
        })
        .unwrap_or_default();
    Some(Login {
        mode: match root.get("auth_mode") {
            None if tokens.is_object() => AuthMode::ChatGpt,
            Some(Value::String(mode)) if mode == "chatgpt" => AuthMode::ChatGpt,
            Some(Value::String(mode)) if mode == "apikey" => AuthMode::ApiKey,
            _ => AuthMode::Unknown,
        },
        access,
        account,
        refresh: tokens["refresh_token"].as_str().map(str::to_owned),
    })
}
pub fn email() -> Option<String> {
    login()?;
    claim(read().ok()?["tokens"]["id_token"].as_str()?, "email")
}
fn payload(jwt: &str) -> Option<Value> {
    serde_json::from_slice(
        &URL_SAFE_NO_PAD
            .decode(jwt.split('.').nth(1)?.trim_end_matches('='))
            .ok()?,
    )
    .ok()
}
fn claim(jwt: &str, name: &str) -> Option<String> {
    let p = payload(jwt)?;
    let value = p.get(name)?;
    Some(
        value
            .as_str()
            .map_or_else(|| value.to_string(), str::to_owned),
    )
}
pub fn expired(jwt: &str) -> bool {
    claim(jwt, "exp")
        .and_then(|e| e.parse::<u64>().ok())
        .is_some_and(|e| {
            e < SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap_or_default()
                .as_secs()
                + 60
        })
}

/// Refresh failures leave the old access token available for the original request.
pub fn refresh(agent: &Agent, stale: &str) -> Option<String> {
    let _gate = REFRESH_LOCK.lock().expect("refresh does not panic");
    let current = login()?;
    if current.access != stale && !expired(&current.access) {
        return Some(current.access);
    }
    let refresh = current.refresh.filter(|s| !s.is_empty())?;
    let client = claim(&current.access, "client_id")
        .unwrap_or_else(|| "app_EMoamEEZ73f0CkXaXp7hrann".into());
    let mut response = agent.post("https://auth.openai.com/oauth/token").send_json(json!({"client_id": client, "grant_type": "refresh_token", "refresh_token": refresh, "scope": "openid profile email"})).ok()?;
    if !response.status().is_success() {
        return None;
    }
    let new: Value = response.body_mut().read_json().ok()?;
    let access = new["access_token"]
        .as_str()
        .filter(|s| !s.is_empty())?
        .to_owned();
    let mut root = read().ok()?;
    let Some(tokens) = root.get_mut("tokens").and_then(Value::as_object_mut) else {
        return Some(access);
    };
    tokens.insert("access_token".into(), json!(access));
    for key in ["id_token", "refresh_token"] {
        if let Some(value) = new[key].as_str() {
            tokens.insert(key.into(), json!(value));
        }
    }
    root["last_refresh"] = json!(
        time::OffsetDateTime::now_utc()
            .format(&time::format_description::well_known::Rfc3339)
            .ok()?
    );
    let auth_path = path().ok()?;
    crate::settings::atomic_write(
        &auth_path,
        &auth_path.with_extension("json.wispx.tmp"),
        &serde_json::to_vec_pretty(&root).ok()?,
    )
    .ok()?;
    Some(access)
}
