use crate::auth::Authenticator;
use chuck_mgmt::{AdminError, AdminService};
use chuck_data::blob::{self, BlobStore};
use chuck_control::{AccountRecords, AccountService, AccountStatus, OwnerPurger, OwnerRecords, Principal, Store, DEFAULT_ACCOUNT_CACHE};
use chuck_core::media::{self, MediaError};
use chuck_data::store::{normalize_tag, NewAttachment, NoteInput, NoteQuery, RuleInput, StoreError};
use chuck_core::domain::input::{self, clean_name, InputError, MAX_BODY};
use serde::Deserialize;
use axum::{
    body::Bytes,
    extract::{DefaultBodyLimit, Path, Query, State},
    http::{header, HeaderMap, HeaderValue, StatusCode},
    response::{Html, IntoResponse, Response},
    routing::{delete, get, post, put},
    Json, Router,
};
use std::collections::HashSet;
use std::sync::Arc;
use std::time::Duration;

#[derive(Clone)]
pub struct AppState {
    pub store: Arc<dyn Store>,
    pub auth: Arc<dyn Authenticator>,
    pub blobs: Arc<dyn BlobStore>,
    /// The control plane: resolves identities to accounts and manages their lifecycle.
    pub accounts: Arc<AccountService>,
    /// The management plane: account administration and maintenance.
    pub admin: Arc<AdminService>,
    admins: Arc<HashSet<String>>,
    account_cache: Duration,
}

impl AppState {
    /// No administrators; account status is cached for `control::DEFAULT_ACCOUNT_CACHE`.
    pub fn new(store: Arc<dyn Store>, auth: Arc<dyn Authenticator>, blobs: Arc<dyn BlobStore>) -> Self {
        let admins = Arc::new(HashSet::new());
        let (accounts, admin) = Self::build(&store, &blobs, &admins, DEFAULT_ACCOUNT_CACHE);
        Self { store, auth, blobs, accounts, admin, admins, account_cache: DEFAULT_ACCOUNT_CACHE }
    }

    /// Owner ids that are administrators (`CHUCK_ADMINS`). Admins manage accounts; they cannot read anyone's content.
    pub fn with_admins(mut self, admins: impl IntoIterator<Item = impl Into<String>>) -> Self {
        self.admins = Arc::new(admins.into_iter().map(Into::into).collect());
        (self.accounts, self.admin) = Self::build(&self.store, &self.blobs, &self.admins, self.account_cache);
        self
    }

    /// How long a resolved account is reused before its record is read again (default 30 seconds).
    pub fn with_account_cache(mut self, ttl: Duration) -> Self {
        self.account_cache = ttl;
        (self.accounts, self.admin) = Self::build(&self.store, &self.blobs, &self.admins, ttl);
        self
    }

    fn build(store: &Arc<dyn Store>, blobs: &Arc<dyn BlobStore>, admins: &HashSet<String>, ttl: Duration) -> (Arc<AccountService>, Arc<AdminService>) {
        let accounts: Arc<dyn AccountRecords> = store.clone();
        let records: Arc<dyn OwnerRecords> = store.clone();
        let data = Arc::new(OwnerPurger::new(records.clone(), blobs.clone()));
        let accounts = Arc::new(AccountService::new(accounts, data).with_admins(admins.iter().cloned()).with_cache_ttl(ttl));
        let admin = Arc::new(AdminService::new(accounts.clone(), records, blobs.clone()));
        (accounts, admin)
    }
}

