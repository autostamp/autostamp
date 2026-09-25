use std::fmt;

/// An invalid catalog, unsupported schema, or invalid generated package.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Error {
    /// Original MCP tool name, when the error belongs to a tool.
    pub tool: Option<String>,
    /// RFC 6901 JSON Pointer into the complete catalog; empty means its root.
    pub pointer: String,
    /// Human-readable explanation of the unsupported or malformed input.
    pub message: String,
}

impl Error {
    pub(crate) fn new(tool: Option<&str>, pointer: &str, message: impl Into<String>) -> Self {
        Self {
            tool: tool.map(str::to_owned),
            pointer: pointer.to_owned(),
            message: message.into(),
        }
    }
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if let Some(tool) = &self.tool {
            write!(f, "tool {tool:?} ")?;
        }
        let path = if self.pointer.is_empty() {
            "<root>"
        } else {
            &self.pointer
        };
        write!(f, "at {path}: {}", self.message)
    }
}

impl std::error::Error for Error {}
