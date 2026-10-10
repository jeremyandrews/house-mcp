//! The plain HTTP routes beside MCP: health, file upload and download for
//! files too large for a tool call, and a small upload page for a phone.

use std::sync::Arc;

use axum::{
    Extension, Json, Router,
    body::{Body, Bytes},
    extract::{DefaultBodyLimit, Multipart, Path, Query, State},
    http::{HeaderMap, HeaderValue, StatusCode, header},
    middleware,
    response::{Html, IntoResponse, Redirect, Response},
    routing::get,
};
use base64::{Engine, engine::general_purpose::STANDARD};
use rmcp::{ErrorData, model::ErrorCode};
use serde::Deserialize;
use sha2::{Digest, Sha256};
use sqlx::{PgPool, Row};
use tokio_util::io::ReaderStream;

use crate::{
    auth::{self, Caller, Tokens},
    files::{FileStore, Sha256Hex},
    markup::escape,
    mcp::{Links, NewFile, live_file, store_file},
};

/// Uploads over HTTP may be larger than over MCP, but not unbounded.
pub const MAX_HTTP_FILE: usize = 100 * 1024 * 1024;
const PAGE_STYLE: &str = "body{font-family:system-ui,sans-serif;margin:1rem;max-width:48rem}\
table{border-collapse:collapse;width:100%}td,th{padding:.3rem;border-bottom:1px solid #ddd;text-align:left}\
form{margin:1rem 0;display:grid;gap:.5rem}";

#[derive(Clone)]
pub struct Web {
    pub db: PgPool,
    pub files: Arc<FileStore>,
}

pub fn routes(web: Web, tokens: Arc<Tokens>) -> Router {
    let api = Router::new()
        .route("/api/files", axum::routing::post(api_upload))
        .route("/api/files/{sha256}", get(download))
        .layer(middleware::from_fn_with_state(tokens.clone(), auth::bearer));
    let page = Router::new()
        .route("/", get(page).post(page_upload))
        .route("/files/{sha256}", get(download))
        .layer(middleware::from_fn_with_state(tokens, auth::basic_read_write));
    Router::new()
        .route("/healthz", get(healthz))
        .merge(api)
        .merge(page)
        .layer(DefaultBodyLimit::max(MAX_HTTP_FILE + 64 * 1024))
        .with_state(web)
}

async fn healthz(State(web): State<Web>) -> StatusCode {
    match sqlx::query("SELECT 1").execute(&web.db).await {
        Ok(_) => StatusCode::OK,
        Err(_) => StatusCode::SERVICE_UNAVAILABLE,
    }
}

#[derive(Debug, Deserialize)]
struct UploadQuery {
    name: String,
    reason: Option<String>,
    site_id: Option<i64>,
    floor_id: Option<i64>,
    space_id: Option<i64>,
    object_id: Option<i64>,
}

/// `POST /api/files?name=...`: the body is the file, its Content-Type the
/// media type.
async fn api_upload(
    State(web): State<Web>,
    Extension(caller): Extension<Caller>,
    Query(q): Query<UploadQuery>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    if !caller.can_write {
        return (StatusCode::FORBIDDEN, "this token may only read").into_response();
    }
    let media_type = headers
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .filter(|v| *v != "application/x-www-form-urlencoded");
    let links = Links {
        site_id: q.site_id,
        floor_id: q.floor_id,
        space_id: q.space_id,
        object_id: q.object_id,
    };
    let reason = q.reason.unwrap_or_else(|| "uploaded over HTTP".into());
    let file = NewFile {
        name: &q.name,
        media_type,
        bytes: &body,
        links: &links,
    };
    match store_file(&web.db, &web.files, &caller.name, &reason, file).await {
        Ok(stored) => (StatusCode::CREATED, Json(stored)).into_response(),
        Err(e) => error_response(e),
    }
}