/// Authenticates every /api request once, before any body or query is parsed, and hands the handler a `Principal`.
/// Anonymous callers get 401, so a malformed request cannot learn anything (such as a 422 that shows which route
/// exists) without signing in. Disabled and deleting accounts are refused here, which makes that apply to every
/// endpoint; /api/admin is refused to everyone but administrators.
async fn require_principal(axum::extract::State(s): axum::extract::State<AppState>, mut req: axum::extract::Request, next: axum::middleware::Next) -> Response {
    let path = req.uri().path().to_string();
    if !path.starts_with("/api/") {
        return next.run(req).await;
    }
    // Authentication may fetch signing keys over the network, and the account may be read or created, so this runs on a
    // blocking thread.
    let (auth, accounts, headers) = (s.auth.clone(), s.accounts.clone(), req.headers().clone());
    let found = tokio::task::spawn_blocking(move || -> chuck_data::store::Result<Option<Principal>> {
        let Some(who) = auth.identity(&headers) else { return Ok(None) };
        accounts.resolve(&who).map(Some)
    })
    .await;
    let principal = match found {
        Err(e) => return ApiError(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response(),
        Ok(Err(e)) => return ApiError::from(e).into_response(),
        Ok(Ok(None)) => return ApiError(StatusCode::UNAUTHORIZED, "unauthorized".into()).into_response(),
        Ok(Ok(Some(p))) => p,
    };
    match principal.status {
        AccountStatus::Active => {}
        AccountStatus::Disabled => return ApiError(StatusCode::UNAUTHORIZED, "this account is disabled".into()).into_response(),
        AccountStatus::Deleting => return ApiError(StatusCode::UNAUTHORIZED, "this account is being deleted".into()).into_response(),
    }
    if path.starts_with("/api/admin/") && !principal.is_admin() {
        return ApiError(StatusCode::FORBIDDEN, "administrators only".into()).into_response();
    }
    req.extensions_mut().insert(principal);
    next.run(req).await
}

/// The signed-in account for this request, as set by `require_principal`. A handler that takes `Authed` can only run for
/// an authenticated, active account (and, under /api/admin, an administrator).
pub struct Authed(pub Principal);

#[axum::async_trait]
impl<S: Send + Sync> axum::extract::FromRequestParts<S> for Authed {
    type Rejection = ApiError;

    async fn from_request_parts(parts: &mut axum::http::request::Parts, _: &S) -> Result<Self, Self::Rejection> {
        parts.extensions.get::<Principal>().cloned().map(Authed).ok_or(ApiError(StatusCode::UNAUTHORIZED, "unauthorized".into()))
    }
}

/// The three planes of the API (see docs/specs/service-layer.md). Each has its own routes and its own Smithy service, so a
/// host can serve any combination: everything in one process, or one function per plane behind a gateway.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Plane {
    /// A user's cards, tags, rules, files and tag suggestions. Smithy service `ChuckData`.
    Data,
    /// A user's own account. Smithy service `ChuckControl`.
    Control,
    /// Administrators: accounts and maintenance. Smithy service `ChuckAdmin`.
    Admin,
}

pub const ALL_PLANES: [Plane; 3] = [Plane::Data, Plane::Control, Plane::Admin];

/// `ChuckData`.
pub fn data_routes() -> Router<AppState> {
    Router::new()
        .route("/api/notes", get(list).post(create))
        .route("/api/notes/:id", put(update).delete(remove))
        .route("/api/notes/:id/move", post(move_note))
        .route("/api/tags", get(tags))
        .route("/api/export", get(export))
        .route("/api/suggest", post(suggest))
        .route("/api/suggest/feedback", post(suggestion_feedback))
        .route("/api/rules", get(rules).post(add_rule))
        .route("/api/rules/apply", post(apply_rules))
        .route("/api/rules/:id", delete(delete_rule))
        .route(
            "/api/notes/:id/attachments",
            post(add_attachment).layer(DefaultBodyLimit::max(media::MAX_UPLOAD_BYTES + 1024)),
        )
        .route("/api/media/:id", get(get_media).delete(delete_media))
        .route("/api/media/:id/thumb", put(put_thumb).layer(DefaultBodyLimit::max(media::MAX_THUMB_BYTES + 1024)))
}

/// `ChuckControl`.
pub fn control_routes() -> Router<AppState> {
    Router::new().route("/api/me", get(me).delete(delete_me)).route("/api/me/export", get(export_me))
}

/// `ChuckAdmin`.
pub fn admin_routes() -> Router<AppState> {
    Router::new()
        .route("/api/admin/users", get(admin_users))
        .route("/api/admin/users/status", post(admin_user_status))
        .route("/api/admin/users/delete", post(admin_delete_user))
        .route("/api/admin/maintenance/sweep", post(admin_sweep))
}

/// All three planes in one router.
pub fn router(state: AppState) -> Router {
    router_for(state, &ALL_PLANES)
}

/// Only the given planes. The page and `/healthz` are always served (they carry no data). Requests for another plane's
/// paths are 404. Authentication and account checks apply the same way whichever planes are served.
pub fn router_for(state: AppState, planes: &[Plane]) -> Router {
    let mut r = Router::new().route("/", get(|| async { Html(include_str!("../static/index.html")) })).route("/healthz", get(|| async { "ok" }));
    for plane in planes {
        r = r.merge(match plane {
            Plane::Data => data_routes(),
            Plane::Control => control_routes(),
            Plane::Admin => admin_routes(),
        });
    }
    r.layer(axum::middleware::from_fn_with_state(state.clone(), require_principal)).with_state(state)
}

