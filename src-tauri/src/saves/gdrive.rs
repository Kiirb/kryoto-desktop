//! Save versions in the player's own Google Drive: sign in once with Google,
//! and every PC that connects the same account shares them. Free, no kryo.to
//! account or perk, no server to run.
//!
//! The same versions, restore and keep-N as a folder on this PC (local.rs),
//! answering the same calls (`api.rs` hands them here), laid out the same: a
//! `Kryoto Saves` folder in My Drive with `index.json` beside `<id>.ksave`.
//!
//! - **Signing in** is Google's flow for desktop apps: the browser opens
//!   Google's page, which sends the answer back to a one-time listener on
//!   127.0.0.1 (with PKCE). Only the `drive.file` scope is asked for: Kryoto
//!   sees the files it made itself, never anything else in the Drive.
//! - **The sign-in** (a refresh token) is kept in the OS secure store, like
//!   Patchbay's tokens; settings.json only has the account's email.
//! - **A backup** goes up through a resumable upload session Drive opens in
//!   `prepare`: its address needs no sign-in, so `api::upload` sends it as it
//!   sends any other. `claim` then checks the size and MD5 Drive computed.
//! - **A restore** comes down through a `gdrive:<file id>` address, which
//!   `api::download` hands back here, as `file:` goes to local.rs.
//!
//! Google needs an OAuth client for this ("Desktop app" type, Drive API on):
//! its id and secret come from `KRYOTO_GOOGLE_CLIENT_ID` and
//! `KRYOTO_GOOGLE_CLIENT_SECRET`, at build time or, for development, when
//! the app runs. A desktop client's secret is not a secret (Google says so);
//! the PKCE verifier is what protects the sign-in.

use std::collections::HashMap;
use std::path::Path;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use base64::Engine;
use serde::Deserialize;
use serde_json::json;
use tauri::{AppHandle, Runtime};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

use super::api::{Allowance, ApiError, Head, Link, List, Prepared, Version};
use super::local::{id_ok, new_id, pending_of, Index, Pending};

const AUTH_URL: &str = "https://accounts.google.com/o/oauth2/v2/auth";
const TOKEN_URL: &str = "https://oauth2.googleapis.com/token";
const REVOKE_URL: &str = "https://oauth2.googleapis.com/revoke";
const API: &str = "https://www.googleapis.com/drive/v3";
const UPLOAD: &str = "https://www.googleapis.com/upload/drive/v3";
const SCOPE: &str = "https://www.googleapis.com/auth/drive.file";
/// The secure store's name for the refresh token.
const SECRET: &str = "google-drive-token";
const FOLDER_NAME: &str = "Kryoto Saves";
const FOLDER_MIME: &str = "application/vnd.google-apps.folder";
/// How long the browser has to come back with Google's answer.
const SIGN_IN_FOR: Duration = Duration::from_secs(300);
/// The biggest one backup can be: Drive takes far more, but a backup this
/// size already takes a long while to go up and come back.
const MAX_BACKUP: u64 = 2 << 30;

/* ── the OAuth client ───────────────────────────────────────────────── */

fn client_credentials() -> Option<(String, String)> {
    let pick = |run: Option<String>, built: Option<&'static str>| run.or(built.map(str::to_string)).map(|v| v.trim().to_string()).filter(|v| !v.is_empty());
    let id = pick(std::env::var("KRYOTO_GOOGLE_CLIENT_ID").ok(), option_env!("KRYOTO_GOOGLE_CLIENT_ID"))?;
    let secret = pick(std::env::var("KRYOTO_GOOGLE_CLIENT_SECRET").ok(), option_env!("KRYOTO_GOOGLE_CLIENT_SECRET")).unwrap_or_default();
    Some((id, secret))
}

fn http() -> reqwest::Client {
    reqwest::Client::builder()
        .user_agent(concat!("KryotoDesktop/", env!("CARGO_PKG_VERSION")))
        .connect_timeout(Duration::from_secs(20))
        .timeout(Duration::from_secs(60))
        .build()
        .unwrap_or_default()
}

fn form(pairs: &[(&str, &str)]) -> String {
    let mut f = url::form_urlencoded::Serializer::new(String::new());
    for (k, v) in pairs {
        f.append_pair(k, v);
    }
    f.finish()
}

