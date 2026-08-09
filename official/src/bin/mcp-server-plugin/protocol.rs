use crate::{api::PluginContext, registry, tools};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

pub(crate) const SUPPORTED_PROTOCOL_VERSIONS: &[&str] =
    &["2025-11-25", "2025-06-18", "2025-03-26", "2024-11-05"];
const SERVER_NAME: &str = "mediary-mcp-server";
pub(crate) const SERVER_VERSION: &str = "0.3.0";

#[derive(Debug, Deserialize)]
struct JsonRpcRequest {
    jsonrpc: String,
    #[serde(default)]
    id: Option<Value>,
    method: String,
    #[serde(default = "empty_object")]
    params: Value,
}

#[derive(Debug, Serialize)]
pub(crate) struct JsonRpcResponse {
    jsonrpc: &'static str,
    id: Value,
    #[serde(skip_serializing_if = "Option::is_none")]
    result: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<JsonRpcErrorBody>,
}

#[derive(Debug, Serialize)]
struct JsonRpcErrorBody {
    code: i64,
    message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    data: Option<Value>,
}

impl JsonRpcResponse {
    fn ok(id: Value, result: Value) -> Self {
        Self {
            jsonrpc: "2.0",
            id,
            result: Some(result),
            error: None,
        }
    }

    fn error(id: Value, code: i64, message: impl Into<String>) -> Self {
        Self {
            jsonrpc: "2.0",
            id,
            result: None,
            error: Some(JsonRpcErrorBody {
                code,
                message: message.into(),
                data: None,
            }),
        }
    }
}

#[derive(Debug, Deserialize)]
struct InitializeParams {
    #[serde(rename = "protocolVersion")]
    protocol_version: String,
    capabilities: Value,
    #[serde(rename = "clientInfo")]
    client_info: ClientInfo,
}

#[derive(Debug, Deserialize)]
struct ClientInfo {
    name: String,
    version: String,
}

#[derive(Debug, Serialize)]
struct InitializeResult {
    #[serde(rename = "protocolVersion")]
    protocol_version: String,
    capabilities: Value,
    #[serde(rename = "serverInfo")]
    server_info: ServerInfo,
    instructions: &'static str,
}

#[derive(Debug, Serialize)]
struct ServerInfo {
    name: &'static str,
    version: &'static str,
}

#[derive(Debug, Deserialize)]
struct CallToolParams {
    name: String,
    #[serde(default = "empty_object")]
    arguments: Value,
}

#[derive(Debug, Serialize)]
struct CallToolResult {
    content: Vec<ToolContent>,
    #[serde(rename = "isError", skip_serializing_if = "Option::is_none")]
    is_error: Option<bool>,
}

#[derive(Debug, Serialize)]
struct ToolContent {
    #[serde(rename = "type")]
    content_type: &'static str,
    text: String,
}

pub(crate) async fn handle_input(ctx: &PluginContext, input: &str) -> Option<JsonRpcResponse> {
    let request = match parse_request(input) {
        Ok(request) => request,
        Err(response) => return Some(*response),
    };

    let Some(id) = request.id.clone() else {
        handle_notification(&request);
        return None;
    };
    Some(dispatch_request(ctx, request, id).await)
}

fn parse_request(input: &str) -> Result<JsonRpcRequest, Box<JsonRpcResponse>> {
    let value: Value = serde_json::from_str(input).map_err(|error| {
        Box::new(JsonRpcResponse::error(
            Value::Null,
            -32700,
            format!("JSON 解析失败: {error}"),
        ))
    })?;
    let raw_id = value.as_object().and_then(|object| object.get("id"));
    if raw_id.is_some_and(|id| !id.is_string() && !id.is_number()) {
        return Err(Box::new(JsonRpcResponse::error(
            Value::Null,
            -32600,
            "请求 id 必须是字符串或数字",
        )));
    }
    let id = raw_id.cloned().unwrap_or(Value::Null);
    let request: JsonRpcRequest = serde_json::from_value(value).map_err(|error| {
        Box::new(JsonRpcResponse::error(
            id.clone(),
            -32600,
            format!("无效请求: {error}"),
        ))
    })?;
    if request.jsonrpc != "2.0" || request.method.trim().is_empty() {
        return Err(Box::new(JsonRpcResponse::error(
            id,
            -32600,
            "无效的 JSON-RPC 2.0 请求",
        )));
    }
    if !request.params.is_object() && !request.params.is_array() && !request.params.is_null() {
        return Err(Box::new(JsonRpcResponse::error(
            id,
            -32602,
            "params 必须是对象或数组",
        )));
    }
    Ok(request)
}

fn handle_notification(request: &JsonRpcRequest) {
    match request.method.as_str() {
        "notifications/initialized" | "notifications/cancelled" => {}
        method => eprintln!("忽略不支持的 MCP 通知: {method}"),
    }
}

async fn dispatch_request(
    ctx: &PluginContext,
    request: JsonRpcRequest,
    id: Value,
) -> JsonRpcResponse {
    match request.method.as_str() {
        "initialize" => initialize(id, request.params),
        "tools/list" => JsonRpcResponse::ok(id, json!({"tools": registry::define_tools()})),
        "tools/call" => call_tool(ctx, id, request.params).await,
        "ping" => JsonRpcResponse::ok(id, json!({})),
        _ => JsonRpcResponse::error(id, -32601, format!("不支持的方法: {}", request.method)),
    }
}

