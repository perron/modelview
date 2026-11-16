use serde::{Deserialize, Serialize};
use std::fmt;

/// A failure with a stable `code` agents can branch on.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct MvError {
    pub code: String,
    pub message: String,
}

impl MvError {
    pub fn new(code: impl Into<String>, message: impl Into<String>) -> Self {
        Self {
            code: code.into(),
            message: message.into(),
        }
    }

    pub fn file_not_found(path: impl fmt::Display) -> Self {
        Self::new("FILE_NOT_FOUND", format!("file not found: {path}"))
    }

    pub fn parse(message: impl Into<String>) -> Self {
        Self::new("PARSE_FAILED", message)
    }

    pub fn timeout(message: impl Into<String>) -> Self {
        Self::new("TIMEOUT", message)
    }

    pub fn io(err: impl fmt::Display) -> Self {
        Self::new("IO_ERROR", err.to_string())
    }

    pub fn render(message: impl Into<String>) -> Self {
        Self::new("RENDER_FAILED", message)
    }

    pub fn export(message: impl Into<String>) -> Self {
        Self::new("EXPORT_FAILED", message)
    }

    pub fn permission(message: impl Into<String>) -> Self {
        Self::new("PERMISSION_DENIED", message)
    }

    pub fn invalid(message: impl Into<String>) -> Self {
        Self::new("INVALID_ARGUMENT", message)
    }
}

impl fmt::Display for MvError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}: {}", self.code, self.message)
    }
}

impl std::error::Error for MvError {}

pub type Result<T> = std::result::Result<T, MvError>;

/// Process exit status. 0 is success. `validate --strict` uses 1 for warnings.
/// 2 is a bad file or a failed command. 3 means the file could not be read
/// (missing, timed out, or an I/O error).
pub fn exit_code(err: &MvError) -> i32 {
    match err.code.as_str() {
        "FILE_NOT_FOUND" | "TIMEOUT" | "IO_ERROR" | "PERMISSION_DENIED" => 3,
        _ => 2,
    }
}

pub fn error_json(err: &MvError) -> serde_json::Value {
    serde_json::json!({
        "schema": "modelview/error/v1",
        "ok": false,
        "code": err.code,
        "message": err.message,
    })
}
