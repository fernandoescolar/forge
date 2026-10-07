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
        if let Some(m) = cause.downcast_ref::<mongodb::error::Error>() {
            parts.push(mongo_message(m));
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

/// The MongoDB driver's errors, without its debugging details.
fn mongo_message(e: &mongodb::error::Error) -> String {
    use mongodb::error::{ErrorKind, WriteFailure};
    match e.kind.as_ref() {
        ErrorKind::Authentication { .. } => "Authentication failed: check the user and password (and which database they belong to)".into(),
        ErrorKind::Command(c) => c.message.clone(),
        ErrorKind::Write(WriteFailure::WriteError(w)) => w.message.clone(),
        ErrorKind::Write(WriteFailure::WriteConcernError(w)) => w.message.clone(),
        ErrorKind::ServerSelection { message, .. } => {
            let first = message.split(". Topology").next().unwrap_or(message);
            format!("Could not reach the server: {first}")
        }
        ErrorKind::InvalidArgument { message, .. } => message.clone(),
        other => other.to_string(),
    }
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
        if let Some(mongodb::error::ErrorKind::Command(c)) = cause.downcast_ref::<mongodb::error::Error>().map(|m| m.kind.as_ref()) {
            return Some(c.code.to_string());
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
