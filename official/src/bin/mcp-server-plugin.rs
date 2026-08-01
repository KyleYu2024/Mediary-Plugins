use axum::{
    extract::{Query, State},
    http::{header, HeaderMap, StatusCode},
    response::{sse::Event, Json, Sse},
    routing::post,
    Router,
};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use tokio::net::TcpListener;
use tokio::sync::broadcast;
use tokio_stream::wrappers::BroadcastStream;
use tokio_stream::StreamExt;

// ── JSON-RPC 2.0 Types ──────────────────────────────────────────

#[derive(Debug, Deserialize)]
#[allow(dead_code)]
struct JsonRpcRequest {
    jsonrpc: String,
    #[serde(default)]
    id: Value,
    method: String,
    #[serde(default)]
    params: Value,
}

#[derive(Debug, Serialize)]
struct JsonRpcResponse {
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
        Self { jsonrpc: "2.0", id, result: Some(result), error: None }
    }
    fn err(id: Value, code: i64, message: impl Into<String>) -> Self {
        Self {
            jsonrpc: "2.0",
            id,
            result: None,
            error: Some(JsonRpcErrorBody { code, message: message.into(), data: None }),
        }
    }
}

// ── MCP Protocol Types ──────────────────────────────────────────

#[derive(Debug, Deserialize)]
#[allow(dead_code)]
struct ClientInfo {
    name: String,
    version: String,
}

#[derive(Debug, Deserialize)]
#[allow(dead_code)]
struct InitializeParams {
    #[serde(rename = "protocolVersion")]
    protocol_version: String,
    capabilities: Value,
    #[serde(rename = "clientInfo")]
    client_info: ClientInfo,
}

