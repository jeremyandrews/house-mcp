//! An MCP server holding a spatial model of a house and its grounds in
//! PostGIS.

mod auth;
pub mod config;
pub mod db;
pub mod dxf_import;
mod error;
mod files;
pub mod geom;
mod markup;
mod mcp;
pub mod render;
pub mod units;
mod view;
mod web;

use std::sync::{Arc, OnceLock};

use axum::{
    Router,
    extract::Request,
    http::StatusCode,
    middleware,
    response::{IntoResponse, Response},
};
use rmcp::transport::streamable_http_server::{
    StreamableHttpServerConfig, StreamableHttpService, session::never::NeverSessionManager,
};
use tokio::task::JoinHandle;
use tower::ServiceExt;

use crate::{auth::Tokens, config::Config, db::Retry, files::FileStore};

/// Bytes in a mebibyte.
pub(crate) const MIB: usize = 1024 * 1024;

/// The MCP request limit: a 25 MiB file as base64 with room for the rest of
/// the call.
const MAX_MCP_BODY: usize = 36 * MIB;

/// Connects once, migrates and builds every route.
pub async fn app(config: &Config) -> Result<Router, String> {
    build(config, Retry::ONCE).await
}

/// Routes that answer at once, and the work of connecting behind them. Until
/// the database is reached and migrated every request is answered 503
/// Service Unavailable, `/healthz` included, so the pod reports not ready
/// rather than exiting while the database starts. The handle finishes when
/// the routes are live, or with the error once the database has stayed out
/// of reach for as long as `retry` allows.
pub fn start(config: Config, retry: Retry) -> (Router, JoinHandle<Result<(), String>>) {
    let live: Arc<OnceLock<Router>> = Arc::default();
    let ready = tokio::spawn({
        let live = live.clone();
        async move {
            let router = build(&config, retry).await?;
            let _ = live.set(router);
            tracing::info!("ready");
            Ok(())
        }
    });
    let router = Router::new().fallback(move |request: Request| {
        let live = live.clone();
        async move {
            match live.get() {
                Some(router) => match router.clone().oneshot(request).await {
                    Ok(response) => response,
                    Err(never) => match never {},
                },
                None => starting(),
            }
        }
    });
    (router, ready)
}

fn starting() -> Response {
    (StatusCode::SERVICE_UNAVAILABLE, "starting: waiting for the database").into_response()
}

async fn build(config: &Config, retry: Retry) -> Result<Router, String> {
    let pool = db::connect_with_retry(&config.database_url, retry)
        .await
        .map_err(|e| format!("connecting to the database: {e}"))?;
    db::MIGRATOR
        .run(&pool)
        .await
        .map_err(|e| format!("migrating the database: {e}"))?;
    let files = Arc::new(
        FileStore::open(&config.files_dir)
            .await
            .map_err(|e| format!("opening {}: {e}", config.files_dir.display()))?,
    );
    let tokens = Arc::new(Tokens::new(&config.read_write, &config.read_only));

    let house = mcp::House::new(pool.clone(), files.clone());
    let mut mcp_config = StreamableHttpServerConfig::default().with_allowed_hosts(config.allowed_hosts.clone());
    // Stateless, so every request carries its own token and is checked on
    // its own.
    mcp_config.legacy_session_mode = false;
    mcp_config.json_response = true;
    mcp_config.max_request_body_bytes = MAX_MCP_BODY;
    let mcp_service = StreamableHttpService::new(
        move || Ok(house.clone()),
        Arc::new(NeverSessionManager::default()),
        mcp_config,
    );
    let mcp_routes = Router::new()
        .nest_service("/mcp", mcp_service)
        .layer(middleware::from_fn_with_state(tokens.clone(), auth::bearer));

    Ok(web::routes(web::Web { db: pool, files }, tokens).merge(mcp_routes))
}
