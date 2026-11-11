//! `eudora-web`: Eudora in a web browser, made for phones (and fine on a computer): read,
//! search, write, reply, forward, delete, move. HTTPS only (its own certificate until a real one
//! is set up), one password and passkeys, a session per device.
//!
//!   eudora-web --library C:\EudoraTest\library [--port 443] [--listen 0.0.0.0]
//!
//! The first password is set in a browser **on this computer** (https://localhost); other
//! devices can only sign in. `--reset-password` forgets it (and signs every device out).
//!
//! It opens the library like the desktop viewer does, beside the download (the library's only
//! writer for messages); moves, deletes and drafts are written to the database as the viewer's
//! are, and sent copies come in through the download.

mod auth;
mod tls;

use std::net::{IpAddr, SocketAddr};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use axum::extract::{ConnectInfo, Path as UrlPath, Query, Request, State};
use axum::http::{HeaderMap, HeaderValue, Method, StatusCode, header};
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use clap::Parser;
use eudora_core::library::{Access, MessageRow, PageCursor};
use eudora_core::{Library, Silent, SortOrder};
use serde::{Deserialize, Serialize};

#[derive(Parser)]
#[command(name = "eudora-web", about = "Eudora in a web browser (phones too): HTTPS, a password and passkeys")]
struct Args {
    /// The library (the folder with eudora.db).
    #[arg(long, short = 'l', env = "EUDORA_LIBRARY")]
    library: PathBuf,
    /// The port to listen on.
    #[arg(long, short = 'p', default_value_t = 443)]
    port: u16,
    /// The address to listen on: 0.0.0.0 = every network (phones on this network can connect),
    /// 127.0.0.1 = this computer only.
    #[arg(long, default_value = "0.0.0.0")]
    listen: IpAddr,
    /// More names for the certificate (a DNS name this computer is reached by).
    #[arg(long = "name")]
    names: Vec<String>,
    /// A certificate chain of your own (PEM; then also --key) instead of Eudora's own.
    #[arg(long, requires = "key")]
    cert: Option<PathBuf>,
    /// The private key for --cert (PEM).
    #[arg(long, requires = "cert")]
    key: Option<PathBuf>,
    /// Forget the password and sign every device out; set a new one from this computer's browser.
    #[arg(long)]
    reset_password: bool,
}

struct AppState {
    library: Mutex<Library>,
    root: PathBuf,
    auth: auth::Auth,
    /// The user's own addresses' ids (All Mail without what they sent), looked up again after a minute.
    own: Mutex<Option<(Instant, Vec<i64>)>>,
    /// Accounts and folders (counting them reads every message's places), for a minute.
    folders: Mutex<Option<(Instant, serde_json::Value)>>,
}

type Shared = Arc<AppState>;

// ------------------------------------------------------------------ errors

struct ApiError(StatusCode, String);

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        (self.0, Json(serde_json::json!({ "error": self.1 }))).into_response()
    }
}

impl<E: std::fmt::Display> From<E> for ApiError {
    fn from(e: E) -> Self {
        ApiError(StatusCode::INTERNAL_SERVER_ERROR, e.to_string())
    }
}

fn bad(status: StatusCode, text: impl Into<String>) -> ApiError {
    ApiError(status, text.into())
}

type Reply<T> = Result<Json<T>, ApiError>;