#[derive(Debug, Serialize)]
struct InitializeResult {
    #[serde(rename = "protocolVersion")]
    protocol_version: &'static str,
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

#[derive(Debug, Serialize)]
struct ToolDefinition {
    name: String,
    description: String,
    #[serde(rename = "inputSchema")]
    input_schema: Value,
}

#[derive(Debug, Deserialize)]
struct CallToolParams {
    name: String,
    #[serde(default)]
    arguments: Value,
}

#[derive(Debug, Serialize)]
struct CallToolResult {
    content: Vec<ToolContent>,
    #[serde(skip_serializing_if = "Option::is_none")]
    is_error: Option<bool>,
}

#[derive(Debug, Serialize)]
struct ToolContent {
    #[serde(rename = "type")]
    content_type: &'static str,
    text: String,
}

// ── Tool Registry ───────────────────────────────────────────────

fn define_tools() -> Vec<ToolDefinition> {
    vec![
        ToolDefinition {
            name: "catalog_search".into(),
            description: "搜索 Mediary 媒体目录，根据关键词查找电影、电视剧等媒体资源。".into(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "query": {
                        "type": "string",
                        "description": "搜索关键词"
                    },
                    "limit": {
                        "type": "integer",
                        "description": "返回结果数量上限，默认 10",
                        "default": 10
                    }
                },
                "required": ["query"]
            }),
        },
        ToolDefinition {
            name: "subscriptions_list".into(),
            description: "列出 Mediary 中当前的订阅列表，获取已订阅的媒体信息。".into(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "status": {
                        "type": "string",
                        "description": "按状态筛选：all / active / completed / archived",
                        "enum": ["all", "active", "completed", "archived"],
                        "default": "all"
                    },
                    "limit": {
                        "type": "integer",
                        "description": "返回数量上限，默认 20",
                        "default": 20
                    }
                }
            }),
        },
        ToolDefinition {
            name: "downloads_list".into(),
            description: "列出当前下载任务，查看下载进度和状态。".into(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "status": {
                        "type": "string",
                        "description": "按状态筛选：all / downloading / seeding / paused / completed / error",
                        "enum": ["all", "downloading", "seeding", "paused", "completed", "error"],
                        "default": "all"
                    },
                    "limit": {
                        "type": "integer",
                        "description": "返回数量上限，默认 20",
                        "default": 20
                    }
                }
            }),
        },
        ToolDefinition {
            name: "downloads_create".into(),
            description: "创建新的下载任务，提交磁力链接或种子链接到下载器。".into(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "url": {
                        "type": "string",
                        "description": "磁力链接 (magnet:) 或种子下载链接"
                    },
                    "save_path": {
                        "type": "string",
                        "description": "保存路径（选填）"
                    },
                    "category": {
                        "type": "string",
                        "description": "下载器分类标签（选填）"
                    }
                },
                "required": ["url"]
            }),
        },
        ToolDefinition {
            name: "subscriptions_create".into(),
            description: "在 Mediary 中创建新的媒体订阅，支持电影和电视剧。".into(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "title": {
                        "type": "string",
                        "description": "媒体标题名称"
                    },
                    "media_type": {
                        "type": "string",
                        "description": "媒体类型：movie 或 tv",
                        "enum": ["movie", "tv"],
                        "default": "movie"
                    },
                    "tmdb_id": {
                        "type": "integer",
                        "description": "TMDB 媒体 ID（选填）"
                    },
                    "year": {
                        "type": "integer",
                        "description": "发行年份（选填）"
                    }
                },
                "required": ["title"]
            }),
        },
        ToolDefinition {
            name: "subscriptions_delete".into(),
            description: "删除 Mediary 中指定的订阅记录。".into(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "id": {
                        "type": "integer",
                        "description": "要删除的订阅 ID"
                    }
                },
                "required": ["id"]
            }),
        },
        ToolDefinition {
            name: "sites_list".into(),
            description: "列出 Mediary 中已配置的 PT 站点信息。".into(),
            input_schema: json!({
                "type": "object",
                "properties": {}
            }),
        },
        ToolDefinition {
            name: "downloader_status".into(),
            description: "获取下载器运行状态，包括连接状态、上传/下载速度、剩余空间等。".into(),
            input_schema: json!({
                "type": "object",
                "properties": {}
            }),
        },
        ToolDefinition {
            name: "torrents_list".into(),
            description: "列出下载器中的种子列表及详细信息。".into(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "limit": {
                        "type": "integer",
                        "description": "返回数量上限，默认 20",
                        "default": 20
                    }
                }
            }),
        },
        ToolDefinition {
            name: "downloads_delete".into(),
            description: "删除指定的下载任务（包括种子和数据文件）。".into(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "id": {
                        "type": "integer",
                        "description": "要删除的下载任务 ID"
                    },
                    "delete_files": {
                        "type": "boolean",
                        "description": "是否同时删除已下载的文件，默认 true",
                        "default": true
                    }
                },
                "required": ["id"]
            }),
        },
        ToolDefinition {
            name: "downloads_pause".into(),
            description: "暂停指定的下载任务。".into(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "id": {
                        "type": "integer",
                        "description": "要暂停的下载任务 ID"
                    }
                },
                "required": ["id"]
            }),
        },
        ToolDefinition {
            name: "downloads_resume".into(),
            description: "恢复已暂停的下载任务。".into(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "id": {
                        "type": "integer",
                        "description": "要恢复的下载任务 ID"
                    }
                },
                "required": ["id"]
            }),
        },
        ToolDefinition {
            name: "system_logs".into(),
            description: "查看 Mediary 系统运行日志，用于排查错误和了解系统状态。".into(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "limit": {
                        "type": "integer",
                        "description": "返回日志条数上限，默认 50",
                        "default": 50
                    },
                    "level": {
                        "type": "string",
                        "description": "按日志级别筛选：info / warning / error / debug",
                        "enum": ["info", "warning", "error", "debug"]
                    }
                }
            }),
        },
        ToolDefinition {
            name: "filters_list".into(),
            description: "列出 Mediary 中已配置的过滤规则。".into(),
            input_schema: json!({
                "type": "object",
                "properties": {}
            }),
        },
        ToolDefinition {
            name: "filters_create".into(),
            description: "创建新的过滤规则。可设置名称、匹配模式、关联站点等。".into(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "name": {
                        "type": "string",
                        "description": "过滤规则名称"
                    },
                    "pattern": {
                        "type": "string",
                        "description": "匹配模式（支持正则表达式）"
                    },
                    "filter_type": {
                        "type": "string",
                        "description": "过滤类型：include（包含）或 exclude（排除）",
                        "enum": ["include", "exclude"],
                        "default": "include"
                    },
                    "site_id": {
                        "type": "integer",
                        "description": "关联站点 ID，不填则全局生效"
                    },
                    "enabled": {
                        "type": "boolean",
                        "description": "是否启用，默认 true",
                        "default": true
                    }
                },
                "required": ["name", "pattern"]
            }),
        },
        ToolDefinition {
            name: "filters_update".into(),
            description: "更新已有的过滤规则，可修改名称、模式、类型、站点、启用状态等。".into(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "id": {
                        "type": "integer",
                        "description": "过滤规则 ID"
                    },
                    "name": {
                        "type": "string",
                        "description": "新的规则名称"
                    },
                    "pattern": {
                        "type": "string",
                        "description": "新的匹配模式"
                    },
                    "filter_type": {
                        "type": "string",
                        "description": "过滤类型",
                        "enum": ["include", "exclude"]
                    },
                    "site_id": {
                        "type": "integer",
                        "description": "关联站点 ID"
                    },
                    "enabled": {
                        "type": "boolean",
                        "description": "是否启用"
                    }
                },
                "required": ["id"]
            }),
        },
        ToolDefinition {
            name: "filters_delete".into(),
            description: "删除指定的过滤规则。".into(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "id": {
                        "type": "integer",
                        "description": "要删除的过滤规则 ID"
                    }
                },
                "required": ["id"]
            }),
        },
        ToolDefinition {
            name: "send_notification".into(),
            description: "通过 Mediary 发送系统通知消息。".into(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "title": {
                        "type": "string",
                        "description": "通知标题"
                    },
                    "message": {
                        "type": "string",
                        "description": "通知正文内容"
                    },
                    "level": {
                        "type": "string",
                        "description": "通知级别",
                        "enum": ["info", "success", "warning", "error"],
                        "default": "info"
                    }
                },
                "required": ["title", "message"]
            }),
        },
    ]
}

