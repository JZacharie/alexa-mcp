use crate::alexa::{AlexaClient, ItemStatus};
use crate::mcp::types::JsonRpcRequest;
use crate::mcp::McpHandler;
use axum::{
    extract::{Query, State},
    http::StatusCode,
    response::{
        sse::{Event, KeepAlive, Sse},
        IntoResponse, Json,
    },
    routing::{get, post},
    Router,
};
use futures::stream::Stream;
use serde::Deserialize;
use serde_json::{json, Value};
use std::{convert::Infallible, sync::Arc, time::Duration};
use tokio_stream::wrappers::IntervalStream;
use tokio_stream::StreamExt as _;
use tower_http::cors::CorsLayer;
use tower_http::trace::TraceLayer;
use tracing::info;

/// Shared state injected into every HTTP handler.
#[derive(Clone)]
pub struct AppState {
    pub mcp_handler: Arc<McpHandler>,
    pub alexa_client: Arc<AlexaClient>,
}

#[derive(Deserialize)]
pub struct ItemsQuery {
    pub status: Option<String>,
}

#[derive(Deserialize)]
pub struct CookiesQuery {
    #[serde(default = "default_true")]
    pub save: bool,
}

fn default_true() -> bool {
    true
}

#[derive(Deserialize)]
pub struct AddItemsRequest {
    pub item: Option<String>,
    pub items: Option<Vec<String>>,
}

#[derive(Deserialize)]
pub struct CompleteItemRequest {
    pub item: String,
    #[serde(default = "default_true")]
    pub completed: bool,
}

#[derive(Deserialize)]
pub struct DeleteItemRequest {
    pub item: String,
}

/// Builds the HTTP router: MCP transport plus a small REST surface.
pub fn create_router(state: AppState) -> Router {
    Router::new()
        .route("/health", get(health_check))
        .route("/ready", get(health_check))
        .route("/sse", get(sse_handler).post(mcp_message_handler))
        .route("/message", post(mcp_message_handler))
        .route("/api/alexa/items", get(alexa_items_handler))
        .route("/api/alexa/items/add", post(alexa_add_handler))
        .route("/api/alexa/items/complete", post(alexa_complete_handler))
        .route("/api/alexa/items/delete", post(alexa_delete_handler))
        .route("/api/alexa/cookies", get(alexa_cookies_handler))
        .layer(CorsLayer::permissive())
        .layer(TraceLayer::new_for_http())
        .with_state(state)
}

async fn health_check() -> impl IntoResponse {
    Json(json!({
        "status": "healthy",
        "service": "alexa-mcp",
        "version": env!("CARGO_PKG_VERSION")
    }))
}

async fn sse_handler() -> Sse<impl Stream<Item = Result<Event, Infallible>>> {
    info!("New MCP client connected to /sse");
    let initial_event = Event::default().event("endpoint").data("/message");
    let interval = tokio::time::interval(Duration::from_secs(15));
    let keep_alive = IntervalStream::new(interval).map(|_| Ok(Event::default().comment("ping")));
    let stream = futures::stream::once(async move { Ok(initial_event) }).chain(keep_alive);
    Sse::new(stream).keep_alive(KeepAlive::default())
}

async fn mcp_message_handler(
    State(state): State<AppState>,
    Json(payload): Json<JsonRpcRequest>,
) -> impl IntoResponse {
    let response = state.mcp_handler.handle_request(payload).await;
    Json(response)
}

async fn alexa_items_handler(
    State(state): State<AppState>,
    Query(params): Query<ItemsQuery>,
) -> (StatusCode, Json<Value>) {
    let status = params
        .status
        .as_deref()
        .map(ItemStatus::parse)
        .unwrap_or(ItemStatus::All);
    match state.alexa_client.get_shopping_list().await {
        Ok(list) => (
            StatusCode::OK,
            Json(json!({
                "success": true,
                "list_id": list.list_id,
                "status": status,
                "total_count": list.total_count,
                "active_count": list.active_count,
                "completed_count": list.completed_count,
                "items": list.filtered(status)
            })),
        ),
        Err(error) => internal_error(error),
    }
}

async fn alexa_add_handler(
    State(state): State<AppState>,
    Json(payload): Json<AddItemsRequest>,
) -> (StatusCode, Json<Value>) {
    let mut items = payload.items.unwrap_or_default();
    if let Some(item) = payload.item {
        items.push(item);
    }
    let items: Vec<String> = items
        .into_iter()
        .filter(|item| !item.trim().is_empty())
        .collect();
    if items.is_empty() {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({ "success": false, "error": "Parameter 'item' or 'items' is required" })),
        );
    }

    let mut details = Vec::new();
    for item in &items {
        match state.alexa_client.add_item(item).await {
            Ok(()) => details.push(json!({ "item": item, "success": true })),
            Err(error) => details
                .push(json!({ "item": item, "success": false, "error": format!("{error:?}") })),
        }
    }
    let failed = details
        .iter()
        .filter(|detail| detail.get("success") == Some(&json!(false)))
        .count();
    (
        if failed == 0 {
            StatusCode::OK
        } else {
            StatusCode::BAD_GATEWAY
        },
        Json(
            json!({ "success": failed == 0, "added": items.len() - failed, "failed": failed, "details": details }),
        ),
    )
}

async fn alexa_complete_handler(
    State(state): State<AppState>,
    Json(payload): Json<CompleteItemRequest>,
) -> (StatusCode, Json<Value>) {
    match state
        .alexa_client
        .complete_item(&payload.item, payload.completed)
        .await
    {
        Ok(item) => (
            StatusCode::OK,
            Json(json!({ "success": true, "item": item })),
        ),
        Err(error) => internal_error(error),
    }
}

async fn alexa_delete_handler(
    State(state): State<AppState>,
    Json(payload): Json<DeleteItemRequest>,
) -> (StatusCode, Json<Value>) {
    match state.alexa_client.delete_item(&payload.item).await {
        Ok(item) => (
            StatusCode::OK,
            Json(json!({ "success": true, "item": item })),
        ),
        Err(error) => internal_error(error),
    }
}

async fn alexa_cookies_handler(
    State(state): State<AppState>,
    Query(params): Query<CookiesQuery>,
) -> (StatusCode, Json<Value>) {
    match state.alexa_client.export_cookies(params.save).await {
        Ok(export) => (
            StatusCode::OK,
            Json(json!({ "success": true, "export": export })),
        ),
        Err(error) => internal_error(error),
    }
}

fn internal_error(error: anyhow::Error) -> (StatusCode, Json<Value>) {
    (
        StatusCode::INTERNAL_SERVER_ERROR,
        Json(json!({ "success": false, "error": format!("{error:?}") })),
    )
}