#[derive(Deserialize)]
struct Tokens {
    access_token: String,
    #[serde(default)]
    expires_in: u64,
    #[serde(default)]
    refresh_token: Option<String>,
}

/// The access token in use and when it runs out.
static ACCESS: Mutex<Option<(String, Instant)>> = Mutex::new(None);
/// The `Kryoto Saves` folder's id, for the account it is in.
static FOLDER: Mutex<Option<(String, String)>> = Mutex::new(None);
/// Backups announced by `prepare`, waiting for `claim`.
static PENDING: Mutex<Option<HashMap<String, Pending>>> = Mutex::new(None);
/// One index change at a time from this PC.
static INDEX: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());
/// Cancels a sign-in that is waiting for the browser.
static CANCEL: tokio::sync::Notify = tokio::sync::Notify::const_new();

fn remember_access(t: &Tokens) {
    let life = Duration::from_secs(t.expires_in.max(120) - 60);
    *ACCESS.lock().unwrap_or_else(|e| e.into_inner()) = Some((t.access_token.clone(), Instant::now() + life));
}

fn forget_access() {
    *ACCESS.lock().unwrap_or_else(|e| e.into_inner()) = None;
}

fn signed_out() -> ApiError {
    ApiError::Other("Google Drive is no longer connected. Connect it again in Settings > Cloud.".into())
}

/// A working access token: the one in use, or a fresh one from the refresh token.
async fn access() -> Result<String, ApiError> {
    if let Some((token, until)) = ACCESS.lock().unwrap_or_else(|e| e.into_inner()).clone() {
        if Instant::now() < until {
            return Ok(token);
        }
    }
    let refresh = crate::patchbay::secrets::read(SECRET).ok_or_else(signed_out)?;
    let (id, secret) = client_credentials().ok_or_else(no_client)?;
    let res = http()
        .post(TOKEN_URL)
        .header("content-type", "application/x-www-form-urlencoded")
        .body(form(&[("client_id", &id), ("client_secret", &secret), ("refresh_token", &refresh), ("grant_type", "refresh_token")]))
        .send()
        .await
        .map_err(|e| ApiError::Other(format!("Could not reach Google: {}", crate::logging::why(&e))))?;
    let status = res.status();
    let body: serde_json::Value = res.json().await.unwrap_or_default();
    if status.as_u16() == 400 && body.get("error").and_then(|e| e.as_str()) == Some("invalid_grant") {
        // Revoked in the Google account, or expired: only signing in again helps.
        return Err(signed_out());
    }
    if !status.is_success() {
        return Err(ApiError::Other(format!("Google refused the sign-in ({status}).")));
    }
    let tokens: Tokens = serde_json::from_value(body).map_err(|e| ApiError::Other(e.to_string()))?;
    remember_access(&tokens);
    Ok(tokens.access_token)
}

fn no_client() -> ApiError {
    ApiError::Other("This build of Kryoto Desktop has no Google sign-in set up (KRYOTO_GOOGLE_CLIENT_ID).".into())
}

/* ── talking to Drive ───────────────────────────────────────────────── */

/// Send a Drive request with the access token; once more with a fresh one
/// when Google says the token ran out.
async fn send(make: impl Fn(&reqwest::Client) -> reqwest::RequestBuilder) -> Result<reqwest::Response, ApiError> {
    let client = http();
    for attempt in 0..2 {
        let token = access().await?;
        let res = make(&client).bearer_auth(&token).send().await.map_err(|e| ApiError::Other(format!("Could not reach Google Drive: {}", crate::logging::why(&e))))?;
        let status = res.status().as_u16();
        if status == 401 && attempt == 0 {
            forget_access();
            continue;
        }
        if res.status().is_success() || status == 308 {
            return Ok(res);
        }
        let body: serde_json::Value = res.json().await.unwrap_or_default();
        let reason = body.pointer("/error/errors/0/reason").and_then(|r| r.as_str()).unwrap_or_default().to_string();
        let message = body.pointer("/error/message").and_then(|m| m.as_str()).unwrap_or("Google Drive could not do that.").to_string();
        return Err(match (status, reason.as_str()) {
            (401, _) => signed_out(),
            (403, "storageQuotaExceeded") => ApiError::TooBig("Your Google Drive is full.".into()),
            (429, _) | (403, "rateLimitExceeded" | "userRateLimitExceeded") => ApiError::Limited,
            (404, _) => ApiError::Other("That backup is not in your Google Drive any more.".into()),
            _ => ApiError::Other(format!("Google Drive: {message}")),
        });
    }
    Err(signed_out())
}

