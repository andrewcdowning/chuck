use crate::auth::Authenticator;
use crate::blob::{self, BlobStore};
use crate::media::{self, MediaError};
use crate::store::{normalize_tag, NewAttachment, NoteInput, NoteQuery, NoteStore, RuleInput, StoreError, User, REPEATS};
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

#[derive(Clone)]
pub struct AppState {
    pub store: Arc<dyn NoteStore>,
    pub auth: Arc<dyn Authenticator>,
    pub blobs: Arc<dyn BlobStore>,
    /// Owner ids that are administrators (`CHUCK_ADMINS`). Admins manage accounts; they cannot read anyone's content.
    pub admins: Arc<HashSet<String>>,
}

impl AppState {
    /// No administrators.
    pub fn new(store: Arc<dyn NoteStore>, auth: Arc<dyn Authenticator>, blobs: Arc<dyn BlobStore>) -> Self {
        Self { store, auth, blobs, admins: Arc::new(HashSet::new()) }
    }
}

const MAX_BODY: usize = 20_000;

/// Turns away anonymous requests to /api before any body or query is parsed, so a malformed request cannot learn
/// anything (such as a 422 that shows which route exists) without signing in. Handlers still do the full check.
async fn require_identity(
    axum::extract::State(s): axum::extract::State<AppState>,
    req: axum::extract::Request,
    next: axum::middleware::Next,
) -> axum::response::Response {
    use axum::response::IntoResponse;
    if req.uri().path().starts_with("/api/") && s.auth.identity(req.headers()).is_none() {
        return ApiError(StatusCode::UNAUTHORIZED, "unauthorized".into()).into_response();
    }
    next.run(req).await
}

pub fn router(state: AppState) -> Router {
    Router::new()
        .route("/", get(|| async { Html(include_str!("../static/index.html")) }))
        .route("/healthz", get(|| async { "ok" }))
        .route("/api/notes", get(list).post(create))
        .route("/api/notes/:id", put(update).delete(remove))
        .route("/api/notes/:id/move", post(move_note))
        .route("/api/me", get(me).delete(delete_me))
        .route("/api/me/export", get(export_me))
        .route("/api/admin/users", get(admin_users))
        .route("/api/admin/users/status", post(admin_user_status))
        .route("/api/admin/users/delete", post(admin_delete_user))
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
        .layer(axum::middleware::from_fn_with_state(state.clone(), require_identity))
        .with_state(state)
}

struct ApiError(StatusCode, String);

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        (self.0, Json(serde_json::json!({ "error": self.1 }))).into_response()
    }
}

impl From<StoreError> for ApiError {
    fn from(e: StoreError) -> Self {
        match e {
            StoreError::NotFound => ApiError(StatusCode::NOT_FOUND, "not found".into()),
            StoreError::Invalid(m) => ApiError(StatusCode::BAD_REQUEST, m),
            StoreError::Other(m) => {
                eprintln!("store error: {m}");
                ApiError(StatusCode::INTERNAL_SERVER_ERROR, "internal error".into())
            }
        }
    }
}

/// Who is calling, as an account. Authentication may fetch signing keys over the network, and the account is looked
/// up (and created on first sight), so all of it runs on a blocking thread. A disabled account is refused here, which
/// is what makes "disabled" apply to every endpoint.
async fn session(s: &AppState, h: &HeaderMap) -> Result<User, ApiError> {
    let (auth, store, admins, headers) = (s.auth.clone(), s.store.clone(), s.admins.clone(), h.clone());
    let found = tokio::task::spawn_blocking(move || -> Result<Option<User>, StoreError> {
        let Some(who) = auth.identity(&headers) else { return Ok(None) };
        let admin = admins.contains(&who.id);
        store.touch_user(&who.id, who.name.as_deref(), admin).map(Some)
    })
    .await
    .map_err(|e| ApiError(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))??;
    let user = found.ok_or(ApiError(StatusCode::UNAUTHORIZED, "unauthorized".into()))?;
    if user.status != "active" {
        return Err(ApiError(StatusCode::UNAUTHORIZED, "this account is disabled".into()));
    }
    Ok(user)
}

