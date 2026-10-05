//! Wire types for the JSON-lines protocol.

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

#[derive(Debug, Deserialize)]
pub struct Request {
    #[serde(default)]
    pub id: Value,
    pub method: String,
    #[serde(default)]
    pub params: Value,
}

#[derive(Debug, Clone, Serialize)]
pub struct RpcError {
    pub message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub code: Option<String>,
    /// For `applyChanges`: index of the change that failed.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub index: Option<usize>,
}

impl RpcError {
    pub fn new(code: &str, message: impl Into<String>) -> Self {
        Self { message: message.into(), code: Some(code.to_string()), index: None }
    }
    pub fn cancelled() -> Self {
        Self::new("cancelled", "Query cancelled")
    }
}

impl From<anyhow::Error> for RpcError {
    fn from(e: anyhow::Error) -> Self {
        Self { message: error_message(&e), code: error_code(&e), index: None }
    }
}

/// Human readable message for an error chain; unwraps driver errors so the user sees the
/// database's own message instead of "error returned from database: ...".
pub fn error_message(e: &anyhow::Error) -> String {
    let mut parts: Vec<String> = Vec::new();
    for cause in e.chain() {
        if let Some(sqlx::Error::Database(db)) = cause.downcast_ref::<sqlx::Error>() {
            parts.push(db.message().to_string());
            break;
        }
        if let Some(tiberius::error::Error::Server(t)) = cause.downcast_ref::<tiberius::error::Error>() {
            parts.push(t.message().to_string());
            break;
        }
        let s = cause.to_string();
        if parts.last().is_some_and(|p| p.contains(&s)) {
            continue;
        }
        parts.push(s);
    }
    parts.join(": ")
}

/// Database-specific error code (SQLSTATE for sqlx engines, error number for SQL Server).
pub fn error_code(e: &anyhow::Error) -> Option<String> {
    e.chain().find_map(|cause| {
        if let Some(sqlx::Error::Database(db)) = cause.downcast_ref::<sqlx::Error>() {
            return db.code().map(|c| c.to_string());
        }
        if let Some(tiberius::error::Error::Server(t)) = cause.downcast_ref::<tiberius::error::Error>() {
            return Some(t.code().to_string());
        }
        None
    })
}

pub fn response(id: Value, result: Result<Value, RpcError>) -> String {
    let v = match result {
        Ok(result) => json!({ "id": id, "result": result }),
        Err(error) => json!({ "id": id, "error": error }),
    };
    v.to_string()
}
