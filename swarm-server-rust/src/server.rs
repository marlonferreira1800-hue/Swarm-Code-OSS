use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::extract::{Query, Request, State};
use axum::http::{header, HeaderValue, Method, StatusCode};
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::Deserialize;
use serde_json::json;
use tokio::sync::broadcast::error::RecvError;

use crate::engine::{SwarmEngine, MONITOR_INTERVAL_MS, MONITOR_STALE_MS};
use crate::fs_safe::{absolutize_path, safe_resolve, walk_project_files};
use crate::llm::create_llm_provider;
use crate::types::{ClientEvent, ServerEvent};

#[derive(Clone)]
pub struct AppState {
    pub engine: Arc<SwarmEngine>,
    pub project_root: PathBuf,
}

#[derive(Deserialize)]
struct FileQuery {
    path: Option<String>,
}

#[derive(Deserialize)]
struct TaskBody {
    prompt: Option<String>,
}

/// Resolve o `PROJECT_ROOT` padrão (`PROJECT_ROOT` env var ou `server/demo-project`).
pub fn resolve_project_root() -> PathBuf {
    if let Ok(env_root) = std::env::var("PROJECT_ROOT") {
        if !env_root.trim().is_empty() {
            return absolutize_path(Path::new(&env_root))
                .unwrap_or_else(|_| PathBuf::from(env_root));
        }
    }

    for candidate in ["server/demo-project", "../server/demo-project"] {
        let p = Path::new(candidate);
        if p.exists() {
            if let Ok(abs) = absolutize_path(p) {
                return abs;
            }
        }
    }

    let manifest_default = Path::new(env!("CARGO_MANIFEST_DIR")).join("../server/demo-project");
    absolutize_path(&manifest_default).unwrap_or(manifest_default)
}

async fn cors_middleware(req: Request, next: Next) -> Response {
    if req.method() == Method::OPTIONS {
        let mut resp = StatusCode::NO_CONTENT.into_response();
        apply_cors_headers(resp.headers_mut());
        return resp;
    }

    let mut resp = next.run(req).await;
    apply_cors_headers(resp.headers_mut());
    resp
}

fn apply_cors_headers(headers: &mut axum::http::HeaderMap) {
    headers.insert(
        header::ACCESS_CONTROL_ALLOW_ORIGIN,
        HeaderValue::from_static("*"),
    );
    headers.insert(
        header::ACCESS_CONTROL_ALLOW_METHODS,
        HeaderValue::from_static("GET, POST, OPTIONS"),
    );
    headers.insert(
        header::ACCESS_CONTROL_ALLOW_HEADERS,
        HeaderValue::from_static("Content-Type, Authorization"),
    );
}

/// Constrói o `Router` Axum com `/api/*`, `/ws` e CORS permissivo para Vite e Tauri WebView.
pub fn build_router(engine: Arc<SwarmEngine>, project_root: PathBuf) -> Router {
    let state = AppState {
        engine,
        project_root,
    };

    Router::new()
        .route("/api/health", get(health))
        .route("/api/files", get(list_files))
        .route("/api/file", get(get_file))
        .route("/api/task", post(submit_task))
        .route("/ws", get(ws_handler))
        .fallback(fallback_not_found)
        .layer(middleware::from_fn(cors_middleware))
        .with_state(state)
}

/// Inicializa o `SwarmEngine` padrão, registra os agentes, inicia o `@monitor` e serve HTTP+WS.
pub async fn run_server(port: u16, project_root: PathBuf) -> Result<(), std::io::Error> {
    let addr = SocketAddr::from(([0, 0, 0, 0], port));
    let listener = tokio::net::TcpListener::bind(addr).await?;

    let engine = SwarmEngine::new(create_llm_provider());
    engine.register_default_agents().await;
    engine.start_monitor(MONITOR_INTERVAL_MS, MONITOR_STALE_MS);

    let app = build_router(Arc::clone(&engine), project_root.clone());

    println!("[swarm-ide] server-rust em http://localhost:{port}");
    println!("[swarm-ide] ws em ws://localhost:{port}/ws");
    println!(
        "[swarm-ide] explorando projeto em {}",
        project_root.to_string_lossy()
    );

    let result = axum::serve(listener, app).await;
    engine.stop();
    result
}

async fn health(State(state): State<AppState>) -> impl IntoResponse {
    Json(json!({
        "ok": true,
        "projectRoot": state.project_root.to_string_lossy(),
    }))
}

async fn list_files(State(state): State<AppState>) -> Response {
    match walk_project_files(&state.project_root, &state.project_root) {
        Ok(files) => Json(json!({ "files": files })).into_response(),
        Err(_) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({
                "error": "projeto não encontrado",
                "projectRoot": state.project_root.to_string_lossy(),
            })),
        )
            .into_response(),
    }
}