// ── App State ───────────────────────────────────────────────────

struct AppState {
    api_url: String,
    api_token: String,
    http_client: reqwest::Client,
    sse_tx: broadcast::Sender<String>,
    request_counter: AtomicU64,
}

// ── Mediary API Helpers ─────────────────────────────────────────

async fn api_get(state: &AppState, path: &str) -> Result<Value, String> {
    let url = format!("{}{}", state.api_url, path);
    let resp = state
        .http_client
        .get(&url)
        .bearer_auth(&state.api_token)
        .send()
        .await
        .map_err(|e| format!("HTTP 请求失败: {e}"))?;
    let status = resp.status();
    let body: Value = resp.json().await.map_err(|e| format!("解析响应失败: {e}"))?;
    if status.is_success() {
        Ok(body)
    } else {
        let msg = body.get("message").and_then(Value::as_str).unwrap_or("未知错误");
        Err(format!("API 错误 ({}): {msg}", status.as_u16()))
    }
}

async fn api_post(state: &AppState, path: &str, payload: Value) -> Result<Value, String> {
    let url = format!("{}{}", state.api_url, path);
    let resp = state
        .http_client
        .post(&url)
        .bearer_auth(&state.api_token)
        .json(&payload)
        .send()
        .await
        .map_err(|e| format!("HTTP 请求失败: {e}"))?;
    let status = resp.status();
    let body: Value = resp.json().await.map_err(|e| format!("解析响应失败: {e}"))?;
    if status.is_success() {
        Ok(body)
    } else {
        let msg = body.get("message").and_then(Value::as_str).unwrap_or("未知错误");
        Err(format!("API 错误 ({}): {msg}", status.as_u16()))
    }
}

