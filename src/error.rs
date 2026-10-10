//! Turning failures into MCP errors a caller can act on.

use rmcp::ErrorData;
use serde_json::json;

/// A caller mistake, naming the field at fault.
pub fn invalid(field: &str, message: impl std::fmt::Display) -> ErrorData {
    ErrorData::invalid_params(format!("{field}: {message}"), Some(json!({ "field": field })))
}

/// A failure that is not the caller's. The detail goes to the log only.
pub fn internal(context: &str, detail: impl std::fmt::Display) -> ErrorData {
    tracing::error!("{context}: {detail}");
    ErrorData::internal_error(format!("{context} failed"), None)
}

/// Database errors that a caller's input caused become invalid params naming
/// the constraint; anything else is internal.
pub fn db(error: sqlx::Error) -> ErrorData {
    if let sqlx::Error::Database(e) = &error {
        let constraint = e.constraint().unwrap_or("input").to_owned();
        match e.code().as_deref() {
            Some("23503") => return invalid(&constraint, "refers to a row that does not exist"),
            Some("23505") => return invalid(&constraint, "a live row with this value already exists"),
            Some("23514") => return invalid(&constraint, "value is not allowed"),
            // Raised by the frame functions with a message meant for the caller.
            Some("22023") => return invalid("site", e.message()),
            _ => {}
        }
    }
    internal("database query", error)
}
