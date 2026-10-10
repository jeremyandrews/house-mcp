//! The MCP tools.
//!
//! Read tools are in `read`, write tools in `write`, the two importers in
//! `import` and the plan renderer in `plan`. A write tool is one in the write
//! router; the handler refuses those to a read only caller before they run,
//! and hides them from that caller's tool list.

mod import;
mod plan;
mod read;
mod write;

use std::sync::Arc;

use axum::http::request::Parts;
use rmcp::{
    ErrorData, RoleServer, ServerHandler,
    handler::server::{router::tool::ToolRouter, tool::ToolCallContext},
    model::{
        CallToolRequestParams, CallToolResponse, CallToolResult, ContentBlock, Implementation, ListToolsResult,
        PaginatedRequestParams, ServerCapabilities, ServerConfig,
    },
    service::RequestContext,
};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sqlx::PgPool;

pub use write::{NewFile, store_file};

use crate::{
    auth::Caller,
    error,
    files::{FileStore, Sha256Hex},
};

#[derive(Clone)]
pub struct House {
    db: PgPool,
    files: Arc<FileStore>,
    read_tools: Arc<ToolRouter<Self>>,
    write_tools: Arc<ToolRouter<Self>>,
}

impl House {
    pub fn new(db: PgPool, files: Arc<FileStore>) -> Self {
        Self {
            db,
            files,
            read_tools: Arc::new(Self::read_router() + Self::plan_router()),
            write_tools: Arc::new(Self::write_router() + Self::import_router()),
        }
    }
}

/// The caller the auth middleware attached to this HTTP request.
fn caller(context: &RequestContext<RoleServer>) -> Result<Caller, ErrorData> {
    context
        .extensions
        .get::<Parts>()
        .and_then(|parts| parts.extensions.get::<Caller>())
        .cloned()
        .ok_or_else(|| ErrorData::invalid_request("request carries no authenticated caller", None))
}

/// The caller, if allowed to write. Write tools take their author from here.
fn writer(parts: &Parts) -> Result<&Caller, ErrorData> {
    match parts.extensions.get::<Caller>() {
        Some(caller) if caller.can_write => Ok(caller),
        _ => Err(ErrorData::invalid_request("this token may only read", None)),
    }
}

impl ServerHandler for House {
    fn get_info(&self) -> ServerConfig {
        ServerConfig::new(ServerCapabilities::builder().enable_tools().build())
            .with_server_info(Implementation::new("house-mcp", env!("CARGO_PKG_VERSION")))
            .with_instructions(INSTRUCTIONS)
    }

    async fn list_tools(
        &self,
        _request: Option<PaginatedRequestParams>,
        context: RequestContext<RoleServer>,
    ) -> Result<ListToolsResult, ErrorData> {
        let mut tools = self.read_tools.list_all();
        if caller(&context)?.can_write {
            tools.extend(self.write_tools.list_all());
        }
        tools.sort_by(|a, b| a.name.cmp(&b.name));
        Ok(ListToolsResult::with_all_items(tools))
    }

    async fn call_tool(
        &self,
        request: CallToolRequestParams,
        context: RequestContext<RoleServer>,
    ) -> Result<CallToolResponse, ErrorData> {
        let can_write = caller(&context)?.can_write;
        if self.write_tools.has_route(&request.name) {
            if !can_write {
                return Err(ErrorData::invalid_request(
                    format!("{} needs the read and write token", request.name),
                    None,
                ));
            }
            return self
                .write_tools
                .call(ToolCallContext::new(self, request, context))
                .await;
        }
        self.read_tools.call(ToolCallContext::new(self, request, context)).await
    }
}

const INSTRUCTIONS: &str = "A model of a house and its grounds. Each site has its own frame: metres, x east, y north, \
with an origin chosen when the site was made. Every coordinate a tool takes or returns is in that frame unless a \
tool says otherwise. Ask spatial questions with what_is_at, in_space, nearest and measure rather than working them \
out from coordinates. Every write needs a reason, which is kept with the change.";

/// The kinds of thing that can be deleted, restored or have a history.
#[derive(Debug, Clone, Copy, Deserialize, Serialize, JsonSchema, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Kind {
    Site,
    Floor,
    Space,
    Object,
    File,
    Note,
}