async fn api_delete(state: &AppState, path: &str) -> Result<Value, String> {
    let url = format!("{}{}", state.api_url, path);
    let resp = state
        .http_client
        .delete(&url)
        .bearer_auth(&state.api_token)
        .send()
        .await
        .map_err(|e| format!("HTTP 请求失败: {e}"))?;
    let status = resp.status();
    let body: Value = resp.json().await.map_err(|e| format!("解析响应失败: {e}"))?;
    if status.is_success() {
        Ok(body)
    } else {
        let msg = body.get("message").and_then(Value::as_str).unwrap_or("未知错误");
        Err(format!("API 错误 ({}): {msg}", status.as_u16()))
    }
}

async fn exec_catalog_search(state: &AppState, args: &Value) -> Result<Value, String> {
    let query = args
        .get("query")
        .and_then(Value::as_str)
        .unwrap_or("");
    let limit = args
        .get("limit")
        .and_then(Value::as_u64)
        .unwrap_or(10);
    let path = format!("/catalog/search?q={query}&limit={limit}");
    api_get(state, &path).await
}

async fn exec_subscriptions_list(state: &AppState, args: &Value) -> Result<Value, String> {
    let status = args
        .get("status")
        .and_then(Value::as_str)
        .unwrap_or("all");
    let limit = args
        .get("limit")
        .and_then(Value::as_u64)
        .unwrap_or(20);
    let path = format!("/subscriptions?status={status}&limit={limit}");
    api_get(state, &path).await
}

async fn exec_downloads_list(state: &AppState, args: &Value) -> Result<Value, String> {
    let status = args
        .get("status")
        .and_then(Value::as_str)
        .unwrap_or("all");
    let limit = args
        .get("limit")
        .and_then(Value::as_u64)
        .unwrap_or(20);
    let path = format!("/downloads?status={status}&limit={limit}");
    api_get(state, &path).await
}

async fn exec_downloads_create(state: &AppState, args: &Value) -> Result<Value, String> {
    let url = args
        .get("url")
        .and_then(Value::as_str)
        .ok_or("缺少 url 参数")?;
    let save_path = args.get("save_path").and_then(Value::as_str).unwrap_or("");
    let category = args.get("category").and_then(Value::as_str).unwrap_or("");
    let mut payload = json!({ "url": url });
    if !save_path.is_empty() {
        payload["save_path"] = json!(save_path);
    }
    if !category.is_empty() {
        payload["category"] = json!(category);
    }
    api_post(state, "/downloads", payload).await
}

async fn exec_sites_list(state: &AppState) -> Result<Value, String> {
    api_get(state, "/sites").await
}

async fn exec_downloader_status(state: &AppState) -> Result<Value, String> {
    api_get(state, "/downloader").await
}

async fn exec_torrents_list(state: &AppState, args: &Value) -> Result<Value, String> {
    let limit = args
        .get("limit")
        .and_then(Value::as_u64)
        .unwrap_or(20);
    let path = format!("/torrents?limit={limit}");
    api_get(state, &path).await
}

async fn exec_subscriptions_create(state: &AppState, args: &Value) -> Result<Value, String> {
    let title = args
        .get("title")
        .and_then(Value::as_str)
        .ok_or("缺少 title 参数")?;
    let media_type = args
        .get("media_type")
        .and_then(Value::as_str)
        .unwrap_or("movie");
    let tmdb_id = args.get("tmdb_id").and_then(Value::as_u64);
    let year = args.get("year").and_then(Value::as_u64);

    let mut payload = json!({
        "title": title,
        "media_type": media_type
    });
    if let Some(id) = tmdb_id {
        payload["tmdb_id"] = json!(id);
    }
    if let Some(y) = year {
        payload["year"] = json!(y);
    }
    api_post(state, "/subscriptions", payload).await
}

