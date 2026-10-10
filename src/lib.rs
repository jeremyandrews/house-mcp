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
mod web;

use std::sync::Arc;

use axum::{Router, middleware};
use rmcp::transport::streamable_http_server::{
    StreamableHttpServerConfig, StreamableHttpService, session::never::NeverSessionManager,
};

use crate::{auth::Tokens, config::Config, files::FileStore};

/// The MCP request limit: a 25 MiB file as base64 with room for the rest of
/// the call.
const MAX_MCP_BODY: usize = 36 * 1024 * 1024;

/// Connects, migrates and builds every route.
pub async fn app(config: &Config) -> Result<Router, String> {
    let pool = db::connect(&config.database_url)
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
