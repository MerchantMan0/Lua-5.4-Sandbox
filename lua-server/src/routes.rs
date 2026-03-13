use std::sync::Arc;

use axum::{
    extract::{Path, State},
    http::StatusCode,
    response::{IntoResponse, Response},
    Json,
};
use lua_host::{WorkerError, WorkerRegistry};
use lua_protocol::{LuaError, LuaValue, Response as LuaResponse};
use serde::Deserialize;
use serde_json::{json, Value};
use uuid::Uuid;

use crate::AppState;

pub struct ApiError {
    status: StatusCode,
    message: String,
}

impl ApiError {
    fn not_found(msg: impl ToString) -> Self {
        ApiError { status: StatusCode::NOT_FOUND, message: msg.to_string() }
    }

    fn bad_gateway(msg: impl ToString) -> Self {
        ApiError { status: StatusCode::BAD_GATEWAY, message: msg.to_string() }
    }

    fn gateway_timeout(msg: impl ToString) -> Self {
        ApiError { status: StatusCode::GATEWAY_TIMEOUT, message: msg.to_string() }
    }

    fn too_many_requests(msg: impl ToString) -> Self {
        ApiError { status: StatusCode::TOO_MANY_REQUESTS, message: msg.to_string() }
    }

    fn internal(err: anyhow::Error) -> Self {
        tracing::error!("internal server error: {:#}", err);
        ApiError { status: StatusCode::INTERNAL_SERVER_ERROR, message: err.to_string() }
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        (self.status, Json(json!({ "error": self.message }))).into_response()
    }
}

fn map_err(err: WorkerError) -> ApiError {
    match err {
        WorkerError::NotFound => ApiError::not_found(err),
        WorkerError::Busy => ApiError::too_many_requests(err),
        WorkerError::Timeout => ApiError::gateway_timeout(err),
        WorkerError::Crashed(_) => ApiError::bad_gateway(err),
        WorkerError::Internal(e) => ApiError::internal(e),
    }
}

fn lua_value_to_json(v: LuaValue) -> Value {
    match v {
        LuaValue::Nil => Value::Null,
        LuaValue::Bool(b) => json!(b),
        LuaValue::Integer(i) => json!(i),
        LuaValue::Float(f) => json!(f),
        // fix this is a hack: from_utf8_lossy replaces invalid UTF-8 with replacement chars.
        LuaValue::String(bytes) => json!(String::from_utf8_lossy(&bytes).into_owned()),
        LuaValue::Table(pairs) => lua_table_to_json(pairs),
    }
}

fn lua_table_to_json(pairs: Vec<(LuaValue, LuaValue)>) -> Value {
    let is_sequence = pairs.iter().enumerate().all(|(i, (k, _))| {
        matches!(k, LuaValue::Integer(n) if *n == (i as i64 + 1))
    });
    if is_sequence {
        return Value::Array(pairs.into_iter().map(|(_, v)| lua_value_to_json(v)).collect());
    }

    let all_string_keys = pairs.iter().all(|(k, _)| matches!(k, LuaValue::String(_)));
    if all_string_keys {
        return Value::Object(
            pairs
                .into_iter()
                .map(|(k, v)| {
                    let key = match k {
                        // fix this is a hack: from_utf8_lossy replaces invalid UTF-8 with replacement chars.
                        LuaValue::String(b) => String::from_utf8_lossy(&b).into_owned(),
                        _ => unreachable!(),
                    };
                    (key, lua_value_to_json(v))
                })
                .collect(),
        );
    }

    // fix this is a hack (design): encoding as [[k,v],...] loses object semantics for mixed tables.
    // Mixed key table pairs
    Value::Array(
        pairs
            .into_iter()
            .map(|(k, v)| Value::Array(vec![lua_value_to_json(k), lua_value_to_json(v)]))
            .collect(),
    )
}

fn json_to_lua_value(v: Value) -> LuaValue {
    match v {
        Value::Null => LuaValue::Nil,
        Value::Bool(b) => LuaValue::Bool(b),
        Value::Number(n) => {
            if let Some(i) = n.as_i64() {
                LuaValue::Integer(i)
            } else {
                // fix this is a hack: non-representable JSON numbers silently become 0.
                LuaValue::Float(n.as_f64().unwrap_or(0.0))
            }
        }
        Value::String(s) => LuaValue::String(s.into_bytes()),
        Value::Array(arr) => LuaValue::Table(
            arr.into_iter()
                .enumerate()
                .map(|(i, v)| (LuaValue::Integer(i as i64 + 1), json_to_lua_value(v)))
                .collect(),
        ),
        Value::Object(map) => LuaValue::Table(
            map.into_iter()
                .map(|(k, v)| (LuaValue::String(k.into_bytes()), json_to_lua_value(v)))
                .collect(),
        ),
    }
}