/// Always an attachment of an opaque type, so a stored HTML or SVG file is
/// never rendered by the browser in this origin. The file is streamed, so a
/// few large downloads at once cannot exhaust the pod's memory.
async fn download(State(web): State<Web>, Path(sha256): Path<String>) -> Response {
    if Sha256Hex::parse(&sha256).is_none() {
        return (StatusCode::BAD_REQUEST, "sha256: not a lowercase hex sha256").into_response();
    }
    // The hash is well formed, so the only way left to fail as the caller's
    // fault is that no live file has it.
    let file = match live_file(&web.db, "sha256", &sha256).await {
        Ok(file) => file,
        Err(e) if e.code == ErrorCode::INVALID_PARAMS => {
            return (StatusCode::NOT_FOUND, "no live file with this hash").into_response();
        }
        Err(e) => return error_response(e),
    };
    let (content, length) = match web.files.open_read(&file.sha).await {
        Ok(opened) => opened,
        Err(e) => return error_response(crate::error::internal("reading a file", e)),
    };
    (
        [
            (
                header::CONTENT_TYPE,
                HeaderValue::from_static("application/octet-stream"),
            ),
            (header::CONTENT_DISPOSITION, content_disposition(&file.name)),
            (header::X_CONTENT_TYPE_OPTIONS, HeaderValue::from_static("nosniff")),
            (header::CONTENT_LENGTH, HeaderValue::from(length)),
        ],
        Body::from_stream(ReaderStream::new(content)),
    )
        .into_response()
}

async fn page(State(web): State<Web>) -> Response {
    let rows = match sqlx::query(
        "SELECT name, media_type, size, sha256, created_at FROM file WHERE deleted_at IS NULL ORDER BY created_at DESC, id DESC LIMIT 500",
    )
    .fetch_all(&web.db)
    .await
    {
        Ok(rows) => rows,
        Err(e) => return error_response(crate::error::db(e)),
    };
    let mut table = String::new();
    for row in &rows {
        let sha: String = row.get("sha256");
        let created: chrono::DateTime<chrono::Utc> = row.get("created_at");
        table.push_str(&format!(
            "<tr><td><a href=\"/files/{sha}\">{}</a></td><td>{}</td><td>{}</td><td>{}</td></tr>",
            escape(&row.get::<String, _>("name")),
            escape(&row.get::<String, _>("media_type")),
            row.get::<i64, _>("size"),
            created.format("%Y-%m-%d %H:%M"),
        ));
    }
    let body = format!(
        "<!doctype html><html><head><meta charset=\"utf-8\"><meta name=\"viewport\" content=\"width=device-width\">\
         <title>House files</title><style>{PAGE_STYLE}</style></head><body><h1>House files</h1>\
         <form method=\"post\" action=\"/\" enctype=\"multipart/form-data\">\
         <input type=\"file\" name=\"file\" required>\
         <input type=\"text\" name=\"reason\" placeholder=\"What this is\" required>\
         <button type=\"submit\">Upload</button></form>\
         <table><thead><tr><th>Name</th><th>Type</th><th>Bytes</th><th>Added</th></tr></thead><tbody>{table}</tbody></table>\
         </body></html>"
    );
    with_page_headers(Html(body).into_response())
}

/// The form post from the page. A browser that holds the basic auth
/// credentials would send them on a cross site post too, so the request must
/// say it came from this page's own origin.
async fn page_upload(
    State(web): State<Web>,
    Extension(caller): Extension<Caller>,
    headers: HeaderMap,
    mut form: Multipart,
) -> Response {
    if !same_origin(&headers) {
        return (StatusCode::FORBIDDEN, "cross origin form posts are refused").into_response();
    }
    let mut file: Option<(String, Option<String>, Bytes)> = None;
    let mut reason = String::new();
    loop {
        let field = match form.next_field().await {
            Ok(Some(field)) => field,
            Ok(None) => break,
            Err(e) => return (StatusCode::BAD_REQUEST, format!("unreadable form: {}", e.body_text())).into_response(),
        };
        match field.name() {
            Some("file") => {
                let name = field.file_name().unwrap_or("upload").to_owned();
                let media_type = field.content_type().map(str::to_owned);
                match field.bytes().await {
                    Ok(bytes) => file = Some((name, media_type, bytes)),
                    Err(e) => {
                        return (StatusCode::BAD_REQUEST, format!("unreadable file: {}", e.body_text()))
                            .into_response();
                    }
                }
            }
            Some("reason") => reason = field.text().await.unwrap_or_default(),
            _ => {}
        }
    }
    let Some((name, media_type, bytes)) = file else {
        return (StatusCode::BAD_REQUEST, "file: is required").into_response();
    };
    let new = NewFile {
        name: &name,
        media_type: media_type.as_deref().filter(|t| *t != "application/octet-stream"),
        bytes: &bytes,
        links: &Links::default(),
    };
    match store_file(&web.db, &web.files, &caller.name, &reason, new).await {
        Ok(_) => Redirect::to("/").into_response(),
        Err(e) => error_response(e),
    }
}