async fn exec_subscriptions_delete(state: &AppState, args: &Value) -> Result<Value, String> {
    let id = args
        .get("id")
        .and_then(Value::as_u64)
        .ok_or("缺少 id 参数")?;
    let path = format!("/subscriptions/{id}");
    api_delete(state, &path).await
}

async fn api_patch(state: &AppState, path: &str, payload: Value) -> Result<Value, String> {
    let url = format!("{}{}", state.api_url, path);
    let resp = state
        .http_client
        .patch(&url)
        .bearer_auth(&state.api_token)
        .json(&payload)
        .send()
        .await
        .map_err(|e| format!("HTTP 请求失败: {e}"))?;
    let status = resp.status();
    let body: Value = resp.json().await.map_err(|e| format!("解析响应失败: {e}"))?;
    if status.is_success() {
        Ok(body)
    } else {
        let msg = body.get("message").and_then(Value::as_str).unwrap_or("未知错误");
        Err(format!("API 错误 ({}): {msg}", status.as_u16()))
    }
}

async fn exec_downloads_delete(state: &AppState, args: &Value) -> Result<Value, String> {
    let id = args
        .get("id")
        .and_then(Value::as_u64)
        .ok_or("缺少 id 参数")?;
    let delete_files = args
        .get("delete_files")
        .and_then(Value::as_bool)
        .unwrap_or(true);
    let payload = json!({ "delete_files": delete_files });
    api_post(state, &format!("/downloads/{id}/delete"), payload).await
}

async fn exec_downloads_pause(state: &AppState, args: &Value) -> Result<Value, String> {
    let id = args
        .get("id")
        .and_then(Value::as_u64)
        .ok_or("缺少 id 参数")?;
    api_post(state, &format!("/downloads/{id}/pause"), json!({})).await
}

async fn exec_downloads_resume(state: &AppState, args: &Value) -> Result<Value, String> {
    let id = args
        .get("id")
        .and_then(Value::as_u64)
        .ok_or("缺少 id 参数")?;
    api_post(state, &format!("/downloads/{id}/resume"), json!({})).await
}

async fn exec_system_logs(state: &AppState, args: &Value) -> Result<Value, String> {
    let mut path = "/logs".to_string();
    let limit = args.get("limit").and_then(Value::as_u64);
    let level = args.get("level").and_then(Value::as_str);
    let mut params = vec![];
    if let Some(l) = limit {
        params.push(format!("limit={l}"));
    }
    if let Some(lv) = level {
        params.push(format!("level={lv}"));
    }
    if !params.is_empty() {
        path.push('?');
        path.push_str(&params.join("&"));
    }
    api_get(state, &path).await
}

async fn exec_filters_list(state: &AppState) -> Result<Value, String> {
    api_get(state, "/filters").await
}

async fn exec_filters_create(state: &AppState, args: &Value) -> Result<Value, String> {
    let name = args
        .get("name")
        .and_then(Value::as_str)
        .ok_or("缺少 name 参数")?;
    let pattern = args
        .get("pattern")
        .and_then(Value::as_str)
        .ok_or("缺少 pattern 参数")?;
    let filter_type = args
        .get("filter_type")
        .and_then(Value::as_str)
        .unwrap_or("include");
    let enabled = args
        .get("enabled")
        .and_then(Value::as_bool)
        .unwrap_or(true);
    let site_id = args.get("site_id").and_then(Value::as_u64);

    let mut payload = json!({
        "name": name,
        "pattern": pattern,
        "filter_type": filter_type,
        "enabled": enabled
    });
    if let Some(sid) = site_id {
        payload["site_id"] = json!(sid);
    }
    api_post(state, "/filters", payload).await
}

