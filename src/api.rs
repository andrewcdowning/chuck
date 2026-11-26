use crate::auth::Authenticator;
use crate::store::{normalize_tag, NoteInput, NoteQuery, NoteStore, RuleInput, StoreError, REPEATS};
use serde::Deserialize;
use axum::{
    extract::{Path, Query, State},
    http::{HeaderMap, StatusCode},
    response::{Html, IntoResponse, Response},
    routing::{delete, get, post, put},
    Json, Router,
};
use std::sync::Arc;

#[derive(Clone)]
pub struct AppState {
    pub store: Arc<dyn NoteStore>,
    pub auth: Arc<dyn Authenticator>,
}

const MAX_BODY: usize = 20_000;

pub fn router(state: AppState) -> Router {
    Router::new()
        .route("/", get(|| async { Html(include_str!("../static/index.html")) }))
        .route("/healthz", get(|| async { "ok" }))
        .route("/api/notes", get(list).post(create))
        .route("/api/notes/:id", put(update).delete(remove))
        .route("/api/tags", get(tags))
        .route("/api/export", get(export))
        .route("/api/cards/todos", get(todos))
        .route("/api/cards/todos/items", post(todo_add))
        .route("/api/cards/todos/items/:id", put(todo_update).delete(todo_delete))
        .route("/api/cards/todos/done", delete(todo_clear_done))
        .route("/api/suggest", post(suggest))
        .route("/api/rules", get(rules).post(add_rule))
        .route("/api/rules/apply", post(apply_rules))
        .route("/api/rules/:id", delete(delete_rule))
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

fn validate(mut i: NoteInput) -> Result<NoteInput, ApiError> {
    i.body = i.body.trim().to_string();
    if i.body.is_empty() {
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

async fn create(State(s): State<AppState>, h: HeaderMap, Json(b): Json<NoteInput>) -> Result<impl IntoResponse, ApiError> {
    let o = owner(&s, &h).await?;
    let input = validate(b)?;
    let st = s.store.clone();
    let n = blocking(move || st.create(&o, &input)).await?;
    Ok((StatusCode::CREATED, Json(n)))
}

async fn update(State(s): State<AppState>, h: HeaderMap, Path(id): Path<i64>, Json(b): Json<NoteInput>) -> Result<impl IntoResponse, ApiError> {
    let o = owner(&s, &h).await?;
    let input = validate(b)?;
    let st = s.store.clone();
    Ok(Json(blocking(move || st.update(&o, id, &input)).await?))
}

async fn remove(State(s): State<AppState>, h: HeaderMap, Path(id): Path<i64>) -> Result<impl IntoResponse, ApiError> {
    let o = owner(&s, &h).await?;
    let st = s.store.clone();
    blocking(move || st.delete(&o, id)).await?;
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

/// The standalone Todos card, in the same card vocabulary as notes (`kind`).
async fn todos(State(s): State<AppState>, h: HeaderMap) -> Result<impl IntoResponse, ApiError> {
    let o = owner(&s, &h).await?;
    let st = s.store.clone();
    let items = blocking(move || st.todo_list(&o)).await?;
    let done = items.iter().filter(|t| t.done).count();
    Ok(Json(serde_json::json!({ "kind": "todos", "title": "Todos", "open": items.len() - done, "done": done, "items": items })))
}

#[derive(Deserialize)]
struct TodoInput {
    text: Option<String>,
    done: Option<bool>,
}

fn todo_text(t: &str) -> Result<String, ApiError> {
    let t = t.trim();
    if t.is_empty() || t.chars().count() > 500 {
        return Err(ApiError(StatusCode::BAD_REQUEST, "todo text must be 1 to 500 characters".into()));
    }
    Ok(t.to_string())
}

async fn todo_add(State(s): State<AppState>, h: HeaderMap, Json(b): Json<TodoInput>) -> Result<impl IntoResponse, ApiError> {
    let o = owner(&s, &h).await?;
    let text = todo_text(b.text.as_deref().unwrap_or(""))?;
    let st = s.store.clone();
    Ok((StatusCode::CREATED, Json(blocking(move || st.todo_add(&o, &text)).await?)))
}

async fn todo_update(State(s): State<AppState>, h: HeaderMap, Path(id): Path<i64>, Json(b): Json<TodoInput>) -> Result<impl IntoResponse, ApiError> {
    let o = owner(&s, &h).await?;
    let text = b.text.as_deref().map(todo_text).transpose()?;
    let st = s.store.clone();
    Ok(Json(blocking(move || st.todo_update(&o, id, text.as_deref(), b.done)).await?))
}

async fn todo_delete(State(s): State<AppState>, h: HeaderMap, Path(id): Path<i64>) -> Result<impl IntoResponse, ApiError> {
    let o = owner(&s, &h).await?;
    let st = s.store.clone();
    blocking(move || st.todo_delete(&o, id)).await?;
    Ok(StatusCode::NO_CONTENT)
}

async fn todo_clear_done(State(s): State<AppState>, h: HeaderMap) -> Result<impl IntoResponse, ApiError> {
    let o = owner(&s, &h).await?;
    let st = s.store.clone();
    let removed = blocking(move || st.todo_clear_done(&o)).await?;
    Ok(Json(serde_json::json!({ "removed": removed })))
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
    // The standalone todo list is part of a full export only; a filtered export is just matching notes.
    let unfiltered = q.q.is_none() && q.tag.is_none() && q.from.is_none() && q.to.is_none() && q.reminders != Some(true);
    let st = s.store.clone();
    let (notes, todos) = blocking(move || {
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
                let todos = if unfiltered { st.todo_list(&o)? } else { Vec::new() };
                return Ok((all, todos));
            }
            offset += PAGE;
        }
    })
    .await?;
    let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs() as i64).unwrap_or(0);
    let body = crate::export::to_markdown(&notes, &todos, now);
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