fn same_origin(headers: &HeaderMap) -> bool {
    let host = headers.get(header::HOST).and_then(|v| v.to_str().ok());
    let origin = headers.get(header::ORIGIN).and_then(|v| v.to_str().ok());
    match (host, origin) {
        (Some(host), Some(origin)) => origin
            .split_once("://")
            .is_some_and(|(scheme, authority)| matches!(scheme, "http" | "https") && authority == host),
        _ => false,
    }
}

fn with_page_headers(mut response: Response) -> Response {
    let style_hash = STANDARD.encode(Sha256::digest(PAGE_STYLE.as_bytes()));
    let policy = format!(
        "default-src 'none'; style-src 'sha256-{style_hash}'; form-action 'self'; base-uri 'none'; frame-ancestors 'none'"
    );
    let headers = response.headers_mut();
    if let Ok(policy) = HeaderValue::from_str(&policy) {
        headers.insert(header::CONTENT_SECURITY_POLICY, policy);
    }
    headers.insert(header::X_CONTENT_TYPE_OPTIONS, HeaderValue::from_static("nosniff"));
    headers.insert(header::REFERRER_POLICY, HeaderValue::from_static("no-referrer"));
    response
}

fn error_response(error: ErrorData) -> Response {
    let status = if error.code == ErrorCode::INVALID_PARAMS {
        StatusCode::BAD_REQUEST
    } else {
        StatusCode::INTERNAL_SERVER_ERROR
    };
    (status, error.message.into_owned()).into_response()
}

/// RFC 6266: an ASCII fallback name, and the real name percent encoded.
fn content_disposition(name: &str) -> HeaderValue {
    let fallback: String = name
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || ".-_ ".contains(c) {
                c
            } else {
                '_'
            }
        })
        .collect();
    let mut encoded = String::new();
    for byte in name.bytes() {
        if byte.is_ascii_alphanumeric() || b"!#$&+-.^_`|~".contains(&byte) {
            encoded.push(char::from(byte));
        } else {
            encoded.push_str(&format!("%{byte:02X}"));
        }
    }
    HeaderValue::from_str(&format!(
        "attachment; filename=\"{fallback}\"; filename*=UTF-8''{encoded}"
    ))
    .unwrap_or_else(|_| HeaderValue::from_static("attachment"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn download_names_cannot_break_the_header() {
        let value = content_disposition("plan\"; x=\r\nä.dxf");
        let text = value.to_str().expect("ascii");
        assert_eq!(
            text,
            "attachment; filename=\"plan__ x____.dxf\"; filename*=UTF-8''plan%22%3B%20x%3D%0D%0A%C3%A4.dxf"
        );
    }

    #[test]
    fn only_same_origin_posts_pass() {
        let mut headers = HeaderMap::new();
        headers.insert(header::HOST, HeaderValue::from_static("house.pozza"));
        assert!(!same_origin(&headers));
        headers.insert(header::ORIGIN, HeaderValue::from_static("https://evil.example"));
        assert!(!same_origin(&headers));
        for other_scheme in ["ftp://house.pozza", "null"] {
            headers.insert(header::ORIGIN, HeaderValue::from_static(other_scheme));
            assert!(!same_origin(&headers), "{other_scheme}");
        }
        headers.insert(header::ORIGIN, HeaderValue::from_static("https://house.pozza"));
        assert!(same_origin(&headers));
    }
}