async fn exec_filters_update(state: &AppState, args: &Value) -> Result<Value, String> {
    let id = args
        .get("id")
        .and_then(Value::as_u64)
        .ok_or("缺少 id 参数")?;
    let mut payload = json!({});
    if let Some(v) = args.get("name").and_then(Value::as_str) {
        payload["name"] = json!(v);
    }
    if let Some(v) = args.get("pattern").and_then(Value::as_str) {
        payload["pattern"] = json!(v);
    }
    if let Some(v) = args.get("filter_type").and_then(Value::as_str) {
        payload["filter_type"] = json!(v);
    }
    if let Some(v) = args.get("site_id").and_then(Value::as_u64) {
        payload["site_id"] = json!(v);
    }
    if let Some(v) = args.get("enabled").and_then(Value::as_bool) {
        payload["enabled"] = json!(v);
    }
    api_patch(state, &format!("/filters/{id}"), payload).await
}

async fn exec_filters_delete(state: &AppState, args: &Value) -> Result<Value, String> {
    let id = args
        .get("id")
        .and_then(Value::as_u64)
        .ok_or("缺少 id 参数")?;
    let path = format!("/filters/{id}");
    api_delete(state, &path).await
}

async fn exec_send_notification(state: &AppState, args: &Value) -> Result<Value, String> {
    let title = args
        .get("title")
        .and_then(Value::as_str)
        .ok_or("缺少 title 参数")?;
    let message = args
        .get("message")
        .and_then(Value::as_str)
        .ok_or("缺少 message 参数")?;
    let level = args
        .get("level")
        .and_then(Value::as_str)
        .unwrap_or("info");
    let payload = json!({
        "title": title,
        "message": message,
        "level": level
    });
    api_post(state, "/notifications", payload).await
}

async fn call_tool(state: &AppState, name: &str, args: &Value) -> Result<Value, String> {
    match name {
        "catalog_search" => exec_catalog_search(state, args).await,
        "subscriptions_list" => exec_subscriptions_list(state, args).await,
        "subscriptions_create" => exec_subscriptions_create(state, args).await,
        "subscriptions_delete" => exec_subscriptions_delete(state, args).await,
        "downloads_list" => exec_downloads_list(state, args).await,
        "downloads_create" => exec_downloads_create(state, args).await,
        "downloads_delete" => exec_downloads_delete(state, args).await,
        "downloads_pause" => exec_downloads_pause(state, args).await,
        "downloads_resume" => exec_downloads_resume(state, args).await,
        "sites_list" => exec_sites_list(state).await,
        "downloader_status" => exec_downloader_status(state).await,
        "torrents_list" => exec_torrents_list(state, args).await,
        "system_logs" => exec_system_logs(state, args).await,
        "filters_list" => exec_filters_list(state).await,
        "filters_create" => exec_filters_create(state, args).await,
        "filters_update" => exec_filters_update(state, args).await,
        "filters_delete" => exec_filters_delete(state, args).await,
        "send_notification" => exec_send_notification(state, args).await,
        _ => Err(format!("未知工具: {name}")),
    }
}

fn tool_result_to_mcp(api_result: Result<Value, String>) -> CallToolResult {
    match api_result {
        Ok(value) => {
            let text = serde_json::to_string_pretty(&value).unwrap_or_else(|e| e.to_string());
            CallToolResult {
                content: vec![ToolContent { content_type: "text", text }],
                is_error: None,
            }
        }
        Err(err) => CallToolResult {
            content: vec![ToolContent { content_type: "text", text: err }],
            is_error: Some(true),
        },
    }
}

// ── JSON-RPC Dispatch ───────────────────────────────────────────