fn lua_response_to_json(r: LuaResponse) -> Value {
    match r {
        LuaResponse::Ok { values, console, gas_remaining, memory_used } => json!({
            "status": "ok",
            "return": values.into_iter().map(lua_value_to_json).collect::<Vec<_>>(),
            "console": console,
            "gas_remaining": gas_remaining,
            "memory_used": memory_used,
        }),
        LuaResponse::Error(e) => json!({
            "status": "error",
            "error": lua_error_to_json(e),
        }),
    }
}

fn lua_error_to_json(e: LuaError) -> Value {
    match e {
        LuaError::Runtime { message, traceback } => {
            json!({ "kind": "Runtime", "message": message, "traceback": traceback })
        }
        LuaError::Syntax(msg) => json!({ "kind": "Syntax", "message": msg }),
        LuaError::Io { path, message } => json!({ "kind": "Io", "path": path, "message": message }),
        LuaError::GasExceeded => json!({ "kind": "GasExceeded" }),
        LuaError::MemoryExceeded => json!({ "kind": "MemoryExceeded" }),
        LuaError::SerializationDepthExceeded => json!({ "kind": "SerializationDepthExceeded" }),
    }
}

#[derive(Deserialize)]
pub struct ExecBody {
    script: String,
}

#[derive(Deserialize)]
pub struct CallBody {
    function: String,
    args: Vec<Value>,
}

#[derive(Deserialize)]
pub struct EvalBody {
    script: String,
}

// fix this is a hack (design): drop-based shutdown + explicit disarm to avoid double shutdown on success path.
// shuts down the ephemeral eval worker on drop
// guards against early returns and panics
struct EvalGuard {
    id: Option<Uuid>,
    pool: Arc<WorkerRegistry>,
}

impl EvalGuard {
    fn new(id: Uuid, pool: Arc<WorkerRegistry>) -> Self {
        Self { id: Some(id), pool }
    }

    // prevents the drop impl from spawning a background shutdown
    fn disarm(&mut self) {
        self.id = None;
    }
}

impl Drop for EvalGuard {
    fn drop(&mut self) {
        if let Some(id) = self.id.take() {
            let pool = Arc::clone(&self.pool);
            tokio::spawn(async move {
                pool.shutdown(id).await.ok();
            });
        }
    }
}

pub async fn spawn_worker(
    State(state): State<AppState>,
) -> Result<impl IntoResponse, ApiError> {
    let id = state.pool.spawn().await.map_err(map_err)?;
    Ok((StatusCode::CREATED, Json(json!({ "id": id }))))
}

pub async fn list_workers(State(state): State<AppState>) -> impl IntoResponse {
    Json(json!({ "ids": state.pool.worker_ids() }))
}

pub async fn exec(
    State(state): State<AppState>,
    Path(id): Path<Uuid>,
    Json(body): Json<ExecBody>,
) -> Result<impl IntoResponse, ApiError> {
    let response = state.pool.exec(id, body.script).await.map_err(map_err)?;
    Ok(Json(lua_response_to_json(response)))
}

pub async fn call(
    State(state): State<AppState>,
    Path(id): Path<Uuid>,
    Json(body): Json<CallBody>,
) -> Result<impl IntoResponse, ApiError> {
    let args: Vec<LuaValue> = body.args.into_iter().map(json_to_lua_value).collect();
    let response = state.pool.call(id, body.function, args).await.map_err(map_err)?;
    Ok(Json(lua_response_to_json(response)))
}

pub async fn shutdown_worker(
    State(state): State<AppState>,
    Path(id): Path<Uuid>,
) -> Result<impl IntoResponse, ApiError> {
    state.pool.shutdown(id).await.map_err(map_err)?;
    Ok(StatusCode::NO_CONTENT)
}

pub async fn health_worker(
    State(state): State<AppState>,
    Path(id): Path<Uuid>,
) -> Result<impl IntoResponse, ApiError> {
    state.pool.ping(id).await.map_err(map_err)?;
    Ok(Json(json!({ "status": "ok" })))
}

// fix this is a hack (design): spawns full worker process per one-shot eval instead of dedicated pool.
pub async fn eval(
    State(state): State<AppState>,
    Json(body): Json<EvalBody>,
) -> Result<impl IntoResponse, ApiError> {
    let id = state.pool.spawn().await.map_err(map_err)?;
    let mut guard = EvalGuard::new(id, Arc::clone(&state.pool));

    let response = state.pool.exec(id, body.script).await;

    // Disarm so the drop doesn't also fire.
    guard.disarm();
    state.pool.shutdown(id).await.ok();

    Ok(Json(lua_response_to_json(response.map_err(map_err)?)))
}