impl Kind {
    fn table(self) -> &'static str {
        match self {
            Self::Site => "site",
            Self::Floor => "floor",
            Self::Space => "space",
            Self::Object => "object",
            Self::File => "file",
            Self::Note => "note",
        }
    }
}

/// A place to measure from: a point in the site frame, or a space or object.
#[derive(Debug, Clone, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum Location {
    /// `[x, y]` in metres.
    Point([f64; 2]),
    Space(i64),
    Object(i64),
}

/// Optional links from a file or note to what it is about.
#[derive(Debug, Default, Clone, Deserialize, JsonSchema)]
pub struct Links {
    pub site_id: Option<i64>,
    pub floor_id: Option<i64>,
    pub space_id: Option<i64>,
    pub object_id: Option<i64>,
}

/// The oldest live record of a file content.
pub struct LiveFile {
    pub sha: Sha256Hex,
    pub id: i64,
    pub name: String,
    pub size: i64,
}

/// Finds the live record for a caller's sha256, refused naming `field` when
/// the hash is malformed or no live record has it.
pub async fn live_file(pool: &PgPool, field: &str, sha: &str) -> Result<LiveFile, ErrorData> {
    let sha = Sha256Hex::parse(sha).ok_or_else(|| error::invalid(field, "not a lowercase hex sha256"))?;
    let (id, name, size): (i64, String, i64) =
        sqlx::query_as("SELECT id, name, size FROM file WHERE sha256 = $1 AND deleted_at IS NULL ORDER BY id LIMIT 1")
            .bind(sha.as_str())
            .fetch_optional(pool)
            .await
            .map_err(error::db)?
            .ok_or_else(|| error::invalid(field, "no live file with this hash"))?;
    Ok(LiveFile { sha, id, name, size })
}

/// The largest stored file an import reads. Parsing builds the whole drawing
/// or document in memory at many times its size on disk, before any entity or
/// feature limit can apply.
pub const MAX_IMPORT_FILE: i64 = 16 * 1024 * 1024;

impl House {
    /// A stored file's bytes for an import, refused before reading when it is
    /// larger than an import may parse.
    async fn import_bytes(&self, field: &str, sha: &str) -> Result<(LiveFile, Vec<u8>), ErrorData> {
        let file = live_file(&self.db, field, sha).await?;
        if file.size > MAX_IMPORT_FILE {
            return Err(error::invalid(field, "is larger than 16 MiB, the most an import reads"));
        }
        let bytes = self
            .files
            .get(&file.sha)
            .await
            .map_err(|e| error::internal("reading a file", e))?;
        Ok((file, bytes))
    }
}

fn json_result(value: impl Serialize) -> Result<CallToolResult, ErrorData> {
    let text = serde_json::to_string_pretty(&value).map_err(|e| error::internal("encoding a result", e))?;
    Ok(CallToolResult::success(vec![ContentBlock::text(text)]))
}

/// WKB from PostGIS as a GeoJSON value for a result.
fn geojson(wkb: &[u8]) -> Result<Value, ErrorData> {
    crate::geom::to_geojson(wkb).map_err(|e| error::internal("reading stored geometry", e))
}

/// A caller's GeoJSON geometry as WKB, refused with the field's name when it
/// is not GeoJSON.
fn wkb(field: &str, geometry: &Value) -> Result<Vec<u8>, ErrorData> {
    crate::geom::to_wkb(geometry).map_err(|e| error::invalid(field, e))
}

fn limit(field: &str, value: Option<i64>, default: i64, max: i64) -> Result<i64, ErrorData> {
    match value.unwrap_or(default) {
        n if (1..=max).contains(&n) => Ok(n),
        _ => Err(error::invalid(field, format!("must be between 1 and {max}"))),
    }
}

fn reason(value: &str) -> Result<&str, ErrorData> {
    let value = value.trim();
    if value.is_empty() {
        return Err(error::invalid("reason", "is required"));
    }
    if value.contains('\0') {
        return Err(error::invalid("reason", "contains a NUL character"));
    }
    if value.chars().count() > 1000 {
        return Err(error::invalid("reason", "is longer than 1000 characters"));
    }
    Ok(value)
}