async fn dispatch_jsonrpc(state: &AppState, request: JsonRpcRequest) -> JsonRpcResponse {
    let id = request.id.clone();
    match request.method.as_str() {
        "initialize" => {
            let _params: InitializeParams = match serde_json::from_value(request.params.clone()) {
                Ok(p) => p,
                Err(e) => return JsonRpcResponse::err(id, -32602, format!("参数解析失败: {e}")),
            };
            let result = InitializeResult {
                protocol_version: "2024-11-05",
                capabilities: json!({
                    "tools": {}
                }),
                server_info: ServerInfo {
                    name: "mediary-mcp-server",
                    version: "0.1.0",
                },
                instructions: "通过 MCP 连接到 Mediary 媒体管理中心。可用工具包括搜索目录、管理订阅、查看下载等。",
            };
            JsonRpcResponse::ok(id, serde_json::to_value(result).unwrap_or_default())
        }
        "notifications/initialized" => {
            JsonRpcResponse::ok(id, json!({}))
        }
        "tools/list" => {
            let tools = define_tools();
            let result = json!({
                "tools": tools.iter().map(|t| serde_json::to_value(t).unwrap_or_default()).collect::<Vec<_>>()
            });
            JsonRpcResponse::ok(id, result)
        }
        "tools/call" => {
            let params: CallToolParams = match serde_json::from_value(request.params.clone()) {
                Ok(p) => p,
                Err(e) => return JsonRpcResponse::err(id, -32602, format!("参数解析失败: {e}")),
            };
            let raw = call_tool(state, &params.name, &params.arguments).await;
            let mcp_result = tool_result_to_mcp(raw);
            JsonRpcResponse::ok(id, serde_json::to_value(mcp_result).unwrap_or_default())
        }
        "ping" => JsonRpcResponse::ok(id, json!({})),
        _ => JsonRpcResponse::err(id, -32601, format!("不支持的方法: {}", request.method)),
    }
}

// ── HTTP / SSE Routes ───────────────────────────────────────────

async fn handle_post(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    body: String,
) -> Result<(StatusCode, HeaderMap, Json<Value>), StatusCode> {
    let request: JsonRpcRequest = match serde_json::from_str(&body) {
        Ok(r) => r,
        Err(e) => {
            let resp = JsonRpcResponse::err(Value::Null, -32700, format!("解析请求失败: {e}"));
            let mut h = HeaderMap::new();
            h.insert(header::CONTENT_TYPE, "application/json".parse().unwrap());
            return Ok((StatusCode::OK, h, Json(serde_json::to_value(resp).unwrap_or_default())));
        }
    };

    let id = request.id.clone();

    if id == Value::Null || id == json!(null) {
        dispatch_jsonrpc(&state, request).await;
        let mut resp_headers = HeaderMap::new();
        if let Some(session_id) = headers.get("mcp-session-id").and_then(|v| v.to_str().ok()) {
            resp_headers.insert("Mcp-Session-Id", session_id.parse().unwrap());
        }
        return Ok((StatusCode::ACCEPTED, resp_headers, Json(json!({}))));
    }

    let response = dispatch_jsonrpc(&state, request).await;
    let mut resp_headers = HeaderMap::new();

    if let Some(session_id) = headers.get("mcp-session-id").and_then(|v| v.to_str().ok()) {
        resp_headers.insert("Mcp-Session-Id", session_id.parse().unwrap());
    } else {
        let counter = state.request_counter.fetch_add(1, Ordering::SeqCst);
        let session_id = format!("mcp-session-{counter}");
        resp_headers.insert("Mcp-Session-Id", session_id.parse().unwrap());
    }

    Ok((StatusCode::OK, resp_headers, Json(serde_json::to_value(response).unwrap_or_default())))
}

async fn handle_get(
    State(state): State<Arc<AppState>>,
    Query(_params): Query<HashMap<String, String>>,
    headers: HeaderMap,
) -> Result<Sse<impl tokio_stream::Stream<Item = Result<Event, std::convert::Infallible>>>, StatusCode> {
    let accept = headers
        .get(header::ACCEPT)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");
    if !accept.contains("text/event-stream") {
        return Err(StatusCode::BAD_REQUEST);
    }

    let rx = state.sse_tx.subscribe();
    let stream = BroadcastStream::new(rx).filter_map(|msg| {
        match msg {
            Ok(text) => Some(Ok(Event::default().data(text))),
            Err(_) => None,
        }
    });

    Ok(Sse::new(stream).keep_alive(
        axum::response::sse::KeepAlive::new()
            .interval(std::time::Duration::from_secs(30))
            .text("ping"),
    ))
}

