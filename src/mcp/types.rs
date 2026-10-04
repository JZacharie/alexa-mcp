use serde::{Deserialize, Serialize};
use serde_json::Value;

/// A JSON-RPC 2.0 request received over MCP.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JsonRpcRequest {
    pub jsonrpc: String,
    pub id: Option<Value>,
    pub method: String,
    pub params: Option<Value>,
}

/// A JSON-RPC 2.0 response sent back over MCP.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JsonRpcResponse {
    pub jsonrpc: String,
    pub id: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub result: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<JsonRpcError>,
}

/// A JSON-RPC 2.0 error object.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JsonRpcError {
    pub code: i64,
    pub message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub data: Option<Value>,
}

/// An MCP tool declaration.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Tool {
    pub name: String,
    pub description: String,
    #[serde(rename = "inputSchema")]
    pub input_schema: Value,
}

/// A single block of MCP tool result content.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolContent {
    #[serde(rename = "type")]
    pub content_type: String,
    pub text: String,
}

/// The result of an MCP tools/call invocation.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CallToolResult {
    pub content: Vec<ToolContent>,
    #[serde(rename = "isError", skip_serializing_if = "Option::is_none")]
    pub is_error: Option<bool>,
}

impl CallToolResult {
    /// A successful plain-text result.
    pub fn text(message: impl Into<String>) -> Self {
        Self {
            content: vec![ToolContent {
                content_type: "text".to_string(),
                text: message.into(),
            }],
            is_error: None,
        }
    }

    /// A successful result carrying a pretty-printed JSON payload.
    pub fn json<T: Serialize>(value: &T) -> Self {
        let text = serde_json::to_string_pretty(value)
            .unwrap_or_else(|error| format!("Failed to serialize the result as JSON: {error}"));
        Self::text(text)
    }

    /// An error result.
    pub fn error(message: impl Into<String>) -> Self {
        Self {
            content: vec![ToolContent {
                content_type: "text".to_string(),
                text: message.into(),
            }],
            is_error: Some(true),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn text_result_is_not_an_error() {
        let result = CallToolResult::text("hello");
        assert_eq!(result.content[0].text, "hello");
        assert!(result.is_error.is_none());
    }

    #[test]
    fn json_result_serializes_the_payload() {
        let result = CallToolResult::json(&json!({ "ok": true }));
        assert!(result.content[0].text.contains("\"ok\""));
        assert!(result.is_error.is_none());
    }

    #[test]
    fn error_result_is_flagged() {
        let result = CallToolResult::error("boom");
        assert_eq!(result.is_error, Some(true));
        assert_eq!(result.content[0].text, "boom");
    }
}
