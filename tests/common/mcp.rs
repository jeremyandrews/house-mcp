//! The server over HTTP for the end to end tests: a fresh database and file
//! store per server, and MCP clients holding either token.

use house_mcp::config::{Config, Token};
use rmcp::{
    RoleClient, ServiceExt,
    model::{CallToolRequestParams, CallToolResult},
    service::RunningService,
    transport::{StreamableHttpClientTransport, streamable_http_client::StreamableHttpClientTransportConfig},
};
use serde_json::Value;

use super::TestDb;

pub const RW: &str = "rw-token-for-tests-0123456789abcdef0123456789";
pub const RO: &str = "ro-token-for-tests-0123456789abcdef0123456789";

pub struct Server {
    pub base: String,
    pub _files: tempfile::TempDir,
    pub db: TestDb,
}

pub async fn start() -> Server {
    let db = TestDb::new().await;
    let files = tempfile::tempdir().expect("tempdir");
    let config = Config {
        database_url: db.house_url(),
        files_dir: files.path().to_owned(),
        listen: "127.0.0.1:0".parse().expect("address"),
        allowed_hosts: vec!["127.0.0.1".into()],
        read_write: Token {
            name: "jesse".into(),
            secret: RW.into(),
        },
        read_only: Token {
            name: "readonly".into(),
            secret: RO.into(),
        },
    };
    let app = house_mcp::app(&config).await.expect("app");
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let base = format!("http://{}", listener.local_addr().expect("address"));
    tokio::spawn(async move { axum::serve(listener, app).await });
    Server {
        base,
        _files: files,
        db,
    }
}

pub async fn client(server: &Server, token: &str) -> RunningService<RoleClient, ()> {
    let config = StreamableHttpClientTransportConfig::with_uri(format!("{}/mcp", server.base)).auth_header(token);
    ().serve(StreamableHttpClientTransport::from_config(config))
        .await
        .expect("MCP client")
}

pub async fn call(
    client: &RunningService<RoleClient, ()>,
    tool: &'static str,
    args: Value,
) -> Result<CallToolResult, String> {
    let Value::Object(args) = args else {
        panic!("arguments are an object")
    };
    client
        .call_tool(CallToolRequestParams::new(tool).with_arguments(args))
        .await
        .map_err(|e| e.to_string())
}

pub async fn ok(client: &RunningService<RoleClient, ()>, tool: &'static str, args: Value) -> Value {
    let result = call(client, tool, args).await.unwrap_or_else(|e| panic!("{tool}: {e}"));
    let text = result.content.iter().find_map(|c| c.as_text()).expect("text content");
    serde_json::from_str(&text.text).unwrap_or_else(|_| Value::String(text.text.clone()))
}

/// The SVG among a plan's text blocks, if it has one.
pub fn svg_of(result: &CallToolResult) -> Option<String> {
    result
        .content
        .iter()
        .filter_map(|c| c.as_text())
        .find(|t| t.text.starts_with("<svg"))
        .map(|t| t.text.clone())
}

/// One field of every item in a list, in order.
pub fn column(list: &Value, key: &str) -> Vec<Value> {
    list.as_array()
        .expect("a list")
        .iter()
        .map(|item| item[key].clone())
        .collect()
}

/// The names in `list[key]`, sorted, for lists whose order is not promised.
pub fn sorted_names(list: &Value, key: &str) -> Vec<String> {
    let mut names: Vec<String> = column(&list[key], "name")
        .iter()
        .map(|name| name.as_str().expect("name").to_owned())
        .collect();
    names.sort();
    names
}