pub struct ApiError(StatusCode, String);

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        (self.0, Json(serde_json::json!({ "error": self.1 }))).into_response()
    }
}

impl From<AdminError> for ApiError {
    fn from(e: AdminError) -> Self {
        match e {
            AdminError::Forbidden => ApiError(StatusCode::FORBIDDEN, "administrators only".into()),
            AdminError::Invalid(m) => ApiError(StatusCode::BAD_REQUEST, m),
            AdminError::Store(e) => e.into(),
        }
    }
}

impl From<StoreError> for ApiError {
    fn from(e: StoreError) -> Self {
        match e {
            StoreError::NotFound => ApiError(StatusCode::NOT_FOUND, "not found".into()),
            StoreError::Invalid(m) => ApiError(StatusCode::BAD_REQUEST, m),
            StoreError::Conflict(m) => ApiError(StatusCode::CONFLICT, m),
            StoreError::Other(m) => {
                eprintln!("store error: {m}");
                ApiError(StatusCode::INTERNAL_SERVER_ERROR, "internal error".into())
            }
        }
    }
}

async fn blocking<T: Send + 'static>(f: impl FnOnce() -> chuck_data::store::Result<T> + Send + 'static) -> Result<T, ApiError> {
    tokio::task::spawn_blocking(f)
        .await
        .map_err(|e| ApiError(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?
        .map_err(Into::into)
}

/// `allow_empty`: a card may have no text when it holds media.
fn validate(mut i: NoteInput, allow_empty: bool) -> Result<NoteInput, ApiError> {
    i.body = input::checked_body(&i.body, i.repeat.as_deref(), allow_empty).map_err(|e| match e {
        InputError::EmptyBody => ApiError(StatusCode::BAD_REQUEST, "body is empty".into()),
        InputError::BodyTooLong => ApiError(StatusCode::PAYLOAD_TOO_LARGE, "note too long".into()),
        InputError::BadRepeat => ApiError(StatusCode::BAD_REQUEST, "invalid repeat".into()),
    })?;
    Ok(i)
}

#[derive(Deserialize)]
struct CreateQuery {
    /// Set when attachments will be uploaded next, so the card may start without text.
    media: Option<bool>,
}

async fn create(State(s): State<AppState>, Authed(p): Authed, Query(q): Query<CreateQuery>, Json(b): Json<NoteInput>) -> Result<impl IntoResponse, ApiError> {
    let o = p.id.clone();
    let input = validate(b, q.media.unwrap_or(false))?;
    let st = s.store.clone();
    let n = blocking(move || st.create(&o, &input)).await?;
    Ok((StatusCode::CREATED, Json(n)))
}

async fn update(State(s): State<AppState>, Authed(p): Authed, Path(id): Path<i64>, Json(b): Json<NoteInput>) -> Result<impl IntoResponse, ApiError> {
    let o = p.id.clone();
    let st = s.store.clone();
    let has_media = {
        let (st, o) = (st.clone(), o.clone());
        !blocking(move || st.get(&o, id)).await?.attachments.is_empty()
    };
    let input = validate(b, has_media)?;
    Ok(Json(blocking(move || st.update(&o, id, &input)).await?))
}

async fn remove(State(s): State<AppState>, Authed(p): Authed, Path(id): Path<i64>) -> Result<impl IntoResponse, ApiError> {
    let o = p.id.clone();
    let st = s.store.clone();
    // The files go with the card's last use; the sweeper deletes them once nothing refers to them.
    blocking(move || st.delete(&o, id)).await?;
    Ok(StatusCode::NO_CONTENT)
}

async fn list(State(s): State<AppState>, Authed(p): Authed, Query(q): Query<NoteQuery>) -> Result<impl IntoResponse, ApiError> {
    let o = p.id.clone();
    let st = s.store.clone();
    Ok(Json(serde_json::json!({ "notes": blocking(move || st.list(&o, &q)).await? })))
}

async fn tags(State(s): State<AppState>, Authed(p): Authed) -> Result<impl IntoResponse, ApiError> {
    let o = p.id.clone();
    let st = s.store.clone();
    Ok(Json(serde_json::json!({ "tags": blocking(move || st.tags(&o)).await? })))
}

#[derive(Deserialize)]
struct SuggestInput {
    text: String,
}

async fn suggest(State(s): State<AppState>, Authed(p): Authed, Json(b): Json<SuggestInput>) -> Result<impl IntoResponse, ApiError> {
    let o = p.id.clone();
    let text: String = b.text.chars().take(MAX_BODY).collect();
    let st = s.store.clone();
    Ok(Json(blocking(move || st.suggest_tags(&o, &text)).await?))
}

#[derive(Deserialize)]
struct FeedbackInput {
    tag: String,
    /// "accepted" or "dismissed"
    action: String,
    confidence: Option<f64>,
}

async fn suggestion_feedback(State(s): State<AppState>, Authed(p): Authed, Json(b): Json<FeedbackInput>) -> Result<impl IntoResponse, ApiError> {
    let o = p.id.clone();
    let tag = normalize_tag(&b.tag).ok_or(ApiError(StatusCode::BAD_REQUEST, "invalid tag".into()))?;
    let accepted = match b.action.as_str() {
        "accepted" => true,
        "dismissed" => false,
        _ => return Err(ApiError(StatusCode::BAD_REQUEST, "action must be accepted or dismissed".into())),
    };
    let confidence = b.confidence.filter(|c| c.is_finite()).unwrap_or(0.0);
    let st = s.store.clone();
    blocking(move || st.record_suggestion_feedback(&o, &tag, accepted, confidence)).await?;
    Ok(StatusCode::NO_CONTENT)
}

async fn rules(State(s): State<AppState>, Authed(p): Authed) -> Result<impl IntoResponse, ApiError> {
    let o = p.id.clone();
    let st = s.store.clone();
    Ok(Json(serde_json::json!({ "rules": blocking(move || st.rules(&o)).await? })))
}

async fn add_rule(State(s): State<AppState>, Authed(p): Authed, Json(b): Json<RuleInput>) -> Result<impl IntoResponse, ApiError> {
    let o = p.id.clone();
    let pattern = b.pattern.trim().to_string();
    if pattern.is_empty() || pattern.chars().count() > 200 || !pattern.split(|c| c == '|' || c == ',').any(|k| !k.trim().is_empty()) {
        return Err(ApiError(StatusCode::BAD_REQUEST, "keywords are empty or too long".into()));
    }
    let tag = normalize_tag(&b.tag).ok_or(ApiError(StatusCode::BAD_REQUEST, "invalid tag name".into()))?;
    let st = s.store.clone();
    let r = blocking(move || st.add_rule(&o, &pattern, &tag)).await?;
    Ok((StatusCode::CREATED, Json(r)))
}

async fn delete_rule(State(s): State<AppState>, Authed(p): Authed, Path(id): Path<i64>) -> Result<impl IntoResponse, ApiError> {
    let o = p.id.clone();
    let st = s.store.clone();
    blocking(move || st.delete_rule(&o, id)).await?;
    Ok(StatusCode::NO_CONTENT)
}

async fn apply_rules(State(s): State<AppState>, Authed(p): Authed) -> Result<impl IntoResponse, ApiError> {
    let o = p.id.clone();
    let st = s.store.clone();
    let changed = blocking(move || st.apply_rules(&o)).await?;
    Ok(Json(serde_json::json!({ "changed": changed })))
}

/// Markdown download of every note (or the notes matching the same filters as `/api/notes`; paging is ignored).
async fn export(State(s): State<AppState>, Authed(p): Authed, Query(mut q): Query<NoteQuery>) -> Result<impl IntoResponse, ApiError> {
    let o = p.id.clone();
    let st = s.store.clone();
    let notes = blocking(move || {
        const PAGE: u32 = 200;
        let mut all = Vec::new();
        let mut offset = 0;
        loop {
            q.limit = Some(PAGE);
            q.offset = Some(offset);
            let page = st.list(&o, &q)?;
            let n = page.len() as u32;
            all.extend(page);
            if n < PAGE {
                return Ok(all);
            }
            offset += PAGE;
        }
    })
    .await?;
    let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs() as i64).unwrap_or(0);
    let body = chuck_core::export::to_markdown(&notes, now);
    let disposition = format!("attachment; filename=\"chuck-export-{}.md\"", chuck_core::export::fmt_date(now));
    Ok((
        [
            (axum::http::header::CONTENT_TYPE, "text/markdown; charset=utf-8".to_string()),
            (axum::http::header::CONTENT_DISPOSITION, disposition),
            (axum::http::header::CACHE_CONTROL, "no-store".to_string()),
        ],
        body,
    ))
}

// ---------------------------------------------------------------- media

impl From<MediaError> for ApiError {
    fn from(e: MediaError) -> Self {
        let code = match e {
            MediaError::Unsupported => StatusCode::UNSUPPORTED_MEDIA_TYPE,
            MediaError::TooLarge => StatusCode::PAYLOAD_TOO_LARGE,
            MediaError::Malformed(_) => StatusCode::BAD_REQUEST,
        };
        ApiError(code, e.to_string())
    }
}

/// Deletes stored files that no attachment of this owner uses any more. Failures are logged, not returned: the
/// database change has already happened, and a leftover file is harmless.
#[derive(Deserialize)]
struct AttachQuery {
    name: Option<String>,
    /// Audio length measured by the browser. Display only; must be between 0 and 6 hours.
    duration_ms: Option<i64>,
}

/// Body: the raw file. The type is detected from the bytes; metadata is stripped before storing.
async fn add_attachment(State(s): State<AppState>, Authed(p): Authed, Path(note_id): Path<i64>, Query(q): Query<AttachQuery>, body: Bytes) -> Result<impl IntoResponse, ApiError> {
    let o = p.id.clone();
    let (st, blobs) = (s.store.clone(), s.blobs.clone());
    let name = clean_name(q.name);
    let res = tokio::task::spawn_blocking(move || -> Result<_, ApiError> {
        // Fail early, before any work, if the card is not this user's.
        st.get(&o, note_id)?;
        let c = media::check_upload(&body)?;
        let duration_ms = match q.duration_ms {
            Some(_) if !c.audio => None, // ignored for images
            Some(d) if !(0..=6 * 3600 * 1000).contains(&d) => return Err(ApiError(StatusCode::BAD_REQUEST, "invalid duration".into())),
            d => d,
        };
        let key = blob::key_for(&o, &c.bytes);
        blobs.put(&key, &c.bytes).map_err(|e| ApiError(StatusCode::INTERNAL_SERVER_ERROR, format!("storing file: {e}")))?;
        let new = NewAttachment {
            mime: c.mime.to_string(),
            bytes: c.bytes.len() as i64,
            width: c.width.map(|w| w as i64),
            height: c.height.map(|h| h as i64),
            duration_ms,
            name,
            sha256: blob::sha256_hex(&c.bytes),
            blob_key: key.clone(),
        };
        match st.add_attachment(&o, note_id, &new) {
            Ok(a) => Ok(a),
            Err(e) => Err(e.into()), // the stored file is deleted by the sweeper if nothing else uses it
        }
    })
    .await
    .map_err(|e| ApiError(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))??;
    Ok((StatusCode::CREATED, Json(res)))
}

/// Body: a small preview image for the attachment, made by the client.
async fn put_thumb(State(s): State<AppState>, Authed(p): Authed, Path(id): Path<i64>, body: Bytes) -> Result<impl IntoResponse, ApiError> {
    let o = p.id.clone();
    let (st, blobs) = (s.store.clone(), s.blobs.clone());
    tokio::task::spawn_blocking(move || -> Result<_, ApiError> {
        st.attachment(&o, id)?;
        let c = media::check_image(&body, media::MAX_THUMB_BYTES)?;
        if c.width > 1024 || c.height > 1024 {
            return Err(ApiError(StatusCode::BAD_REQUEST, "preview must be at most 1024 pixels on each side".into()));
        }
        let key = blob::key_for(&o, &c.bytes);
        blobs.put(&key, &c.bytes).map_err(|e| ApiError(StatusCode::INTERNAL_SERVER_ERROR, format!("storing file: {e}")))?;
        st.set_attachment_thumb(&o, id, &key)?; // the replaced preview is deleted by the sweeper
        Ok(())
    })
    .await
    .map_err(|e| ApiError(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))??;
    Ok(StatusCode::NO_CONTENT)
}

#[derive(Deserialize)]
struct MediaQuery {
    /// Serve the small preview when there is one.
    thumb: Option<bool>,
    /// The attachment's version, from its JSON. Only a URL with the current version may be cached for long.
    v: Option<String>,
}

async fn get_media(State(s): State<AppState>, Authed(p): Authed, h: HeaderMap, Path(id): Path<i64>, Query(q): Query<MediaQuery>) -> Result<Response, ApiError> {
    let o = p.id.clone();
    let if_none_match = h.get(header::IF_NONE_MATCH).and_then(|v| v.to_str().ok()).map(str::to_string);
    let wanted_v = q.v.clone();
    let (st, blobs) = (s.store.clone(), s.blobs.clone());
    let (a, bytes, is_thumb) = tokio::task::spawn_blocking(move || -> Result<_, ApiError> {
        let a = st.attachment(&o, id)?;
        let (key, is_thumb) = match (&a.thumb_key, q.thumb.unwrap_or(false)) {
            (Some(t), true) => (t.clone(), true),
            _ => (a.blob_key.clone(), false),
        };
        let bytes = blobs.get(&key).map_err(|_| ApiError(StatusCode::NOT_FOUND, "file missing".into()))?;
        Ok((a, bytes, is_thumb))
    })
    .await
    .map_err(|e| ApiError(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))??;
    let etag = format!("\"{}{}\"", a.v, if is_thumb { "-t" } else { "" });
    // Long caching only when the URL names the current version; anything else must be checked with the server, so a
    // reused id can never show an old picture.
    let cache = if wanted_v.as_deref() == Some(a.v.as_str()) { "private, max-age=31536000, immutable" } else { "private, no-cache" };
    if if_none_match.as_deref() == Some(etag.as_str()) {
        let mut r = StatusCode::NOT_MODIFIED.into_response();
        r.headers_mut().insert(header::ETAG, HeaderValue::from_str(&etag).unwrap());
        r.headers_mut().insert(header::CACHE_CONTROL, HeaderValue::from_static(cache));
        return Ok(r);
    }
    // The preview may be a different format from the original; detect it from its bytes.
    let mime = if is_thumb { media::sniff(&bytes).unwrap_or("application/octet-stream").to_string() } else { a.mime.clone() };
    // Byte ranges: Safari will not play audio without them, and every browser needs them to seek. A range only
    // applies to the version the client already has (If-Range); otherwise the whole file is sent.
    let total = bytes.len();
    let if_range_ok = h.get(header::IF_RANGE).and_then(|v| v.to_str().ok()).map_or(true, |v| v == etag);
    let range = h.get(header::RANGE).and_then(|v| v.to_str().ok()).filter(|_| if_range_ok).map(|r| parse_range(r, total));
    let (status, body, content_range) = match range {
        None => (StatusCode::OK, bytes, None),
        Some(Some((start, end))) => (StatusCode::PARTIAL_CONTENT, bytes[start..=end].to_vec(), Some(format!("bytes {start}-{end}/{total}"))),
        Some(None) => {
            let mut r = StatusCode::RANGE_NOT_SATISFIABLE.into_response();
            r.headers_mut().insert(header::CONTENT_RANGE, HeaderValue::from_str(&format!("bytes */{total}")).unwrap());
            return Ok(r);
        }
    };
    let mut resp = (status, body).into_response();
    let hd = resp.headers_mut();
    hd.insert(header::ACCEPT_RANGES, HeaderValue::from_static("bytes"));
    if let Some(cr) = content_range {
        hd.insert(header::CONTENT_RANGE, HeaderValue::from_str(&cr).unwrap());
    }
    hd.insert(header::ETAG, HeaderValue::from_str(&etag).unwrap());
    hd.insert(header::CONTENT_TYPE, HeaderValue::from_str(&mime).unwrap_or(HeaderValue::from_static("application/octet-stream")));
    hd.insert(header::X_CONTENT_TYPE_OPTIONS, HeaderValue::from_static("nosniff"));
    // Even if a file were opened directly in a tab, it could not run script or load anything.
    hd.insert(header::CONTENT_SECURITY_POLICY, HeaderValue::from_static("default-src 'none'; style-src 'unsafe-inline'; sandbox"));
    hd.insert(header::CACHE_CONTROL, HeaderValue::from_static(cache));
    hd.insert(header::CONTENT_DISPOSITION, HeaderValue::from_static("inline"));
    hd.insert("cross-origin-resource-policy", HeaderValue::from_static("same-origin"));
    Ok(resp)
}

async fn delete_media(State(s): State<AppState>, Authed(p): Authed, Path(id): Path<i64>) -> Result<impl IntoResponse, ApiError> {
    let o = p.id.clone();
    let st = s.store.clone();
    blocking(move || st.delete_attachment(&o, id).map(|_| ())).await?;
    Ok(StatusCode::NO_CONTENT)
}

#[derive(Deserialize)]
struct MoveInput {
    /// The card that will be directly above (before) this one, or null for the top of its group.
    prev: Option<i64>,
    /// The card that will be directly below (after) this one, or null for the bottom.
    next: Option<i64>,
}

async fn move_note(State(s): State<AppState>, Authed(p): Authed, Path(id): Path<i64>, Json(b): Json<MoveInput>) -> Result<impl IntoResponse, ApiError> {
    let o = p.id.clone();
    let st = s.store.clone();
    Ok(Json(blocking(move || st.move_note(&o, id, b.prev, b.next)).await?))
}

/// One byte range from a Range header: `bytes=a-b`, `bytes=a-` or `bytes=-n`. Several ranges are answered with the
/// first one only, which browsers handle. Returns None when the range cannot be satisfied.
fn parse_range(h: &str, total: usize) -> Option<(usize, usize)> {
    let spec = h.trim().strip_prefix("bytes=")?.split(',').next()?.trim();
    let (a, b) = spec.split_once('-')?;
    if total == 0 {
        return None;
    }
    let (start, end) = if a.is_empty() {
        let n: usize = b.parse().ok()?;
        if n == 0 {
            return None;
        }
        (total.saturating_sub(n), total - 1)
    } else {
        let start: usize = a.parse().ok()?;
        let end = if b.is_empty() { total - 1 } else { b.parse::<usize>().ok()?.min(total - 1) };
        (start, end)
    };
    (start <= end && start < total).then_some((start, end))
}

#[cfg(test)]
mod range_tests {
    use super::parse_range;

    #[test]
    fn ranges() {
        assert_eq!(parse_range("bytes=0-99", 1000), Some((0, 99)));
        assert_eq!(parse_range("bytes=500-", 1000), Some((500, 999)));
        assert_eq!(parse_range("bytes=-100", 1000), Some((900, 999)));
        assert_eq!(parse_range("bytes=900-5000", 1000), Some((900, 999)), "end past the file is clamped");
        assert_eq!(parse_range("bytes=0-0,5-9", 1000), Some((0, 0)), "first of several");
        assert_eq!(parse_range("bytes=1000-", 1000), None);
        assert_eq!(parse_range("bytes=5-1", 1000), None);
        assert_eq!(parse_range("bytes=-0", 1000), None);
        assert_eq!(parse_range("items=0-1", 1000), None);
        assert_eq!(parse_range("bytes=x-", 1000), None);
        assert_eq!(parse_range("bytes=-5000", 1000), Some((0, 999)));
    }
}

// ---------------------------------------------------------------- accounts

async fn me(State(s): State<AppState>, Authed(p): Authed) -> Result<impl IntoResponse, ApiError> {
    let accounts = s.accounts.clone();
    Ok(Json(blocking(move || accounts.get(&p.id)).await?))
}

/// Everything the account owns, as one ZIP: notes (JSON and Markdown), tag rules, suggestion feedback, and the original
/// files. Built in memory, so for now it is limited to 1 GB of files.
async fn export_me(State(s): State<AppState>, Authed(p): Authed) -> Result<impl IntoResponse, ApiError> {
    const MAX_FILES_BYTES: i64 = 1 << 30;
    let o = p.id.clone();
    let (st, blobs) = (s.store.clone(), s.blobs.clone());
    let (zip, date) = tokio::task::spawn_blocking(move || -> Result<(Vec<u8>, String), ApiError> {
        let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs() as i64).unwrap_or(0);
        let mut notes = Vec::new();
        loop {
            let q = NoteQuery { limit: Some(200), offset: Some(notes.len() as u32), ..Default::default() };
            let page = st.list(&o, &q)?;
            let n = page.len();
            notes.extend(page);
            if n < 200 {
                break;
            }
        }
        let total: i64 = notes.iter().flat_map(|n| n.attachments.iter()).map(|a| a.bytes).sum();
        if total > MAX_FILES_BYTES {
            return Err(ApiError(StatusCode::PAYLOAD_TOO_LARGE, "your files are over 1 GB, too large to download in one piece for now; ask the administrator".into()));
        }
        let zerr = |e: String| ApiError(StatusCode::INTERNAL_SERVER_ERROR, e);
        let mut z = chuck_core::zip::ZipWriter::new();
        z.add("README.txt", format!(
            "Your Chuck data, exported {}.\n\nnotes.json      every card with its tags, dates, reminders and attachment details\nnotes.md        the same cards as readable Markdown\nrules.json      your auto-tag rules\nsuggestion-feedback.json  the tag suggestions you accepted or dismissed\nattachments/    your original pictures and audio, named <id>-<file name>\n",
            chuck_core::export::fmt_utc(now)
        ).as_bytes(), now).map_err(zerr)?;
        z.add("notes.json", &pretty(&notes), now).map_err(zerr)?;
        z.add("notes.md", chuck_core::export::to_markdown(&notes, now).as_bytes(), now).map_err(zerr)?;
        z.add("rules.json", &pretty(&st.rules(&o)?), now).map_err(zerr)?;
        z.add("suggestion-feedback.json", &pretty(&st.feedback_events(&o)?), now).map_err(zerr)?;
        for a in notes.iter().flat_map(|n| n.attachments.iter()) {
            let bytes = blobs.get(&a.blob_key).map_err(|e| ApiError(StatusCode::INTERNAL_SERVER_ERROR, format!("reading file {}: {e}", a.id)))?;
            let name = clean_name(a.name.clone()).unwrap_or_else(|| "file".into());
            z.add(&format!("attachments/{}-{}", a.id, name.replace('/', "_")), &bytes, now).map_err(zerr)?;
        }
        Ok((z.finish().map_err(zerr)?, chuck_core::export::fmt_date(now)))
    })
    .await
    .map_err(|e| ApiError(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))??;
    Ok((
        [
            (header::CONTENT_TYPE, "application/zip".to_string()),
            (header::CONTENT_DISPOSITION, format!("attachment; filename=\"chuck-data-{date}.zip\"")),
            (header::CACHE_CONTROL, "no-store".to_string()),
        ],
        zip,
    ))
}

