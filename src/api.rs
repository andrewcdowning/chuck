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
        .route("/api/notes/:id/toggle", post(toggle))
        .route("/api/cards/todos", get(todos))
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

#[derive(Deserialize)]
struct ToggleInput {
    line: usize,
}

async fn toggle(State(s): State<AppState>, h: HeaderMap, Path(id): Path<i64>, Json(b): Json<ToggleInput>) -> Result<impl IntoResponse, ApiError> {
    let o = owner(&s, &h).await?;
    let st = s.store.clone();
    Ok(Json(blocking(move || st.toggle_item(&o, id, b.line)).await?))
}

/// A virtual card: every open checklist item across all notes, in the same card vocabulary as notes.
async fn todos(State(s): State<AppState>, h: HeaderMap) -> Result<impl IntoResponse, ApiError> {
    let o = owner(&s, &h).await?;
    let st = s.store.clone();
    let items = blocking(move || st.todos(&o)).await?;
    Ok(Json(serde_json::json!({ "kind": "todos", "title": "Todos", "count": items.len(), "items": items })))
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