fn initialize(id: Value, params: Value) -> JsonRpcResponse {
    let params: InitializeParams = match serde_json::from_value(params) {
        Ok(params) => params,
        Err(error) => {
            return JsonRpcResponse::error(id, -32602, format!("初始化参数无效: {error}"));
        }
    };
    if params.client_info.name.trim().is_empty()
        || params.client_info.version.trim().is_empty()
        || !params.capabilities.is_object()
    {
        return JsonRpcResponse::error(id, -32602, "初始化参数缺少有效的客户端信息或 capabilities");
    }
    let protocol_version =
        if SUPPORTED_PROTOCOL_VERSIONS.contains(&params.protocol_version.as_str()) {
            params.protocol_version
        } else {
            SUPPORTED_PROTOCOL_VERSIONS[0].to_string()
        };
    let result = InitializeResult {
        protocol_version,
        capabilities: json!({"tools": {"listChanged": false}}),
        server_info: ServerInfo {
            name: SERVER_NAME,
            version: SERVER_VERSION,
        },
        instructions: "通过 MCP 安全访问 Mediary 的目录、订阅、下载器、过滤规则、日志和通知功能。执行删除操作前应先向用户确认。",
    };
    JsonRpcResponse::ok(id, serde_json::to_value(result).unwrap_or_default())
}

async fn call_tool(ctx: &PluginContext, id: Value, params: Value) -> JsonRpcResponse {
    let params: CallToolParams = match serde_json::from_value(params) {
        Ok(params) => params,
        Err(error) => return JsonRpcResponse::error(id, -32602, format!("工具参数无效: {error}")),
    };
    if !params.arguments.is_object() {
        return JsonRpcResponse::error(id, -32602, "arguments 必须是对象");
    }
    if !registry::define_tools()
        .iter()
        .any(|tool| tool.name == params.name)
    {
        return JsonRpcResponse::error(id, -32602, format!("未知工具: {}", params.name));
    }
    let result = tool_result(tools::call_tool(ctx, &params.name, &params.arguments).await);
    JsonRpcResponse::ok(id, serde_json::to_value(result).unwrap_or_default())
}

fn tool_result(result: Result<Value, String>) -> CallToolResult {
    match result {
        Ok(value) => CallToolResult {
            content: vec![ToolContent {
                content_type: "text",
                text: serde_json::to_string_pretty(&value)
                    .unwrap_or_else(|error| error.to_string()),
            }],
            is_error: None,
        },
        Err(error) => CallToolResult {
            content: vec![ToolContent {
                content_type: "text",
                text: error,
            }],
            is_error: Some(true),
        },
    }
}

fn empty_object() -> Value {
    json!({})
}

#[cfg(test)]
mod tests {
    use super::*;

    fn context() -> PluginContext {
        PluginContext::new("http://127.0.0.1:1/api".into(), "test".into()).unwrap()
    }

    fn response_value(response: JsonRpcResponse) -> Value {
        serde_json::to_value(response).unwrap()
    }

    #[tokio::test]
    async fn negotiates_supported_protocol_version() {
        let response = handle_input(
            &context(),
            r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-06-18","capabilities":{},"clientInfo":{"name":"test","version":"1"}}}"#,
        )
        .await
        .unwrap();
        assert_eq!(
            response_value(response)["result"]["protocolVersion"],
            "2025-06-18"
        );
    }

    #[tokio::test]
    async fn falls_back_to_latest_supported_protocol_version() {
        let response = handle_input(
            &context(),
            r#"{"jsonrpc":"2.0","id":"init","method":"initialize","params":{"protocolVersion":"2099-01-01","capabilities":{},"clientInfo":{"name":"test","version":"1"}}}"#,
        )
        .await
        .unwrap();
        assert_eq!(
            response_value(response)["result"]["protocolVersion"],
            SUPPORTED_PROTOCOL_VERSIONS[0]
        );
    }

    #[tokio::test]
    async fn returns_json_rpc_errors_instead_of_failing_process() {
        let parse_error = handle_input(&context(), "{").await.unwrap();
        assert_eq!(response_value(parse_error)["error"]["code"], -32700);

        let invalid = handle_input(&context(), r#"{"jsonrpc":"1.0","id":7,"method":"ping"}"#)
            .await
            .unwrap();
        assert_eq!(response_value(invalid)["error"]["code"], -32600);
    }

    #[tokio::test]
    async fn notifications_produce_no_output_and_do_not_call_tools() {
        let initialized = handle_input(
            &context(),
            r#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#,
        )
        .await;
        assert!(initialized.is_none());

        let tool_notification = handle_input(
            &context(),
            r#"{"jsonrpc":"2.0","method":"tools/call","params":{"name":"subscriptions_delete","arguments":{"id":1}}}"#,
        )
        .await;
        assert!(tool_notification.is_none());
    }

    #[tokio::test]
    async fn rejects_null_ids_and_unknown_tools() {
        let null_id = handle_input(&context(), r#"{"jsonrpc":"2.0","id":null,"method":"ping"}"#)
            .await
            .unwrap();
        assert_eq!(response_value(null_id)["error"]["code"], -32600);

        let unknown = handle_input(
            &context(),
            r#"{"jsonrpc":"2.0","id":9,"method":"tools/call","params":{"name":"missing","arguments":{}}}"#,
        )
        .await
        .unwrap();
        assert_eq!(response_value(unknown)["error"]["code"], -32602);
    }
}