fn pretty<T: serde::Serialize>(v: &T) -> Vec<u8> {
    serde_json::to_vec_pretty(v).unwrap_or_default()
}

#[derive(Deserialize)]
struct ConfirmInput {
    /// Must equal the account id: deleting is permanent.
    confirm: String,
}

/// Deletes the caller's own account and all of its data and files. Signing in again starts a new, empty account.
async fn delete_me(State(s): State<AppState>, Authed(p): Authed, Json(b): Json<ConfirmInput>) -> Result<impl IntoResponse, ApiError> {
    if b.confirm != p.id {
        return Err(ApiError(StatusCode::BAD_REQUEST, "type your account name to confirm".into()));
    }
    erase_account(&s, &p.id).await?;
    Ok(StatusCode::NO_CONTENT)
}

/// Deletes an account and everything it owns, through the control plane (which marks it `deleting`, purges the data
/// plane, and only then removes the account record).
async fn erase_account(s: &AppState, id: &str) -> Result<(), ApiError> {
    let (accounts, id) = (s.accounts.clone(), id.to_string());
    blocking(move || accounts.delete(&id)).await
}

/// Accounts with counts and sizes. Never any content.
async fn admin_users(State(s): State<AppState>, Authed(me): Authed) -> Result<impl IntoResponse, ApiError> {
    let admin = s.admin.clone();
    let users = tokio::task::spawn_blocking(move || admin.accounts(&me)).await.map_err(join_error)??;
    Ok(Json(serde_json::json!({ "users": users })))
}

