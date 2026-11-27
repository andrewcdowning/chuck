use crate::auth::Authenticator;
use crate::blob::{self, BlobStore};
use crate::media::{self, MediaError};
use crate::store::{normalize_tag, NewAttachment, NoteInput, NoteQuery, NoteStore, RuleInput, StoreError, REPEATS};
use serde::Deserialize;
use axum::{
    body::Bytes,
    extract::{DefaultBodyLimit, Path, Query, State},
    http::{header, HeaderMap, HeaderValue, StatusCode},
    response::{Html, IntoResponse, Response},
    routing::{delete, get, post, put},
    Json, Router,
};
use std::sync::Arc;

#[derive(Clone)]
pub struct AppState {
    pub store: Arc<dyn NoteStore>,
    pub auth: Arc<dyn Authenticator>,
    pub blobs: Arc<dyn BlobStore>,
}

const MAX_BODY: usize = 20_000;

pub fn router(state: AppState) -> Router {
    Router::new()
        .route("/", get(|| async { Html(include_str!("../static/index.html")) }))
        .route("/healthz", get(|| async { "ok" }))
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
            post(add_attachment).layer(DefaultBodyLimit::max(media::MAX_IMAGE_BYTES + 1024)),
        )
        .route("/api/media/:id", get(get_media).delete(delete_media))
        .route("/api/media/:id/thumb", put(put_thumb).layer(DefaultBodyLimit::max(media::MAX_THUMB_BYTES + 1024)))
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

/// Authentication may fetch signing keys over the network, so it runs on a blocking thread.
async fn owner(s: &AppState, h: &HeaderMap) -> Result<String, ApiError> {
    let (auth, headers) = (s.auth.clone(), h.clone());
    tokio::task::spawn_blocking(move || auth.authenticate(&headers))
        .await
        .ok()
        .flatten()
        .ok_or(ApiError(StatusCode::UNAUTHORIZED, "unauthorized".into()))
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
    Ok(Json(blocking(move || st.list(&o, &q)).await?))
}

async fn tags(State(s): State<AppState>, h: HeaderMap) -> Result<impl IntoResponse, ApiError> {
    let o = owner(&s, &h).await?;
    let st = s.store.clone();
    Ok(Json(blocking(move || st.tags(&o)).await?))
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
    Ok(Json(blocking(move || st.rules(&o)).await?))
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
}

/// Body: the raw file. The type is detected from the bytes; metadata is stripped before storing.
async fn add_attachment(State(s): State<AppState>, h: HeaderMap, Path(note_id): Path<i64>, Query(q): Query<AttachQuery>, body: Bytes) -> Result<impl IntoResponse, ApiError> {
    let o = owner(&s, &h).await?;
    let (st, blobs) = (s.store.clone(), s.blobs.clone());
    let name = clean_name(q.name);
    let res = tokio::task::spawn_blocking(move || -> Result<_, ApiError> {
        // Fail early, before any work, if the card is not this user's.
        st.get(&o, note_id)?;
        let c = media::check_image(&body, media::MAX_IMAGE_BYTES)?;
        let key = blob::key_for(&o, &c.bytes);
        blobs.put(&key, &c.bytes).map_err(|e| ApiError(StatusCode::INTERNAL_SERVER_ERROR, format!("storing file: {e}")))?;
        let new = NewAttachment {
            mime: c.mime.to_string(),
            bytes: c.bytes.len() as i64,
            width: Some(c.width as i64),
            height: Some(c.height as i64),
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
}

async fn get_media(State(s): State<AppState>, h: HeaderMap, Path(id): Path<i64>, Query(q): Query<MediaQuery>) -> Result<Response, ApiError> {
    let o = owner(&s, &h).await?;
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
    // The preview may be a different format from the original; detect it from its bytes.
    let mime = if is_thumb { media::sniff(&bytes).unwrap_or("application/octet-stream").to_string() } else { a.mime.clone() };
    let mut resp = bytes.into_response();
    let hd = resp.headers_mut();
    hd.insert(header::CONTENT_TYPE, HeaderValue::from_str(&mime).unwrap_or(HeaderValue::from_static("application/octet-stream")));
    hd.insert(header::X_CONTENT_TYPE_OPTIONS, HeaderValue::from_static("nosniff"));
    // Even if a file were opened directly in a tab, it could not run script or load anything.
    hd.insert(header::CONTENT_SECURITY_POLICY, HeaderValue::from_static("default-src 'none'; style-src 'unsafe-inline'; sandbox"));
    hd.insert(header::CACHE_CONTROL, HeaderValue::from_static("private, max-age=86400"));
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