// ── Main Entry Point ────────────────────────────────────────────

async fn run_mediary_stdin(_state: Arc<AppState>) {
    let mut input = String::new();
    loop {
        input.clear();
        match std::io::stdin().read_line(&mut input) {
            Ok(0) => {
                eprintln!("stdin 已关闭");
                break;
            }
            Ok(_) => {
                let action = std::env::var("MEDIARY_PLUGIN_ACTION").unwrap_or_default();
                let trigger = std::env::var("MEDIARY_PLUGIN_TRIGGER").unwrap_or_default();
                let _payload: Value = serde_json::from_str(&input).unwrap_or(Value::Null);

                match action.as_str() {
                    "status" => {
                        let result = json!({
                            "notice": "MCP 服务器运行中 — 端点: http://localhost:8100/mcp",
                            "items": [
                                {
                                    "title": "OpenClaw 连接",
                                    "subtitle": "使用 Streamable HTTP 传输，填入端点地址即可",
                                    "metadata": [{"label": "端点", "value": "http://localhost:8100/mcp"}],
                                    "actions": [
                                        {
                                            "type": "copy",
                                            "label": "复制端点",
                                            "text": "http://localhost:8100/mcp"
                                        }
                                    ]
                                },
                                {
                                    "title": "Hemes 连接",
                                    "subtitle": "添加 MCP 服务器，选择 Streamable HTTP 类型",
                                    "metadata": [{"label": "端点", "value": "http://localhost:8100/mcp"}],
                                    "actions": [
                                        {
                                            "type": "copy",
                                            "label": "复制端点",
                                            "text": "http://localhost:8100/mcp"
                                        }
                                    ]
                                }
                            ],
                            "report": {
                                "port": 8100,
                                "tools": 18,
                                "trigger": trigger
                            }
                        });
                        println!("{result}");
                    }
                    _ => {
                        let result = json!({
                            "notice": format!("MCP 服务器运行中 (action={action})"),
                        });
                        println!("{result}");
                    }
                }
            }
            Err(e) => {
                eprintln!("读取 stdin 失败: {e}");
                break;
            }
        }
    }
}

async fn run() -> Result<(), String> {
    let api_url = std::env::var("MEDIARY_PLUGIN_API_URL")
        .map_err(|_| "缺少 MEDIARY_PLUGIN_API_URL 环境变量".to_string())?;
    let api_token = std::env::var("MEDIARY_PLUGIN_TOKEN")
        .map_err(|_| "缺少 MEDIARY_PLUGIN_TOKEN 环境变量".to_string())?;

    let default_port: u16 = 8100;

    let (sse_tx, _) = broadcast::channel::<String>(64);

    let state = Arc::new(AppState {
        api_url,
        api_token,
        http_client: reqwest::Client::new(),
        sse_tx,
        request_counter: AtomicU64::new(0),
    });

    let app = Router::new()
        .route("/mcp", post(handle_post).get(handle_get))
        .with_state(state.clone());

    let addr = format!("0.0.0.0:{default_port}");
    let listener = TcpListener::bind(&addr)
        .await
        .map_err(|e| format!("绑定地址失败 ({addr}): {e}"))?;

    eprintln!("MCP 服务器启动: http://{addr}/mcp");

    let server = axum::serve(listener, app);

    let state_for_stdin = state.clone();
    let stdin_handle = tokio::spawn(async move {
        run_mediary_stdin(state_for_stdin).await;
    });

    tokio::select! {
        result = server => {
            if let Err(e) = result {
                return Err(format!("HTTP 服务器错误: {e}"));
            }
        }
        _ = stdin_handle => {}
    }

    Ok(())
}

#[tokio::main]
async fn main() {
    if let Err(error) = run().await {
        eprintln!("{error}");
        std::process::exit(1);
    }
}