#[derive(Deserialize)]
struct StatusInput {
    id: String,
    active: bool,
}

async fn admin_user_status(State(s): State<AppState>, Authed(me): Authed, Json(b): Json<StatusInput>) -> Result<impl IntoResponse, ApiError> {
    let admin = s.admin.clone();
    Ok(Json(tokio::task::spawn_blocking(move || admin.set_active(&me, &b.id, b.active)).await.map_err(join_error)??))
}

#[derive(Deserialize)]
struct IdInput {
    id: String,
}

/// Deletes another account and everything it owns. To keep someone out without deleting, disable the account.
async fn admin_delete_user(State(s): State<AppState>, Authed(me): Authed, Json(b): Json<IdInput>) -> Result<impl IntoResponse, ApiError> {
    let admin = s.admin.clone();
    tokio::task::spawn_blocking(move || admin.delete_account(&me, &b.id)).await.map_err(join_error)??;
    Ok(StatusCode::NO_CONTENT)
}

/// Deletes stored files that no card refers to. Reports counts only.
async fn admin_sweep(State(s): State<AppState>, Authed(me): Authed) -> Result<impl IntoResponse, ApiError> {
    let admin = s.admin.clone();
    Ok(Json(tokio::task::spawn_blocking(move || admin.sweep(&me)).await.map_err(join_error)??))
}

fn join_error(e: tokio::task::JoinError) -> ApiError {
    ApiError(StatusCode::INTERNAL_SERVER_ERROR, e.to_string())
}