async fn owner(s: &AppState, h: &HeaderMap) -> Result<String, ApiError> {
    Ok(session(s, h).await?.id)
}

async fn admin(s: &AppState, h: &HeaderMap) -> Result<User, ApiError> {
    let u = session(s, h).await?;
    if u.role != "admin" {
        return Err(ApiError(StatusCode::FORBIDDEN, "administrators only".into()));
    }
    Ok(u)
}

async fn blocking<T: Send + 'static>(f: impl FnOnce() -> crate::store::Result<T> + Send + 'static) -> Result<T, ApiError> {
    tokio::task::spawn_blocking(f)
        .await
        .map_err(|e| ApiError(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?
        .map_err(Into::into)
}

/// `allow_empty`: a card may have no text when it holds media.
fn validate(mut i: NoteInput, allow_empty: bool) -> Result<NoteInput, ApiError> {
    i.body = i.body.trim().to_string();
    if i.body.is_empty() && !allow_empty {
        return Err(ApiError(StatusCode::BAD_REQUEST, "body is empty".into()));
    }
    if i.body.len() > MAX_BODY {
        return Err(ApiError(StatusCode::PAYLOAD_TOO_LARGE, "note too long".into()));
    }
    if let Some(r) = &i.repeat {
        if !r.is_empty() && !REPEATS.contains(&r.as_str()) {
            return Err(ApiError(StatusCode::BAD_REQUEST, "invalid repeat".into()));
        }
    }
    Ok(i)
}

#[derive(Deserialize)]
struct CreateQuery {
    /// Set when attachments will be uploaded next, so the card may start without text.
    media: Option<bool>,
}

async fn create(State(s): State<AppState>, h: HeaderMap, Query(q): Query<CreateQuery>, Json(b): Json<NoteInput>) -> Result<impl IntoResponse, ApiError> {
    let o = owner(&s, &h).await?;
    let input = validate(b, q.media.unwrap_or(false))?;
    let st = s.store.clone();
    let n = blocking(move || st.create(&o, &input)).await?;
    Ok((StatusCode::CREATED, Json(n)))
}

async fn update(State(s): State<AppState>, h: HeaderMap, Path(id): Path<i64>, Json(b): Json<NoteInput>) -> Result<impl IntoResponse, ApiError> {
    let o = owner(&s, &h).await?;
    let st = s.store.clone();
    let has_media = {
        let (st, o) = (st.clone(), o.clone());
        !blocking(move || st.get(&o, id)).await?.attachments.is_empty()
    };
    let input = validate(b, has_media)?;
    Ok(Json(blocking(move || st.update(&o, id, &input)).await?))
}

async fn remove(State(s): State<AppState>, h: HeaderMap, Path(id): Path<i64>) -> Result<impl IntoResponse, ApiError> {
    let o = owner(&s, &h).await?;
    let (st, blobs) = (s.store.clone(), s.blobs.clone());
    blocking(move || {
        let note = st.get(&o, id)?;
        st.delete(&o, id)?;
        let keys = note.attachments.into_iter().flat_map(|a| std::iter::once(a.blob_key).chain(a.thumb_key)).collect();
        collect_garbage(&*st, &*blobs, &o, keys);
        Ok(())
    })
    .await?;
    Ok(StatusCode::NO_CONTENT)
}

async fn list(State(s): State<AppState>, h: HeaderMap, Query(q): Query<NoteQuery>) -> Result<impl IntoResponse, ApiError> {
    let o = owner(&s, &h).await?;
    let st = s.store.clone();
    Ok(Json(serde_json::json!({ "notes": blocking(move || st.list(&o, &q)).await? })))
}

async fn tags(State(s): State<AppState>, h: HeaderMap) -> Result<impl IntoResponse, ApiError> {
    let o = owner(&s, &h).await?;
    let st = s.store.clone();
    Ok(Json(serde_json::json!({ "tags": blocking(move || st.tags(&o)).await? })))
}

#[derive(Deserialize)]
struct SuggestInput {
    text: String,
}

async fn suggest(State(s): State<AppState>, h: HeaderMap, Json(b): Json<SuggestInput>) -> Result<impl IntoResponse, ApiError> {
    let o = owner(&s, &h).await?;
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

async fn suggestion_feedback(State(s): State<AppState>, h: HeaderMap, Json(b): Json<FeedbackInput>) -> Result<impl IntoResponse, ApiError> {
    let o = owner(&s, &h).await?;
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

async fn rules(State(s): State<AppState>, h: HeaderMap) -> Result<impl IntoResponse, ApiError> {
    let o = owner(&s, &h).await?;
    let st = s.store.clone();
    Ok(Json(serde_json::json!({ "rules": blocking(move || st.rules(&o)).await? })))
}

async fn add_rule(State(s): State<AppState>, h: HeaderMap, Json(b): Json<RuleInput>) -> Result<impl IntoResponse, ApiError> {
    let o = owner(&s, &h).await?;
    let pattern = b.pattern.trim().to_string();
    if pattern.is_empty() || pattern.chars().count() > 200 || !pattern.split(|c| c == '|' || c == ',').any(|k| !k.trim().is_empty()) {
        return Err(ApiError(StatusCode::BAD_REQUEST, "keywords are empty or too long".into()));
    }
    let tag = normalize_tag(&b.tag).ok_or(ApiError(StatusCode::BAD_REQUEST, "invalid tag name".into()))?;
    let st = s.store.clone();
    let r = blocking(move || st.add_rule(&o, &pattern, &tag)).await?;
    Ok((StatusCode::CREATED, Json(r)))
}

async fn delete_rule(State(s): State<AppState>, h: HeaderMap, Path(id): Path<i64>) -> Result<impl IntoResponse, ApiError> {
    let o = owner(&s, &h).await?;
    let st = s.store.clone();
    blocking(move || st.delete_rule(&o, id)).await?;
    Ok(StatusCode::NO_CONTENT)
}

async fn apply_rules(State(s): State<AppState>, h: HeaderMap) -> Result<impl IntoResponse, ApiError> {
    let o = owner(&s, &h).await?;
    let st = s.store.clone();
    let changed = blocking(move || st.apply_rules(&o)).await?;
    Ok(Json(serde_json::json!({ "changed": changed })))
}

/// Markdown download of every note (or the notes matching the same filters as `/api/notes`; paging is ignored).
async fn export(State(s): State<AppState>, h: HeaderMap, Query(mut q): Query<NoteQuery>) -> Result<impl IntoResponse, ApiError> {
    let o = owner(&s, &h).await?;
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
    let body = crate::export::to_markdown(&notes, now);
    let disposition = format!("attachment; filename=\"chuck-export-{}.md\"", crate::export::fmt_date(now));
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
fn collect_garbage(st: &dyn NoteStore, blobs: &dyn BlobStore, owner: &str, keys: Vec<String>) {
    for k in keys {
        match st.blob_referenced(owner, &k) {
            Ok(false) => {
                if let Err(e) = blobs.delete(&k) {
                    eprintln!("could not delete blob {k}: {e}");
                }
            }
            Ok(true) => {}
            Err(_) => eprintln!("could not check blob {k}"),
        }
    }
}

/// File names are display-only: keep the last path component, drop control characters, cap the length.
fn clean_name(n: Option<String>) -> Option<String> {
    let n = n?;
    let base = n.rsplit(['/', '\\']).next().unwrap_or("");
    let s: String = base.chars().filter(|c| !c.is_control()).take(200).collect();
    let s = s.trim().to_string();
    (!s.is_empty()).then_some(s)
}

#[derive(Deserialize)]
struct AttachQuery {
    name: Option<String>,
    /// Audio length measured by the browser. Display only; must be between 0 and 6 hours.
    duration_ms: Option<i64>,
}

/// Body: the raw file. The type is detected from the bytes; metadata is stripped before storing.
async fn add_attachment(State(s): State<AppState>, h: HeaderMap, Path(note_id): Path<i64>, Query(q): Query<AttachQuery>, body: Bytes) -> Result<impl IntoResponse, ApiError> {
    let o = owner(&s, &h).await?;
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
            Err(e) => {
                collect_garbage(&*st, &*blobs, &o, vec![key]);
                Err(e.into())
            }
        }
    })
    .await
    .map_err(|e| ApiError(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))??;
    Ok((StatusCode::CREATED, Json(res)))
}

/// Body: a small preview image for the attachment, made by the client.
async fn put_thumb(State(s): State<AppState>, h: HeaderMap, Path(id): Path<i64>, body: Bytes) -> Result<impl IntoResponse, ApiError> {
    let o = owner(&s, &h).await?;
    let (st, blobs) = (s.store.clone(), s.blobs.clone());
    tokio::task::spawn_blocking(move || -> Result<_, ApiError> {
        st.attachment(&o, id)?;
        let c = media::check_image(&body, media::MAX_THUMB_BYTES)?;
        if c.width > 1024 || c.height > 1024 {
            return Err(ApiError(StatusCode::BAD_REQUEST, "preview must be at most 1024 pixels on each side".into()));
        }
        let key = blob::key_for(&o, &c.bytes);
        blobs.put(&key, &c.bytes).map_err(|e| ApiError(StatusCode::INTERNAL_SERVER_ERROR, format!("storing file: {e}")))?;
        let old = st.set_attachment_thumb(&o, id, &key)?;
        collect_garbage(&*st, &*blobs, &o, old.into_iter().collect());
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

async fn get_media(State(s): State<AppState>, h: HeaderMap, Path(id): Path<i64>, Query(q): Query<MediaQuery>) -> Result<Response, ApiError> {
    let o = owner(&s, &h).await?;
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

async fn delete_media(State(s): State<AppState>, h: HeaderMap, Path(id): Path<i64>) -> Result<impl IntoResponse, ApiError> {
    let o = owner(&s, &h).await?;
    let (st, blobs) = (s.store.clone(), s.blobs.clone());
    blocking(move || {
        let a = st.delete_attachment(&o, id)?;
        collect_garbage(&*st, &*blobs, &o, std::iter::once(a.blob_key).chain(a.thumb_key).collect());
        Ok(())
    })
    .await?;
    Ok(StatusCode::NO_CONTENT)
}

#[derive(Deserialize)]
struct MoveInput {
    /// The card that will be directly above (before) this one, or null for the top of its group.
    prev: Option<i64>,
    /// The card that will be directly below (after) this one, or null for the bottom.
    next: Option<i64>,
}

async fn move_note(State(s): State<AppState>, h: HeaderMap, Path(id): Path<i64>, Json(b): Json<MoveInput>) -> Result<impl IntoResponse, ApiError> {
    let o = owner(&s, &h).await?;
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

async fn me(State(s): State<AppState>, h: HeaderMap) -> Result<impl IntoResponse, ApiError> {
    Ok(Json(session(&s, &h).await?))
}

/// Everything the account owns, as one ZIP: notes (JSON and Markdown), tag rules, suggestion feedback, and the original
/// files. Built in memory, so for now it is limited to 1 GB of files.
async fn export_me(State(s): State<AppState>, h: HeaderMap) -> Result<impl IntoResponse, ApiError> {
    const MAX_FILES_BYTES: i64 = 1 << 30;
    let o = owner(&s, &h).await?;
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
        let mut z = crate::zip::ZipWriter::new();
        z.add("README.txt", format!(
            "Your Chuck data, exported {}.\n\nnotes.json      every card with its tags, dates, reminders and attachment details\nnotes.md        the same cards as readable Markdown\nrules.json      your auto-tag rules\nsuggestion-feedback.json  the tag suggestions you accepted or dismissed\nattachments/    your original pictures and audio, named <id>-<file name>\n",
            crate::export::fmt_utc(now)
        ).as_bytes(), now).map_err(zerr)?;
        z.add("notes.json", &pretty(&notes), now).map_err(zerr)?;
        z.add("notes.md", crate::export::to_markdown(&notes, now).as_bytes(), now).map_err(zerr)?;
        z.add("rules.json", &pretty(&st.rules(&o)?), now).map_err(zerr)?;
        z.add("suggestion-feedback.json", &pretty(&st.feedback_events(&o)?), now).map_err(zerr)?;
        for a in notes.iter().flat_map(|n| n.attachments.iter()) {
            let bytes = blobs.get(&a.blob_key).map_err(|e| ApiError(StatusCode::INTERNAL_SERVER_ERROR, format!("reading file {}: {e}", a.id)))?;
            let name = clean_name(a.name.clone()).unwrap_or_else(|| "file".into());
            z.add(&format!("attachments/{}-{}", a.id, name.replace('/', "_")), &bytes, now).map_err(zerr)?;
        }
        Ok((z.finish().map_err(zerr)?, crate::export::fmt_date(now)))
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
async fn delete_me(State(s): State<AppState>, h: HeaderMap, Json(b): Json<ConfirmInput>) -> Result<impl IntoResponse, ApiError> {
    let me = session(&s, &h).await?;
    if b.confirm != me.id {
        return Err(ApiError(StatusCode::BAD_REQUEST, "type your account name to confirm".into()));
    }
    erase_account(&s, &me.id).await?;
    Ok(StatusCode::NO_CONTENT)
}

async fn erase_account(s: &AppState, id: &str) -> Result<(), ApiError> {
    let (st, blobs, id) = (s.store.clone(), s.blobs.clone(), id.to_string());
    blocking(move || {
        let keys = st.delete_user(&id)?;
        for k in keys {
            if let Err(e) = blobs.delete(&k) {
                eprintln!("could not delete blob {k}: {e}");
            }
        }
        Ok(())
    })
    .await
}

/// Accounts with counts and sizes. Never any content.
async fn admin_users(State(s): State<AppState>, h: HeaderMap) -> Result<impl IntoResponse, ApiError> {
    admin(&s, &h).await?;
    let st = s.store.clone();
    Ok(Json(serde_json::json!({ "users": blocking(move || st.list_users()).await? })))
}

#[derive(Deserialize)]
struct StatusInput {
    id: String,
    active: bool,
}

async fn admin_user_status(State(s): State<AppState>, h: HeaderMap, Json(b): Json<StatusInput>) -> Result<impl IntoResponse, ApiError> {
    let me = admin(&s, &h).await?;
    if b.id == me.id {
        return Err(ApiError(StatusCode::BAD_REQUEST, "you cannot disable your own account".into()));
    }
    let st = s.store.clone();
    Ok(Json(blocking(move || st.set_user_active(&b.id, b.active)).await?))
}

#[derive(Deserialize)]
struct IdInput {
    id: String,
}

/// Deletes another account and everything it owns. To keep someone out without deleting, disable the account.
async fn admin_delete_user(State(s): State<AppState>, h: HeaderMap, Json(b): Json<IdInput>) -> Result<impl IntoResponse, ApiError> {
    let me = admin(&s, &h).await?;
    if b.id == me.id {
        return Err(ApiError(StatusCode::BAD_REQUEST, "to delete your own account use Delete my account".into()));
    }
    erase_account(&s, &b.id).await?;
    Ok(StatusCode::NO_CONTENT)
}
