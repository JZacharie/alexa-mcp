pub mod types;

use crate::alexa::{AlexaClient, ItemStatus};
use serde_json::{json, Value};
use std::sync::Arc;
use tracing::{info, warn};
use types::*;

/// MCP request handler exposing the Alexa shopping list toolset.
#[derive(Clone)]
pub struct McpHandler {
    alexa_client: Arc<AlexaClient>,
}

impl McpHandler {
    pub fn new(alexa_client: Arc<AlexaClient>) -> Self {
        Self { alexa_client }
    }

    /// Declares the tools exposed over MCP.
    pub fn get_tools(&self) -> Vec<Tool> {
        vec![
            Tool {
                name: "alexa_get_shopping_list".to_string(),
                description: "Reads the Amazon Alexa shopping list. Returns every item with its id, name and completion state, plus active/completed counters.".to_string(),
                input_schema: json!({
                    "type": "object",
                    "properties": {
                        "status": {
                            "type": "string",
                            "enum": ["all", "active", "completed"],
                            "description": "Which items to return (default: all)"
                        }
                    },
                    "required": []
                }),
            },
            Tool {
                name: "alexa_add_item".to_string(),
                description: "Adds one or more items to the Alexa shopping list.".to_string(),
                input_schema: json!({
                    "type": "object",
                    "properties": {
                        "item": { "type": "string", "description": "A single item name to add" },
                        "items": {
                            "type": "array",
                            "items": { "type": "string" },
                            "description": "Several item names to add at once"
                        }
                    },
                    "required": []
                }),
            },
            Tool {
                name: "alexa_complete_item".to_string(),
                description: "Marks an Alexa shopping list item as completed, or reopens it.".to_string(),
                input_schema: json!({
                    "type": "object",
                    "properties": {
                        "item": { "type": "string", "description": "Item id or exact name" },
                        "completed": { "type": "boolean", "description": "true to complete (default), false to reopen" }
                    },
                    "required": ["item"]
                }),
            },
            Tool {
                name: "alexa_delete_item".to_string(),
                description: "Deletes an item from the Alexa shopping list.".to_string(),
                input_schema: json!({
                    "type": "object",
                    "properties": {
                        "item": { "type": "string", "description": "Item id or exact name" }
                    },
                    "required": ["item"]
                }),
            },
            Tool {
                name: "alexa_get_cookies".to_string(),
                description: "Reads the live Amazon cookies from the browser session and returns them in the browser-extension export format. Optionally persists them (default) for ALEXA_COOKIES_JSON / config/alexa-cookies.json.".to_string(),
                input_schema: json!({
                    "type": "object",
                    "properties": {
                        "save": { "type": "boolean", "description": "Persist the cookies to the configured file (default: true)" }
                    },
                    "required": []
                }),
            },
        ]
    }

    pub async fn handle_request(&self, req: JsonRpcRequest) -> JsonRpcResponse {
        let id = req.id.clone();
        match req.method.as_str() {
            "initialize" => JsonRpcResponse {
                jsonrpc: "2.0".to_string(),
                id,
                result: Some(json!({
                    "protocolVersion": "2024-11-05",
                    "capabilities": { "tools": { "listChanged": false } },
                    "serverInfo": {
                        "name": "alexa-mcp",
                        "version": env!("CARGO_PKG_VERSION")
                    }
                })),
                error: None,
            },
            "notifications/initialized" | "ping" => JsonRpcResponse {
                jsonrpc: "2.0".to_string(),
                id,
                result: Some(json!({})),
                error: None,
            },
            method if method.starts_with("notifications/") => {
                info!("Ignoring MCP notification: {method}");
                JsonRpcResponse {
                    jsonrpc: "2.0".to_string(),
                    id,
                    result: Some(json!({})),
                    error: None,
                }
            }
            "tools/list" => JsonRpcResponse {
                jsonrpc: "2.0".to_string(),
                id,
                result: Some(json!({ "tools": self.get_tools() })),
                error: None,
            },
            "tools/call" => {
                let call_result = self.execute_tool_call(req.params).await;
                JsonRpcResponse {
                    jsonrpc: "2.0".to_string(),
                    id,
                    result: Some(serde_json::to_value(call_result).unwrap_or_else(|_| json!({}))),
                    error: None,
                }
            }
            unknown => {
                warn!("Unknown MCP method requested: {unknown}");
                JsonRpcResponse {
                    jsonrpc: "2.0".to_string(),
                    id,
                    result: None,
                    error: Some(JsonRpcError {
                        code: -32601,
                        message: format!("Method not found: {unknown}"),
                        data: None,
                    }),
                }
            }
        }
    }