async fn get_file(State(state): State<AppState>, Query(query): Query<FileQuery>) -> Response {
    let rel = query.path.unwrap_or_default();
    let Some(full) = safe_resolve(&state.project_root, &rel) else {
        return (
            StatusCode::NOT_FOUND,
            Json(json!({ "error": "arquivo não encontrado" })),
        )
            .into_response();
    };

    if !full.is_file() {
        return (
            StatusCode::NOT_FOUND,
            Json(json!({ "error": "arquivo não encontrado" })),
        )
            .into_response();
    }

    match std::fs::read_to_string(&full) {
        Ok(content) => Json(json!({ "path": rel, "content": content })).into_response(),
        Err(_) => (
            StatusCode::NOT_FOUND,
            Json(json!({ "error": "arquivo não encontrado" })),
        )
            .into_response(),
    }
}

async fn submit_task(
    State(state): State<AppState>,
    payload: Result<Json<TaskBody>, axum::extract::rejection::JsonRejection>,
) -> Response {
    let Ok(Json(body)) = payload else {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({ "error": "JSON inválido" })),
        )
            .into_response();
    };

    let prompt = body.prompt.unwrap_or_default();
    if prompt.is_empty() {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({ "error": "Error: prompt vazio" })),
        )
            .into_response();
    }

    let engine = Arc::clone(&state.engine);
    tokio::spawn(async move {
        engine.submit_task(prompt).await;
    });

    Json(json!({ "ok": true })).into_response()
}

async fn ws_handler(ws: WebSocketUpgrade, State(state): State<AppState>) -> impl IntoResponse {
    ws.on_upgrade(move |socket| handle_ws(socket, state.engine))
}

async fn handle_ws(mut socket: WebSocket, engine: Arc<SwarmEngine>) {
    let mut rx = engine.subscribe();

    let history_evt = ServerEvent::BoardHistory {
        posts: engine.get_history().await,
    };
    if let Ok(json_str) = serde_json::to_string(&history_evt) {
        if socket.send(Message::Text(json_str)).await.is_err() {
            return;
        }
    }

    let agents_evt = ServerEvent::Agents {
        agents: engine.get_agents().await,
    };
    if let Ok(json_str) = serde_json::to_string(&agents_evt) {
        if socket.send(Message::Text(json_str)).await.is_err() {
            return;
        }
    }

    loop {
        tokio::select! {
            evt_res = rx.recv() => {
                match evt_res {
                    Ok(evt) => {
                        if let Ok(json_str) = serde_json::to_string(&evt) {
                            if socket.send(Message::Text(json_str)).await.is_err() {
                                break;
                            }
                        }
                    }
                    Err(RecvError::Lagged(skipped)) => {
                        eprintln!("[ws] cliente atrasado, {skipped} eventos descartados");
                    }
                    Err(RecvError::Closed) => break,
                }
            }
            ws_msg = socket.recv() => {
                match ws_msg {
                    Some(Ok(Message::Text(raw))) => {
                        match serde_json::from_str::<ClientEvent>(&raw) {
                            Ok(evt) => {
                                let engine_clone = Arc::clone(&engine);
                                tokio::spawn(async move {
                                    engine_clone.handle_client_event(evt).await;
                                });
                            }
                            Err(err) => {
                                eprintln!("[ws] evento inválido: {err}");
                            }
                        }
                    }
                    Some(Ok(Message::Close(_))) | None => break,
                    Some(Ok(_)) => {}
                    Some(Err(err)) => {
                        eprintln!("[ws] erro na conexão: {err}");
                        break;
                    }
                }
            }
        }
    }
}

async fn fallback_not_found() -> impl IntoResponse {
    (StatusCode::NOT_FOUND, Json(json!({ "error": "not found" })))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::llm::create_llm_provider;

    #[test]
    fn cors_headers_allow_tauri_and_vite_origins() {
        let mut headers = axum::http::HeaderMap::new();
        apply_cors_headers(&mut headers);
        assert_eq!(headers.get(header::ACCESS_CONTROL_ALLOW_ORIGIN).unwrap(), "*");
        assert_eq!(
            headers.get(header::ACCESS_CONTROL_ALLOW_METHODS).unwrap(),
            "GET, POST, OPTIONS"
        );
        assert_eq!(
            headers.get(header::ACCESS_CONTROL_ALLOW_HEADERS).unwrap(),
            "Content-Type, Authorization"
        );
    }

    #[tokio::test]
    async fn build_router_constructs_without_panicking() {
        let engine = SwarmEngine::new(create_llm_provider());
        let _router = build_router(engine, resolve_project_root());
    }
}