async fn json_of(res: reqwest::Response) -> Result<serde_json::Value, ApiError> {
    res.json().await.map_err(|e| ApiError::Other(format!("Google Drive answered oddly: {e}")))
}

/// A Drive query string value, quoted.
fn quoted(s: &str) -> String {
    format!("'{}'", s.replace('\\', "\\\\").replace('\'', "\\'"))
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase", default)]
struct DriveFile {
    id: String,
    #[serde(deserialize_with = "number_text")]
    size: u64,
    md5_checksum: String,
    sha256_checksum: String,
}

/// Drive sends sizes as text.
fn number_text<'de, D: serde::Deserializer<'de>>(d: D) -> Result<u64, D::Error> {
    let v = serde_json::Value::deserialize(d)?;
    Ok(match v {
        serde_json::Value::String(s) => s.parse().unwrap_or(0),
        serde_json::Value::Number(n) => n.as_u64().unwrap_or(0),
        _ => 0,
    })
}

async fn search(q: String) -> Result<Vec<DriveFile>, ApiError> {
    let res = send(|c| c.get(format!("{API}/files")).query(&[("q", q.as_str()), ("spaces", "drive"), ("fields", "files(id,size,md5Checksum,sha256Checksum)"), ("pageSize", "10")])).await?;
    let v = json_of(res).await?;
    Ok(serde_json::from_value(v.get("files").cloned().unwrap_or_default()).unwrap_or_default())
}

/// The `Kryoto Saves` folder, made the first time. With `drive.file`, only a
/// folder Kryoto made itself can be found, so the player's own folders of
/// the same name are never touched.
async fn folder(account: &str) -> Result<String, ApiError> {
    if let Some((who, id)) = FOLDER.lock().unwrap_or_else(|e| e.into_inner()).clone() {
        if who == account {
            return Ok(id);
        }
    }
    let q = format!("appProperties has {{ key='kryoto' and value='saves' }} and mimeType={} and trashed=false", quoted(FOLDER_MIME));
    let id = match search(q).await?.into_iter().next() {
        Some(f) => f.id,
        None => {
            let meta = json!({ "name": FOLDER_NAME, "mimeType": FOLDER_MIME, "appProperties": { "kryoto": "saves" } });
            let res = send(|c| c.post(format!("{API}/files")).query(&[("fields", "id")]).json(&meta)).await?;
            json_of(res).await?.get("id").and_then(|i| i.as_str()).map(str::to_string).ok_or_else(|| ApiError::Other("Google Drive did not make the folder.".into()))?
        }
    };
    *FOLDER.lock().unwrap_or_else(|e| e.into_inner()) = Some((account.to_string(), id.clone()));
    Ok(id)
}

async fn find(folder: &str, name: &str) -> Result<Option<DriveFile>, ApiError> {
    let q = format!("name={} and {} in parents and trashed=false", quoted(name), quoted(folder));
    Ok(search(q).await?.into_iter().next())
}

/// A JSON file in the folder, with its Drive id; the default when there is none yet.
async fn read_json<T: serde::de::DeserializeOwned + Default>(folder: &str, name: &str) -> Result<(T, Option<String>), ApiError> {
    let Some(file) = find(folder, name).await? else { return Ok((T::default(), None)) };
    let res = send(|c| c.get(format!("{API}/files/{}", file.id)).query(&[("alt", "media")])).await?;
    let bytes = res.bytes().await.map_err(|e| ApiError::Other(format!("Could not read {name} from Google Drive: {e}")))?;
    let value = serde_json::from_slice(&bytes).map_err(|_| ApiError::Other(format!("{name} in your Google Drive's Kryoto Saves folder is damaged.")))?;
    Ok((value, Some(file.id)))
}

/// Write a JSON file in the folder: over `existing`, or as a new file.
async fn write_json<T: serde::Serialize>(folder: &str, name: &str, existing: Option<String>, value: &T) -> Result<(), ApiError> {
    let body = serde_json::to_vec_pretty(value).map_err(|e| ApiError::Other(e.to_string()))?;
    let id = match existing {
        Some(id) => id,
        None => {
            let meta = json!({ "name": name, "parents": [folder], "mimeType": "application/json", "appProperties": { "kryoto": "data" } });
            let res = send(|c| c.post(format!("{API}/files")).query(&[("fields", "id")]).json(&meta)).await?;
            json_of(res).await?.get("id").and_then(|i| i.as_str()).map(str::to_string).ok_or_else(|| ApiError::Other(format!("Google Drive did not make {name}.")))?
        }
    };
    send(|c| c.patch(format!("{UPLOAD}/files/{id}")).query(&[("uploadType", "media")]).header("content-type", "application/json").body(body.clone())).await?;
    Ok(())
}

async fn read_index(folder: &str) -> Result<(Index, Option<String>), ApiError> {
    read_json(folder, "index.json").await
}

async fn write_index(folder: &str, existing: Option<String>, index: &Index) -> Result<(), ApiError> {
    write_json(folder, "index.json", existing, index).await
}

/// One JSON file in the account's Kryoto Saves folder, for other things kept
/// beside the save versions (achievements_cloud.rs's `achievements.json`).
/// One reader-then-writer at a time from this PC.
pub(crate) async fn update_json<T, R>(account: &str, name: &str, change: impl FnOnce(&mut T) -> (bool, R)) -> Result<R, String>
where
    T: serde::de::DeserializeOwned + serde::Serialize + Default,
{
    let _one = INDEX.lock().await;
    let folder = folder(account).await.map_err(|e| e.to_string())?;
    let (mut value, id) = read_json::<T>(&folder, name).await.map_err(|e| e.to_string())?;
    let (changed, out) = change(&mut value);
    if changed {
        write_json(&folder, name, id, &value).await.map_err(|e| e.to_string())?;
    }
    Ok(out)
}

async fn delete(file_id: &str) {
    let _ = send(|c| c.delete(format!("{API}/files/{file_id}"))).await;
}

async fn allowance(keep: u32, used: u64) -> Allowance {
    let free = async {
        let res = send(|c| c.get(format!("{API}/about")).query(&[("fields", "storageQuota(limit,usage)")])).await.ok()?;
        let v = res.json::<serde_json::Value>().await.ok()?;
        let num = |k: &str| v.pointer(&format!("/storageQuota/{k}")).and_then(|n| n.as_str()).and_then(|n| n.parse::<u64>().ok());
        // No limit: an unlimited (Workspace) account.
        Some(num("limit").map(|l| l.saturating_sub(num("usage").unwrap_or(0))).unwrap_or(1 << 40))
    };
    let free = free.await.unwrap_or(0);
    Allowance { total_bytes: used + free, versions_per_game: keep, max_backup_bytes: MAX_BACKUP, max_raw_bytes: 4 * MAX_BACKUP }
}

/* ── the store, as api.rs asks it ───────────────────────────────────── */

pub struct DriveStore {
    account: String,
    keep: u32,
}

impl DriveStore {
    pub fn new(account: String, keep: u32) -> Self {
        DriveStore { account, keep: keep.clamp(1, 100) }
    }

    pub async fn list(&self) -> Result<List, ApiError> {
        let (index, _) = read_index(&folder(&self.account).await?).await?;
        Ok(index.list(allowance(self.keep, index.used()).await))
    }

    pub async fn head(&self, game_key: &str) -> Result<Head, ApiError> {
        let (index, _) = read_index(&folder(&self.account).await?).await?;
        Ok(Head { perk: true, allowance: allowance(self.keep, index.used()).await, head: index.head(game_key) })
    }

    /// Check the backup and that this PC saw the newest version, then open
    /// a resumable upload for its bytes.
    pub async fn prepare(&self, body: serde_json::Value) -> Result<Prepared, ApiError> {
        let pending = pending_of(&body, MAX_BACKUP)?;
        let folder = folder(&self.account).await?;
        read_index(&folder).await?.0.check_parent(&pending)?;
        let id = new_id(body.get("gameKey").and_then(|k| k.as_str()).unwrap_or_default());
        let meta = json!({ "name": format!("{id}.ksave"), "parents": [folder], "mimeType": "application/octet-stream", "appProperties": { "kryoto": "version" } });
        let size = pending.size.to_string();
        let res = send(|c| {
            c.post(format!("{UPLOAD}/files"))
                .query(&[("uploadType", "resumable")])
                .header("x-upload-content-type", "application/octet-stream")
                .header("x-upload-content-length", size.as_str())
                .json(&meta)
        })
        .await?;
        let url = res.headers().get("location").and_then(|l| l.to_str().ok()).map(str::to_string).ok_or_else(|| ApiError::Other("Google Drive did not open an upload.".into()))?;
        PENDING.lock().unwrap_or_else(|e| e.into_inner()).get_or_insert_with(HashMap::new).insert(id.clone(), pending);
        Ok(Prepared { upload_id: id, url, headers: HashMap::new() })
    }

    /// The upload must be exactly what was announced; then it is the newest
    /// version, and versions past the newest `keep` are deleted from Drive.
    pub async fn claim(&self, upload_id: &str) -> Result<Version, ApiError> {
        let never = || ApiError::Rejected("That upload never arrived.".into());
        if !id_ok(upload_id) {
            return Err(never());
        }
        let pending = PENDING.lock().unwrap_or_else(|e| e.into_inner()).get_or_insert_with(HashMap::new).remove(upload_id);
        let folder = folder(&self.account).await?;
        let file = find(&folder, &format!("{upload_id}.ksave")).await?;
        let (Some(p), Some(file)) = (pending, file) else {
            return Err(never());
        };
        let same = file.size == p.size
            && if !file.sha256_checksum.is_empty() {
                file.sha256_checksum.eq_ignore_ascii_case(&p.sha256)
            } else {
                !p.md5.is_empty() && file.md5_checksum.eq_ignore_ascii_case(&p.md5)
            };
        if !same {
            delete(&file.id).await;
            return Err(ApiError::Rejected("The backup did not arrive in Google Drive as it was sent.".into()));
        }
        let _one = INDEX.lock().await;
        let (mut index, index_id) = read_index(&folder).await?;
        let (version, dropped) = match index.commit(upload_id, p, self.keep) {
            Ok(done) => done,
            Err(e) => {
                delete(&file.id).await;
                return Err(e);
            }
        };
        write_index(&folder, index_id, &index).await?;
        for old in dropped {
            if let Ok(Some(f)) = find(&folder, &format!("{old}.ksave")).await {
                delete(&f.id).await;
            }
        }
        Ok(version)
    }

    pub async fn link(&self, version_id: &str) -> Result<Link, ApiError> {
        let folder = folder(&self.account).await?;
        let (index, _) = read_index(&folder).await?;
        let v = index.find(version_id).cloned().ok_or_else(|| ApiError::Other("No such backup.".into()))?;
        let file = find(&folder, &format!("{}.ksave", v.id)).await?.ok_or_else(|| ApiError::Other("That backup is not in your Google Drive any more.".into()))?;
        Ok(Link { url: format!("gdrive:{}", file.id), size: v.size, sha256: v.sha256 })
    }
}

/// A `gdrive:` address from `link`, as a Drive file id; None for anything else.
pub(crate) fn file_of(url: &str) -> Option<&str> {
    url.strip_prefix("gdrive:").filter(|id| !id.is_empty() && id.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_'))
}

/// Download a backup from Drive, for `api::download`.
pub(crate) async fn download(file_id: &str, out: &Path, max: u64, on_got: impl Fn(u64, u64)) -> Result<(), String> {
    let res = send(|c| c.get(format!("{API}/files/{file_id}")).query(&[("alt", "media")])).await.map_err(|e| e.to_string())?;
    super::api::save_body(res, out, max, on_got).await
}

/* ── connecting an account ──────────────────────────────────────────── */

fn random_text(bytes: usize) -> Result<String, String> {
    let mut buf = vec![0u8; bytes];
    getrandom::fill(&mut buf).map_err(|e| format!("No randomness for the sign-in: {e}"))?;
    Ok(base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(buf))
}

/// What a browser asked of the listener: Google's answer, or something else.
enum Reply {
    Code(String),
    Refused(String),
    Other,
}

fn read_reply(request: &str, state: &str) -> Reply {
    let Some(target) = request.lines().next().and_then(|l| l.split_whitespace().nth(1)) else { return Reply::Other };
    let Ok(url) = url::Url::parse(&format!("http://127.0.0.1{target}")) else { return Reply::Other };
    if url.path() != "/" {
        return Reply::Other;
    }
    let q: HashMap<String, String> = url.query_pairs().into_owned().collect();
    if q.get("state").map(String::as_str) != Some(state) {
        return Reply::Other;
    }
    match (q.get("code"), q.get("error")) {
        (Some(code), _) => Reply::Code(code.clone()),
        (None, Some(err)) => Reply::Refused(if err == "access_denied" { "Google Drive was not connected: access was not allowed.".into() } else { format!("Google said no: {err}.") }),
        _ => Reply::Other,
    }
}

fn page(title: &str, text: &str) -> String {
    let body = format!(
        "<!doctype html><meta charset=utf-8><title>{title}</title><body style=\"font:16px system-ui;background:#111;color:#eee;display:grid;place-items:center;height:100vh;margin:0\"><div style=\"text-align:center\"><h1 style=\"font-size:20px\">{title}</h1><p>{text}</p></div>"
    );
    format!("HTTP/1.1 200 OK\r\ncontent-type: text/html; charset=utf-8\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}", body.len())
}

/// Wait for Google to send the browser back with a code.
async fn wait_for_code(listener: tokio::net::TcpListener, state: &str) -> Result<String, String> {
    loop {
        let (mut sock, _) = listener.accept().await.map_err(|e| e.to_string())?;
        let mut buf = vec![0u8; 8192];
        let n = tokio::time::timeout(Duration::from_secs(10), sock.read(&mut buf)).await.ok().and_then(|r| r.ok()).unwrap_or(0);
        let request = String::from_utf8_lossy(&buf[..n]).to_string();
        match read_reply(&request, state) {
            Reply::Code(code) => {
                let _ = sock.write_all(page("Google Drive connected", "You can close this tab and go back to Kryoto Desktop.").as_bytes()).await;
                return Ok(code);
            }
            Reply::Refused(why) => {
                let _ = sock.write_all(page("Not connected", &why).as_bytes()).await;
                return Err(why);
            }
            Reply::Other => {
                let _ = sock.write_all(b"HTTP/1.1 404 Not Found\r\ncontent-length: 0\r\nconnection: close\r\n\r\n").await;
            }
        }
    }
}

/// Sign in with Google in the browser, keep the sign-in, and keep `what`
/// (`saves` or `achievements`) there from now on. Returns the account's email.
pub async fn connect<R: Runtime>(app: &AppHandle<R>, what: &str) -> Result<String, String> {
    let (client_id, client_secret) = client_credentials().ok_or_else(|| no_client().to_string())?;
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.map_err(|e| format!("Could not wait for Google's answer: {e}"))?;
    let redirect = format!("http://127.0.0.1:{}", listener.local_addr().map_err(|e| e.to_string())?.port());
    let state = random_text(24)?;
    let verifier = random_text(48)?;
    let challenge = {
        use sha2::Digest;
        base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(sha2::Sha256::digest(verifier.as_bytes()))
    };
    let mut url = url::Url::parse(AUTH_URL).map_err(|e| e.to_string())?;
    url.query_pairs_mut()
        .append_pair("client_id", &client_id)
        .append_pair("redirect_uri", &redirect)
        .append_pair("response_type", "code")
        .append_pair("scope", SCOPE)
        .append_pair("code_challenge", &challenge)
        .append_pair("code_challenge_method", "S256")
        .append_pair("state", &state)
        .append_pair("access_type", "offline")
        .append_pair("prompt", "consent");
    crate::open_with_system(url.as_str())?;
    let code = tokio::select! {
        r = tokio::time::timeout(SIGN_IN_FOR, wait_for_code(listener, &state)) => r.map_err(|_| "Google did not answer in time. Try connecting again.".to_string())??,
        _ = CANCEL.notified() => return Err("Connecting Google Drive was canceled.".into()),
    };
    let res = http()
        .post(TOKEN_URL)
        .header("content-type", "application/x-www-form-urlencoded")
        .body(form(&[
            ("code", &code),
            ("client_id", &client_id),
            ("client_secret", &client_secret),
            ("redirect_uri", &redirect),
            ("grant_type", "authorization_code"),
            ("code_verifier", &verifier),
        ]))
        .send()
        .await
        .map_err(|e| format!("Could not reach Google: {}", crate::logging::why(&e)))?;
    if !res.status().is_success() {
        return Err(format!("Google refused the sign-in ({}).", res.status()));
    }
    let tokens: Tokens = res.json().await.map_err(|e| e.to_string())?;
    let refresh = tokens.refresh_token.clone().ok_or("Google gave no lasting sign-in. Try connecting again.")?;
    remember_access(&tokens);
    let about = send(|c| c.get(format!("{API}/about")).query(&[("fields", "user(emailAddress)")])).await.map_err(|e| e.to_string())?;
    let email = json_of(about).await.map_err(|e| e.to_string())?.pointer("/user/emailAddress").and_then(|e| e.as_str()).unwrap_or("Google account").to_string();
    crate::patchbay::secrets::write(SECRET, &refresh)?;
    *FOLDER.lock().unwrap_or_else(|e| e.into_inner()) = None;
    let mut s = crate::settings::load(app);
    s.saves_gdrive_account = email.clone();
    if what == "achievements" {
        s.achievements_mode = "gdrive".into();
    } else {
        s.saves_mode = "gdrive".into();
    }
    crate::settings::write(app, &s)?;
    crate::patchbay::endpoints_changed(app);
    crate::logging::info("saves", &format!("Google Drive connected for {what}"));
    Ok(email)
}

/// Stop a sign-in waiting for the browser.
pub fn cancel() {
    CANCEL.notify_waiters();
}

/// Forget the Google account (for saves and achievements both): the sign-in
/// is revoked and removed. What is in the Drive stays there, for connecting
/// again later.
pub async fn disconnect<R: Runtime>(app: &AppHandle<R>) -> Result<(), String> {
    if let Some(refresh) = crate::patchbay::secrets::read(SECRET) {
        let _ = http().post(REVOKE_URL).header("content-type", "application/x-www-form-urlencoded").body(form(&[("token", &refresh)])).send().await;
    }
    crate::patchbay::secrets::remove(SECRET)?;
    forget_access();
    *FOLDER.lock().unwrap_or_else(|e| e.into_inner()) = None;
    let mut s = crate::settings::load(app);
    s.saves_gdrive_account = String::new();
    let fallback = if s.saves_local_dir.trim().is_empty() { String::new() } else { "local".to_string() };
    if s.saves_mode == "gdrive" {
        s.saves_mode = fallback.clone();
    }
    if s.achievements_mode == "gdrive" {
        s.achievements_mode = fallback;
    }
    crate::settings::write(app, &s)?;
    crate::patchbay::endpoints_changed(app);
    Ok(())
}

/// `what`: `saves` (the default) or `achievements`, which starts keeping there.
#[tauri::command]
pub async fn gdrive_connect(app: AppHandle, what: Option<String>) -> Result<String, String> {
    connect(&app, what.as_deref().unwrap_or("saves")).await
}

#[tauri::command]
pub fn gdrive_cancel() {
    cancel();
}

#[tauri::command]
pub async fn gdrive_disconnect(app: AppHandle) -> Result<(), String> {
    disconnect(&app).await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_googles_answer_with_the_right_state_counts() {
        let req = |target: &str| format!("GET {target} HTTP/1.1\r\nHost: 127.0.0.1\r\n\r\n");
        assert!(matches!(read_reply(&req("/?state=abc&code=4%2F0xyz&scope=x"), "abc"), Reply::Code(c) if c == "4/0xyz"));
        assert!(matches!(read_reply(&req("/?state=abc&error=access_denied"), "abc"), Reply::Refused(_)));
        // Another page's request, or a forged one, is not taken.
        assert!(matches!(read_reply(&req("/?state=other&code=x"), "abc"), Reply::Other));
        assert!(matches!(read_reply(&req("/favicon.ico"), "abc"), Reply::Other));
        assert!(matches!(read_reply("", "abc"), Reply::Other));
    }

    #[test]
    fn drive_addresses_are_only_file_ids() {
        assert_eq!(file_of("gdrive:1AbC_d-9"), Some("1AbC_d-9"));
        assert_eq!(file_of("gdrive:../x"), None);
        assert_eq!(file_of("gdrive:"), None);
        assert_eq!(file_of("https://example.com"), None);
    }

    #[test]
    fn drive_queries_are_quoted() {
        assert_eq!(quoted("it's"), "'it\\'s'");
    }
}