    async fn execute_tool_call(&self, params: Option<Value>) -> CallToolResult {
        let Some(params) = params else {
            return CallToolResult::error("Missing params in tools/call");
        };
        let Some(name) = params.get("name").and_then(Value::as_str) else {
            return CallToolResult::error("Missing tool name in params");
        };
        let arguments = params
            .get("arguments")
            .cloned()
            .unwrap_or_else(|| json!({}));
        info!("Executing tool call '{name}' with args: {arguments}");

        match name {
            "alexa_get_shopping_list" => {
                let status = arguments
                    .get("status")
                    .and_then(Value::as_str)
                    .map(ItemStatus::parse)
                    .unwrap_or(ItemStatus::All);
                match self.alexa_client.get_shopping_list().await {
                    Ok(list) => CallToolResult::json(&json!({
                        "list_id": list.list_id,
                        "status": status,
                        "total_count": list.total_count,
                        "active_count": list.active_count,
                        "completed_count": list.completed_count,
                        "items": list.filtered(status)
                    })),
                    Err(error) => CallToolResult::error(format!(
                        "Failed to read the Alexa shopping list: {error:?}"
                    )),
                }
            }
            "alexa_add_item" => match collect_items(&arguments) {
                Ok(items) if !items.is_empty() => {
                    let mut results = Vec::new();
                    let mut failures = 0;
                    for item in &items {
                        match self.alexa_client.add_item(item).await {
                            Ok(()) => results.push(json!({ "item": item, "success": true })),
                            Err(error) => {
                                failures += 1;
                                results.push(json!({ "item": item, "success": false, "error": format!("{error:?}") }));
                            }
                        }
                    }
                    let result = json!({
                        "success": failures == 0,
                        "added": items.len() - failures,
                        "failed": failures,
                        "details": results
                    });
                    if failures == 0 {
                        CallToolResult::json(&result)
                    } else {
                        let mut text = CallToolResult::json(&result);
                        text.is_error = Some(true);
                        text
                    }
                }
                Ok(_) => CallToolResult::error("Missing required parameter 'item' or 'items'"),
                Err(message) => CallToolResult::error(message),
            },
            "alexa_complete_item" => {
                let Some(item) = arguments.get("item").and_then(Value::as_str) else {
                    return CallToolResult::error("Missing required parameter 'item'");
                };
                let completed = arguments
                    .get("completed")
                    .and_then(Value::as_bool)
                    .unwrap_or(true);
                match self.alexa_client.complete_item(item, completed).await {
                    Ok(updated) => CallToolResult::json(&json!({
                        "message": format!("{} '{}'", if completed { "Completed" } else { "Reopened" }, updated.value),
                        "item": updated
                    })),
                    Err(error) => {
                        CallToolResult::error(format!("Failed to update the item: {error:?}"))
                    }
                }
            }
            "alexa_delete_item" => {
                let Some(item) = arguments.get("item").and_then(Value::as_str) else {
                    return CallToolResult::error("Missing required parameter 'item'");
                };
                match self.alexa_client.delete_item(item).await {
                    Ok(deleted) => CallToolResult::json(&json!({
                        "message": format!("Deleted '{}'", deleted.value),
                        "item": deleted
                    })),
                    Err(error) => {
                        CallToolResult::error(format!("Failed to delete the item: {error:?}"))
                    }
                }
            }
            "alexa_get_cookies" => {
                let save = arguments
                    .get("save")
                    .and_then(Value::as_bool)
                    .unwrap_or(true);
                match self.alexa_client.export_cookies(save).await {
                    Ok(export) => CallToolResult::json(&export),
                    Err(error) => CallToolResult::error(format!("Cookie export failed: {error:?}")),
                }
            }
            other => CallToolResult::error(format!("Tool '{other}' is not supported")),
        }
    }
}

/// Reads a single 'item' string or an 'items' array of strings.
fn collect_items(arguments: &Value) -> Result<Vec<String>, String> {
    let mut items = Vec::new();
    if let Some(item) = arguments.get("item").and_then(Value::as_str) {
        if !item.trim().is_empty() {
            items.push(item.trim().to_string());
        }
    }
    if let Some(array) = arguments.get("items").and_then(Value::as_array) {
        for entry in array {
            let Some(text) = entry.as_str() else {
                return Err("Parameter 'items' must only contain strings".to_string());
            };
            if !text.trim().is_empty() {
                items.push(text.trim().to_string());
            }
        }
    }
    Ok(items)
}