/// Runs `f` with the library on a blocking thread (it's SQLite and Tantivy: no async).
async fn with<T: Send + 'static>(state: &Shared, f: impl FnOnce(&Library) -> eudora_core::Result<T> + Send + 'static) -> Result<T, ApiError> {
    let state = state.clone();
    tokio::task::spawn_blocking(move || {
        let library = state.library.lock().map_err(|_| bad(StatusCode::INTERNAL_SERVER_ERROR, "the library is unavailable"))?;
        f(&library).map_err(|e| bad(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))
    })
    .await
    .map_err(|e| bad(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?
}

impl AppState {
    fn own_ids(&self, l: &Library) -> eudora_core::Result<Vec<i64>> {
        let mut own = self.own.lock().unwrap();
        if let Some((at, ids)) = own.as_ref() {
            if at.elapsed() < Duration::from_secs(60) {
                return Ok(ids.clone());
            }
        }
        let ids = l.address_ids(&l.own_addresses()?)?;
        *own = Some((Instant::now(), ids.clone()));
        Ok(ids)
    }
}

// ------------------------------------------------------------------ who's asking

const COOKIE: &str = "eudora_session";

fn cookie_token(headers: &HeaderMap) -> Option<String> {
    headers.get_all(header::COOKIE).iter().filter_map(|v| v.to_str().ok()).flat_map(|v| v.split(';')).find_map(|c| c.trim().strip_prefix(&format!("{COOKIE}=")).map(str::to_string))
}

fn session_cookie(token: &str) -> HeaderValue {
    HeaderValue::from_str(&format!("{COOKIE}={token}; Path=/; Max-Age={}; HttpOnly; Secure; SameSite=Strict", auth::SESSION_DAYS * 86400)).unwrap()
}

fn cleared_cookie() -> HeaderValue {
    HeaderValue::from_static("eudora_session=; Path=/; Max-Age=0; HttpOnly; Secure; SameSite=Strict")
}

/// The host as the browser named it ("mypc", "mypc:8443"): the passkeys' site and origin.
fn host(headers: &HeaderMap) -> String {
    headers.get(header::HOST).and_then(|h| h.to_str().ok()).unwrap_or("localhost").to_lowercase()
}

fn rp_id(headers: &HeaderMap) -> String {
    let h = host(headers);
    // "[::1]:443" / "mypc:443" → the name alone
    if let Some(rest) = h.strip_prefix('[') {
        return rest.split(']').next().unwrap_or("").to_string();
    }
    h.rsplit_once(':').filter(|(_, port)| port.chars().all(|c| c.is_ascii_digit())).map(|(name, _)| name.to_string()).unwrap_or(h)
}

fn origin(headers: &HeaderMap) -> String {
    format!("https://{}", host(headers))
}

/// "Safari on iPhone", "Edge on Windows"…
fn device(headers: &HeaderMap) -> String {
    let ua = headers.get(header::USER_AGENT).and_then(|h| h.to_str().ok()).unwrap_or("");
    let system = ["iPhone", "iPad", "Android", "Windows", "Macintosh", "Linux"].into_iter().find(|s| ua.contains(s)).unwrap_or("a device");
    let browser = if ua.contains("Edg/") {
        "Edge"
    } else if ua.contains("Firefox/") {
        "Firefox"
    } else if ua.contains("Chrome/") || ua.contains("CriOS/") {
        "Chrome"
    } else if ua.contains("Safari/") {
        "Safari"
    } else {
        "A browser"
    };
    format!("{browser} on {}", if system == "Macintosh" { "Mac" } else { system })
}

/// Every /api request but signing in needs a session; every change needs our own header too
/// (a page on another site can't send it without asking first, which we never allow).
async fn guard(State(state): State<Shared>, request: Request, next: Next) -> Response {
    let path = request.uri().path().to_string();
    if request.method() != Method::GET && request.headers().get("x-eudora").is_none() {
        return bad(StatusCode::FORBIDDEN, "missing request header").into_response();
    }
    let public = ["/api/me", "/api/setup", "/api/login", "/api/passkey/login-options", "/api/passkey/login"];
    if path.starts_with("/api/") && !public.contains(&path.as_str()) {
        let signed_in = cookie_token(request.headers()).and_then(|t| state.auth.session(&t)).is_some();
        if !signed_in {
            return bad(StatusCode::UNAUTHORIZED, "sign in first").into_response();
        }
    }
    next.run(request).await
}

/// Security headers on every answer.
async fn headers_on(request: Request, next: Next) -> Response {
    let mut response = next.run(request).await;
    let h = response.headers_mut();
    h.insert(header::STRICT_TRANSPORT_SECURITY, HeaderValue::from_static("max-age=31536000"));
    h.insert(header::X_CONTENT_TYPE_OPTIONS, HeaderValue::from_static("nosniff"));
    h.insert(header::REFERRER_POLICY, HeaderValue::from_static("no-referrer"));
    h.insert(header::X_FRAME_OPTIONS, HeaderValue::from_static("DENY"));
    // messages are shown in a sandboxed frame (srcdoc), which inherits this: no scripts in
    // them, pictures only as data: (web pictures are fetched by Eudora when asked)
    h.insert(
        header::CONTENT_SECURITY_POLICY,
        HeaderValue::from_static("default-src 'self'; script-src 'self'; style-src 'self' 'unsafe-inline'; img-src 'self' data:; font-src 'self' data:; connect-src 'self'; frame-src 'self'; frame-ancestors 'none'; base-uri 'none'; form-action 'self'"),
    );
    if !h.contains_key(header::CACHE_CONTROL) {
        h.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    }
    response
}

// ------------------------------------------------------------------ signing in

#[derive(Serialize)]
struct Me {
    signed_in: bool,
    password_set: bool,
    passkeys: usize,
    /// This browser is on the computer itself (the first password is set from here).
    local: bool,
    /// Reached by a name (passkeys need one: not an IP address).
    by_name: bool,
}

async fn me(State(state): State<Shared>, ConnectInfo(peer): ConnectInfo<SocketAddr>, headers: HeaderMap) -> Json<Me> {
    let rp = rp_id(&headers);
    Json(Me {
        signed_in: cookie_token(&headers).and_then(|t| state.auth.session(&t)).is_some(),
        password_set: state.auth.password_set(),
        passkeys: state.auth.passkey_count(),
        local: peer.ip().is_loopback(),
        by_name: rp.parse::<IpAddr>().is_err(),
    })
}

#[derive(Deserialize)]
struct PasswordForm {
    password: String,
}

async fn setup(State(state): State<Shared>, ConnectInfo(peer): ConnectInfo<SocketAddr>, headers: HeaderMap, Json(form): Json<PasswordForm>) -> Result<Response, ApiError> {
    if state.auth.password_set() {
        return Err(bad(StatusCode::CONFLICT, "a password is set already"));
    }
    if !peer.ip().is_loopback() {
        return Err(bad(StatusCode::FORBIDDEN, "set the first password from a browser on the computer Eudora runs on (https://localhost)"));
    }
    state.auth.set_password(&form.password).map_err(|e| bad(StatusCode::BAD_REQUEST, e))?;
    let token = state.auth.sign_in(&form.password, peer.ip(), &device(&headers)).map_err(|e| bad(StatusCode::BAD_REQUEST, e))?;
    Ok(([(header::SET_COOKIE, session_cookie(&token))], Json(serde_json::json!({ "ok": true }))).into_response())
}

async fn login(State(state): State<Shared>, ConnectInfo(peer): ConnectInfo<SocketAddr>, headers: HeaderMap, Json(form): Json<PasswordForm>) -> Result<Response, ApiError> {
    let auth_state = state.clone();
    let (ip, dev) = (peer.ip(), device(&headers));
    // (Argon2 takes a moment: off the async threads)
    let token = tokio::task::spawn_blocking(move || auth_state.auth.sign_in(&form.password, ip, &dev)).await?.map_err(|e| bad(StatusCode::UNAUTHORIZED, e))?;
    Ok(([(header::SET_COOKIE, session_cookie(&token))], Json(serde_json::json!({ "ok": true }))).into_response())
}

async fn logout(State(state): State<Shared>, headers: HeaderMap) -> Result<Response, ApiError> {
    if let Some(token) = cookie_token(&headers) {
        state.auth.end_session_token(&token)?;
    }
    Ok(([(header::SET_COOKIE, cleared_cookie())], Json(serde_json::json!({ "ok": true }))).into_response())
}

#[derive(Deserialize)]
struct ChangePassword {
    current: String,
    new: String,
}

async fn change_password(State(state): State<Shared>, ConnectInfo(peer): ConnectInfo<SocketAddr>, headers: HeaderMap, Json(form): Json<ChangePassword>) -> Reply<serde_json::Value> {
    let s = state.clone();
    let dev = device(&headers);
    tokio::task::spawn_blocking(move || -> Result<(), String> {
        let token = s.auth.sign_in(&form.current, peer.ip(), &dev)?;
        s.auth.end_session_token(&token)?;
        s.auth.set_password(&form.new)
    })
    .await?
    .map_err(|e| bad(StatusCode::BAD_REQUEST, e))?;
    Ok(Json(serde_json::json!({ "ok": true })))
}

async fn passkey_login_options(State(state): State<Shared>, headers: HeaderMap) -> Json<serde_json::Value> {
    Json(state.auth.login_options(&rp_id(&headers)))
}

async fn passkey_login(State(state): State<Shared>, ConnectInfo(peer): ConnectInfo<SocketAddr>, headers: HeaderMap, Json(answer): Json<auth::LoginAnswer>) -> Result<Response, ApiError> {
    let token = state.auth.passkey_sign_in(&rp_id(&headers), &origin(&headers), &answer, peer.ip(), &device(&headers)).map_err(|e| bad(StatusCode::UNAUTHORIZED, e))?;
    Ok(([(header::SET_COOKIE, session_cookie(&token))], Json(serde_json::json!({ "ok": true }))).into_response())
}

async fn passkey_register_options(State(state): State<Shared>, headers: HeaderMap) -> Json<serde_json::Value> {
    Json(state.auth.register_options(&rp_id(&headers)))
}

async fn passkey_register(State(state): State<Shared>, headers: HeaderMap, Json(answer): Json<auth::RegisterAnswer>) -> Reply<serde_json::Value> {
    state.auth.register(&rp_id(&headers), &origin(&headers), &answer).map_err(|e| bad(StatusCode::BAD_REQUEST, e))?;
    Ok(Json(serde_json::json!({ "ok": true })))
}

#[derive(Serialize)]
struct Security {
    passkeys: Vec<auth::Passkey>,
    sessions: Vec<auth::Session>,
    /// This device's session (its hash).
    this: Option<String>,
}

async fn security(State(state): State<Shared>, headers: HeaderMap) -> Json<Security> {
    let this = cookie_token(&headers).and_then(|t| state.auth.session(&t)).map(|s| s.hash);
    Json(Security { passkeys: state.auth.passkeys(), sessions: state.auth.sessions(), this })
}

#[derive(Deserialize)]
struct IdForm {
    id: String,
}

async fn remove_passkey(State(state): State<Shared>, Json(form): Json<IdForm>) -> Reply<serde_json::Value> {
    state.auth.remove_passkey(&form.id)?;
    Ok(Json(serde_json::json!({ "ok": true })))
}

async fn end_session(State(state): State<Shared>, Json(form): Json<IdForm>) -> Reply<serde_json::Value> {
    state.auth.end_session(&form.id)?;
    Ok(Json(serde_json::json!({ "ok": true })))
}

// ------------------------------------------------------------------ mail

#[derive(Serialize)]
struct Row {
    id: i64,
    date: i64,
    from: String,
    to: String,
    subject: String,
    snippet: String,
    size: u64,
    attachments: u64,
    /// The folders it's in (id, name).
    labels: Vec<(i64, String)>,
}

fn rows(l: &Library, rows: Vec<MessageRow>) -> eudora_core::Result<Vec<Row>> {
    let ids: Vec<i64> = rows.iter().map(|r| r.id).collect();
    let labels = l.labels_of(&ids)?;
    let recipients = l.recipients_of(&ids)?;
    Ok(rows
        .into_iter()
        .zip(labels)
        .zip(recipients)
        .map(|((r, labels), (to, _))| Row { id: r.id, date: r.date, from: r.from, to, subject: r.subject, snippet: r.snippet, size: r.size, attachments: r.attachments, labels })
        .collect())
}

/// Accounts and their folders, and how many messages All Mail has.
async fn folders(State(state): State<Shared>, Query(q): Query<HashQuery>) -> Reply<serde_json::Value> {
    if q.fresh.is_none() {
        if let Some((at, v)) = state.folders.lock().unwrap().as_ref() {
            if at.elapsed() < Duration::from_secs(60) {
                return Ok(Json(v.clone()));
            }
        }
    }
    let s = state.clone();
    let value = with(&state, move |l| {
        l.refresh()?;
        let accounts: Vec<serde_json::Value> = l
            .accounts()?
            .into_iter()
            .map(|a| serde_json::json!({ "id": a.id, "shown": a.shown(), "kind": a.kind, "kind_name": a.kind_name, "address": a.address, "messages": a.messages }))
            .collect();
        let folders: Vec<serde_json::Value> =
            l.folders()?.into_iter().map(|f| serde_json::json!({ "id": f.id, "path": f.path, "messages": f.messages, "account_id": f.account_id })).collect();
        let own = s.own_ids(l)?;
        Ok(serde_json::json!({ "accounts": accounts, "folders": folders, "all": l.count_all_except_from(&own)?, "all_with_sent": l.count(&[])? }))
    })
    .await?;
    *state.folders.lock().unwrap() = Some((Instant::now(), value.clone()));
    Ok(Json(value))
}

#[derive(Deserialize)]
struct HashQuery {
    fresh: Option<u8>,
}

#[derive(Deserialize)]
struct ListQuery {
    /// Folder ids, comma-separated; empty = All Mail.
    #[serde(default)]
    folders: String,
    /// Continue after this row (its date and id): the next page.
    after_date: Option<i64>,
    after_id: Option<i64>,
    size: Option<usize>,
    /// All Mail without what the user sent.
    hide_sent: Option<u8>,
}

fn folder_ids(text: &str) -> Vec<i64> {
    text.split(',').filter_map(|s| s.trim().parse().ok()).collect()
}

async fn list(State(state): State<Shared>, Query(q): Query<ListQuery>) -> Reply<Vec<Row>> {
    let folders = folder_ids(&q.folders);
    let after = match (q.after_date, q.after_id) {
        (Some(date), Some(id)) => Some(PageCursor { date, id }),
        _ => None,
    };
    let size = q.size.unwrap_or(50).clamp(1, 200);
    let hide = folders.is_empty() && q.hide_sent == Some(1);
    let s = state.clone();
    Ok(Json(
        with(&state, move |l| {
            let found = if hide { l.list_all_except_from(&s.own_ids(l)?, after, size)? } else { l.list(&folders, after, size)? };
            rows(l, found)
        })
        .await?,
    ))
}

#[derive(Deserialize)]
struct SearchQuery {
    q: String,
    limit: Option<usize>,
    order: Option<String>,
}

async fn search(State(state): State<Shared>, Query(q): Query<SearchQuery>) -> Reply<serde_json::Value> {
    let order = match q.order.as_deref() {
        Some("oldest") => SortOrder::Oldest,
        Some("relevance") => SortOrder::Relevance,
        _ => SortOrder::Newest,
    };
    let limit = q.limit.unwrap_or(200).clamp(1, 2000);
    Ok(Json(
        with(&state, move |l| {
            l.refresh()?;
            let (result, found) = l.search(&q.q, limit, order)?;
            let coverage = if eudora_core::header_index::uses_headers(&q.q) { Some(l.header_coverage()?) } else { None };
            Ok(serde_json::json!({ "total": result.total, "millis": result.took.as_secs_f64() * 1000.0, "rows": rows(l, found)?, "header_coverage": coverage, "notes": result.notes, "suggestions": result.suggestions }))
        })
        .await?,
    ))
}

async fn message(State(state): State<Shared>, UrlPath(id): UrlPath<i64>) -> Reply<eudora_core::outgoing::MessageView> {
    Ok(Json(with(&state, move |l| l.message_view(id)).await?))
}

async fn attachment(State(state): State<Shared>, UrlPath((id, part)): UrlPath<(i64, String)>) -> Result<Response, ApiError> {
    let view_part = part.clone();
    let (name, (mime, bytes)) = with(&state, move |l| {
        let name = l.message_view(id)?.attachments.into_iter().find(|a| a.part == view_part).map(|a| a.name).unwrap_or_else(|| "attachment".into());
        Ok((name, l.attachment_bytes(id, &view_part)?))
    })
    .await?;
    let safe: String = name.chars().map(|c| if c.is_ascii_alphanumeric() || " ._-()".contains(c) { c } else { '_' }).collect();
    let disposition = format!("attachment; filename=\"{safe}\"");
    Ok((
        [
            (header::CONTENT_TYPE, HeaderValue::from_str(&mime).unwrap_or(HeaderValue::from_static("application/octet-stream"))),
            (header::CONTENT_DISPOSITION, HeaderValue::from_str(&disposition).unwrap_or(HeaderValue::from_static("attachment"))),
        ],
        bytes,
    )
        .into_response())
}

#[derive(Deserialize)]
struct PicturesForm {
    urls: Vec<String>,
}

/// Fetches a message's web pictures for it (the message itself never goes online).
async fn pictures(Json(form): Json<PicturesForm>) -> Reply<Vec<eudora_core::remote_images::Picture>> {
    let urls: Vec<String> = form.urls.into_iter().take(200).collect();
    Ok(Json(tokio::task::spawn_blocking(move || eudora_core::remote_images::fetch(&urls, None)).await?))
}

#[derive(Deserialize)]
struct ComposeQuery {
    mode: String,
    id: Option<i64>,
    #[serde(default)]
    ids: String,
}

async fn compose(State(state): State<Shared>, Query(q): Query<ComposeQuery>) -> Reply<eudora_core::outgoing::ComposeStart> {
    let ids = folder_ids(&q.ids);
    Ok(Json(with(&state, move |l| l.compose_start(&q.mode, q.id, &ids)).await?))
}

async fn send(State(state): State<Shared>, Json(draft): Json<eudora_core::compose::Draft>) -> Reply<serde_json::Value> {
    let origin = draft.origin.clone();
    let outgoing = with(&state, move |l| l.prepare_send(draft)).await?;
    let id = tokio::task::spawn_blocking(move || outgoing.send()).await?.map_err(|e| bad(StatusCode::BAD_GATEWAY, e.to_string()))?;
    let _ = with(&state, move |l| l.mark_origin(&origin)).await;
    let _ = eudora_core::graph_sync::request_check(&state.root);
    Ok(Json(serde_json::json!({ "message_id": id })))
}

#[derive(Deserialize)]
struct DraftForm {
    id: Option<i64>,
    draft: eudora_core::compose::Draft,
}

async fn save_draft(State(state): State<Shared>, Json(form): Json<DraftForm>) -> Reply<i64> {
    let json = serde_json::to_string(&form.draft)?;
    Ok(Json(with(&state, move |l| l.save_draft(form.id, &json)).await?))
}

#[derive(Deserialize)]
struct MoveForm {
    ids: Vec<i64>,
    from: i64,
    to: Option<i64>,
}

async fn move_messages(State(state): State<Shared>, Json(form): Json<MoveForm>) -> Reply<u64> {
    let to = form.to.ok_or_else(|| bad(StatusCode::BAD_REQUEST, "where to?"))?;
    let n = with(&state, move |l| l.move_messages(&form.ids, form.from, to)).await?;
    *state.folders.lock().unwrap() = None;
    Ok(Json(n))
}

/// Delete: to the account's Trash; returns the Trash folder (for Undo).
async fn delete_messages(State(state): State<Shared>, Json(form): Json<MoveForm>) -> Reply<i64> {
    let trash = with(&state, move |l| {
        let account: i64 = l.connection().query_row("SELECT account FROM folders WHERE id = ?1", [form.from], |r| r.get(0))?;
        let trash = l.trash_folder(account)?;
        l.move_messages(&form.ids, form.from, trash)?;
        Ok(trash)
    })
    .await?;
    *state.folders.lock().unwrap() = None;
    Ok(Json(trash))
}

async fn copy_messages(State(state): State<Shared>, Json(form): Json<MoveForm>) -> Reply<u64> {
    let to = form.to.ok_or_else(|| bad(StatusCode::BAD_REQUEST, "where to?"))?;
    Ok(Json(with(&state, move |l| l.copy_messages(&form.ids, to)).await?))
}

async fn check_mail(State(state): State<Shared>) -> Reply<bool> {
    eudora_core::graph_sync::request_check(&state.root)?;
    let running = eudora_core::graph_sync::read_status(&state.root).is_some_and(|s| eudora_core::library::now() - s.updated < 90);
    Ok(Json(running))
}

async fn status(State(state): State<Shared>) -> Json<serde_json::Value> {
    let status = eudora_core::graph_sync::read_status(&state.root);
    let age = status.as_ref().map(|s| eudora_core::library::now() - s.updated);
    Json(serde_json::json!({ "status": status, "age": age }))
}

#[derive(Deserialize)]
struct CompleteQuery {
    q: String,
}

async fn complete(State(state): State<Shared>, Query(q): Query<CompleteQuery>) -> Reply<Vec<serde_json::Value>> {
    Ok(Json(
        with(&state, move |l| {
            let c: serde_json::Value = l.setting("completion")?.and_then(|j| serde_json::from_str(&j).ok()).unwrap_or(serde_json::json!({}));
            let (sent, received) = (c["sent"].as_bool().unwrap_or(true), c["received"].as_bool().unwrap_or(true));
            Ok(l.complete_address(&q.q, sent, received, 8)?.into_iter().map(|(name, address)| serde_json::json!({ "name": name, "address": address })).collect())
        })
        .await?,
    ))
}

// ------------------------------------------------------------------ the page itself

macro_rules! file {
    ($path:literal, $mime:literal) => {
        ([(header::CONTENT_TYPE, $mime), (header::CACHE_CONTROL, "no-cache")], include_str!(concat!("../ui/", $path)))
    };
}

async fn index() -> impl IntoResponse {
    file!("index.html", "text/html; charset=utf-8")
}
async fn script() -> impl IntoResponse {
    file!("app.js", "text/javascript; charset=utf-8")
}
async fn style() -> impl IntoResponse {
    file!("app.css", "text/css; charset=utf-8")
}
async fn manifest() -> impl IntoResponse {
    file!("manifest.webmanifest", "application/manifest+json")
}

/// The icons the page uses: Font Awesome Free (CC BY 4.0), the same ones the desktop viewer commits.
async fn icon(UrlPath(name): UrlPath<String>) -> Response {
    macro_rules! icons {
        ($($n:literal),*) => {
            match name.trim_end_matches(".svg") {
                $($n => include_str!(concat!("../../eudora-app/ui/icons/free/", $n, ".svg")),)*
                _ => return StatusCode::NOT_FOUND.into_response(),
            }
        };
    }
    let svg = icons!("arrow-left", "arrows-rotate", "folder-open", "folder", "gear", "inbox", "layer-group", "list", "magnifying-glass", "paper-plane", "paperclip", "pen-to-square", "reply-all", "reply", "share", "trash-can", "users", "xmark", "envelope", "copy", "box-archive");
    ([(header::CONTENT_TYPE, "image/svg+xml"), (header::CACHE_CONTROL, "max-age=86400")], svg).into_response()
}

/// The certificate authority Eudora made, to install on a phone (a public certificate: no
/// sign-in needed).
async fn ca_certificate(State(state): State<Shared>) -> Result<Response, ApiError> {
    let pem = std::fs::read(state.root.join("web").join("tls").join("ca.pem")).map_err(|_| bad(StatusCode::NOT_FOUND, "Eudora isn't using its own certificate"))?;
    Ok(([(header::CONTENT_TYPE, "application/x-x509-ca-cert"), (header::CONTENT_DISPOSITION, "attachment; filename=\"eudora-local-ca.crt\"")], pem).into_response())
}

fn router(state: Shared) -> Router {
    let api = Router::new()
        .route("/api/me", get(me))
        .route("/api/setup", post(setup))
        .route("/api/login", post(login))
        .route("/api/logout", post(logout))
        .route("/api/password", post(change_password))
        .route("/api/passkey/login-options", get(passkey_login_options))
        .route("/api/passkey/login", post(passkey_login))
        .route("/api/passkey/register-options", get(passkey_register_options))
        .route("/api/passkey/register", post(passkey_register))
        .route("/api/security", get(security))
        .route("/api/passkeys/remove", post(remove_passkey))
        .route("/api/sessions/end", post(end_session))
        .route("/api/folders", get(folders))
        .route("/api/list", get(list))
        .route("/api/search", get(search))
        .route("/api/message/{id}", get(message))
        .route("/api/message/{id}/attachment/{part}", get(attachment))
        .route("/api/pictures", post(pictures))
        .route("/api/compose", get(compose))
        .route("/api/send", post(send))
        .route("/api/draft", post(save_draft))
        .route("/api/move", post(move_messages))
        .route("/api/delete", post(delete_messages))
        .route("/api/copy", post(copy_messages))
        .route("/api/check-mail", post(check_mail))
        .route("/api/status", get(status))
        .route("/api/complete", get(complete));
    Router::new()
        .merge(api)
        .route("/", get(index))
        .route("/app.js", get(script))
        .route("/app.css", get(style))
        .route("/manifest.webmanifest", get(manifest))
        .route("/icons/{name}", get(icon))
        .route("/ca.crt", get(ca_certificate))
        .layer(middleware::from_fn_with_state(state.clone(), guard))
        .layer(middleware::from_fn(headers_on))
        .with_state(state)
}

#[tokio::main]
async fn main() {
    eudora_core::library::move_from_eudora_27();
    if let Err(e) = run().await {
        eprintln!("eudora-web: {e}");
        std::process::exit(1);
    }
}

async fn run() -> Result<(), String> {
    let args = Args::parse();
    let auth = auth::Auth::open(&args.library)?;
    if args.reset_password {
        auth.reset()?;
        println!("The password is forgotten and every device is signed out. Set a new one in a browser on this computer: https://localhost{}", if args.port == 443 { String::new() } else { format!(":{}", args.port) });
        return Ok(());
    }
    let library = Library::open(&args.library, Access::Read, &Silent).map_err(|e| format!("opening the library {}: {e}", args.library.display()))?;
    let (cert, key) = match (&args.cert, &args.key) {
        (Some(c), Some(k)) => (c.clone(), k.clone()),
        _ => {
            let names = tls::local_names(&args.names);
            let dir = tls::ensure_self_signed(&args.library, &names)?;
            println!("HTTPS with Eudora's own certificate for {}.", names.join(", "));
            println!("To trust it on a phone, install its certificate authority once: https://<this computer>/ca.crt");
            (dir.join("server.pem"), dir.join("server.key.pem"))
        }
    };
    let config = axum_server::tls_rustls::RustlsConfig::from_config(tls::server_config(&cert, &key)?);
    let state = Arc::new(AppState { library: Mutex::new(library), root: args.library.clone(), auth, own: Mutex::new(None), folders: Mutex::new(None) });
    let address = SocketAddr::new(args.listen, args.port);
    let shown = if args.listen.is_unspecified() { "every network address".to_string() } else { args.listen.to_string() };
    println!("Eudora on the web: https://localhost{} (listening on {shown}, port {})", if args.port == 443 { String::new() } else { format!(":{}", args.port) }, args.port);
    if !state.auth.password_set() {
        println!("No password yet: open that address in a browser on this computer to set one.");
    }
    axum_server::bind_rustls(address, config)
        .serve(router(state).into_make_service_with_connect_info::<SocketAddr>())
        .await
        .map_err(|e| format!("listening on {address}: {e} (another program on that port? try --port)"))
}
