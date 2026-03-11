mod routes;

use std::path::PathBuf;
use std::sync::Arc;

use axum::{
    routing::{delete, get, post},
    Router,
};
use anyhow::Context;
use lua_host::WorkerRegistry;
use tokio::net::TcpListener;
use tower_http::{cors::CorsLayer, trace::TraceLayer};

#[derive(Clone)]
pub struct AppState {
    pub pool: Arc<WorkerRegistry>,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "lua_server=info,tower_http=info".into()),
        )
        .init();

    let worker_bin = std::env::var("LUA_WORKER_BIN")
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from("./lua-worker"));
    anyhow::ensure!(
        worker_bin.exists(),
        "lua-worker binary not found at '{}' (set LUA_WORKER_BIN to override)",
        worker_bin.display()
    );

    let bind_addr = std::env::var("LUA_BIND").unwrap_or_else(|_| "127.0.0.1:8080".to_string());

    let sandbox_root = std::env::var("LUA_SANDBOX_ROOT")
        .map(PathBuf::from)
        .unwrap_or_else(|_| std::env::temp_dir().join("lua-sandboxes"));
    std::fs::create_dir_all(&sandbox_root)
        .with_context(|| format!("create sandbox root {}", sandbox_root.display()))?;

    let pool = Arc::new(WorkerRegistry::new(worker_bin, sandbox_root));
    let state = AppState { pool };

    let app = Router::new()
        .route("/workers", post(routes::spawn_worker))
        .route("/workers", get(routes::list_workers))
        .route("/workers/{id}/health", get(routes::health_worker))
        .route("/workers/{id}/exec", post(routes::exec))
        .route("/workers/{id}/call", post(routes::call))
        .route("/workers/{id}", delete(routes::shutdown_worker))
        .route("/eval", post(routes::eval))
        .layer(CorsLayer::permissive())
        .layer(TraceLayer::new_for_http())
        .with_state(state);

    let listener = TcpListener::bind(&bind_addr).await?;
    tracing::info!("listening on {bind_addr}");
    axum::serve(listener, app).await?;

    Ok(())
}
