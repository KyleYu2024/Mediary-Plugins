use reqwest::Client;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::env;

const ROUTE_CATALOG_SEARCH: &str = "/search/tmdb";
const ROUTE_SUBSCRIPTIONS: &str = "/subscriptions";
const ROUTE_DOWNLOADS: &str = "/downloads";
const ROUTE_SITES: &str = "/sites";
const ROUTE_DOWNLOADER: &str = "/plugin/downloader";
const ROUTE_TORRENTS: &str = "/plugin/torrents";
const ROUTE_ADD_DOWNLOAD: &str = "/plugin/downloads";
const ROUTE_CONTROL_DOWNLOADS: &str = "/plugin/downloader/torrents/control";
const ROUTE_LOGS: &str = "/logs";
const ROUTE_FILTER_RULES: &str = "/filter/rules";
const ROUTE_NOTIFICATIONS: &str = "/plugin/notifications";

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
        Self {
            jsonrpc: "2.0",
            id,
            result: Some(result),
            error: None,
        }
    }
    fn err(id: Value, code: i64, message: impl Into<String>) -> Self {
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

// ── MCP Protocol Types ──────────────────────────────────────────

#[derive(Debug, Deserialize)]
#[allow(dead_code)]
struct InitializeParams {
    #[serde(rename = "protocolVersion")]
    protocol_version: String,
    capabilities: Value,
    #[serde(rename = "clientInfo")]
    client_info: ClientInfo,
}

#[derive(Debug, Deserialize)]
#[allow(dead_code)]
struct ClientInfo {
    name: String,
    version: String,
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
                    "media_type": {
                        "type": "string",
                        "description": "媒体类型：multi、movie 或 tv",
                        "enum": ["multi", "movie", "tv"],
                        "default": "multi"
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
                    "tmdb_id": {
                        "type": "integer",
                        "description": "按 TMDB ID 精确筛选"
                    },
                    "media_type": {
                        "type": "string",
                        "description": "按媒体类型筛选：movie 或 tv",
                        "enum": ["movie", "tv"]
                    },
                    "season": {
                        "type": "integer",
                        "description": "按季号精确筛选"
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
                        "description": "TMDB 媒体 ID"
                    },
                    "year": {
                        "type": "integer",
                        "description": "发行年份（选填）"
                    },
                    "season": {
                        "type": "integer",
                        "description": "电视剧季号；media_type 为 tv 时必填"
                    }
                },
                "required": ["title", "tmdb_id"]
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
                    "hash": {
                        "type": "string",
                        "description": "要删除的下载任务 torrent hash"
                    },
                    "delete_files": {
                        "type": "boolean",
                        "description": "是否同时删除已下载的文件，默认 true",
                        "default": true
                    }
                },
                "required": ["hash"]
            }),
        },
        ToolDefinition {
            name: "downloads_pause".into(),
            description: "暂停指定的下载任务。".into(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "hash": {
                        "type": "string",
                        "description": "要暂停的下载任务 torrent hash"
                    }
                },
                "required": ["hash"]
            }),
        },
        ToolDefinition {
            name: "downloads_resume".into(),
            description: "恢复已暂停的下载任务。".into(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "hash": {
                        "type": "string",
                        "description": "要恢复的下载任务 torrent hash"
                    }
                },
                "required": ["hash"]
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
                    "scope": {
                        "type": "string",
                        "description": "日志范围",
                        "enum": ["general", "cloudhub_broadcast", "pt_scheduled_fetch", "plugin", "all"],
                        "default": "all"
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
            description: "创建新的自定义过滤规则。".into(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "id": {
                        "type": "string",
                        "description": "唯一规则 ID"
                    },
                    "name": {
                        "type": "string",
                        "description": "过滤规则名称"
                    },
                    "include": {
                        "type": "string",
                        "description": "必须包含的关键词表达式"
                    },
                    "exclude": {
                        "type": "string",
                        "description": "必须排除的关键词表达式"
                    },
                    "size_range": {
                        "type": "string",
                        "description": "体积范围表达式"
                    },
                    "seeders": {
                        "type": "string",
                        "description": "做种人数表达式"
                    }
                },
                "required": ["id", "name"]
            }),
        },
        ToolDefinition {
            name: "filters_update".into(),
            description: "按规则 ID 更新已有的自定义过滤规则。".into(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "id": {
                        "type": "string",
                        "description": "过滤规则 ID"
                    },
                    "name": {
                        "type": "string",
                        "description": "新的规则名称"
                    },
                    "include": {
                        "type": "string",
                        "description": "新的包含表达式"
                    },
                    "exclude": {
                        "type": "string",
                        "description": "新的排除表达式"
                    },
                    "size_range": {
                        "type": "string",
                        "description": "新的体积范围表达式"
                    },
                    "seeders": {
                        "type": "string",
                        "description": "新的做种人数表达式"
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
                        "type": "string",
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
                    }
                },
                "required": ["title", "message"]
            }),
        },
    ]
}

// ── Mediary API Helpers ─────────────────────────────────────────

struct PluginContext {
    api_url: String,
    api_token: String,
    http_client: Client,
}

async fn api_get(ctx: &PluginContext, path: &str) -> Result<Value, String> {
    let url = format!("{}{}", ctx.api_url, path);
    let resp = ctx
        .http_client
        .get(url)
        .bearer_auth(&ctx.api_token)
        .send()
        .await
        .map_err(|e| format!("HTTP 请求失败: {e}"))?;
    parse_response(resp).await
}

async fn api_get_query(
    ctx: &PluginContext,
    path: &str,
    query: &[(&str, String)],
) -> Result<Value, String> {
    let url = format!("{}{}", ctx.api_url, path);
    let resp = ctx
        .http_client
        .get(url)
        .query(query)
        .bearer_auth(&ctx.api_token)
        .send()
        .await
        .map_err(|e| format!("HTTP 请求失败: {e}"))?;
    parse_response(resp).await
}

async fn api_post(ctx: &PluginContext, path: &str, payload: Value) -> Result<Value, String> {
    let url = format!("{}{}", ctx.api_url, path);
    let resp = ctx
        .http_client
        .post(url)
        .bearer_auth(&ctx.api_token)
        .json(&payload)
        .send()
        .await
        .map_err(|e| format!("HTTP 请求失败: {e}"))?;
    parse_response(resp).await
}

async fn api_delete(ctx: &PluginContext, path: &str) -> Result<Value, String> {
    let url = format!("{}{}", ctx.api_url, path);
    let resp = ctx
        .http_client
        .delete(url)
        .bearer_auth(&ctx.api_token)
        .send()
        .await
        .map_err(|e| format!("HTTP 请求失败: {e}"))?;
    parse_response(resp).await
}

async fn parse_response(resp: reqwest::Response) -> Result<Value, String> {
    let status = resp.status();
    let text = resp
        .text()
        .await
        .map_err(|e| format!("读取响应失败: {e}"))?;
    let body = if text.trim().is_empty() {
        json!({})
    } else {
        serde_json::from_str(&text).map_err(|e| format!("解析响应失败: {e}"))?
    };
    if status.is_success() {
        Ok(body)
    } else {
        let msg = body
            .get("error")
            .or_else(|| body.get("message"))
            .and_then(Value::as_str)
            .unwrap_or("未知错误");
        Err(format!("API 错误 ({}): {msg}", status.as_u16()))
    }
}

// ── Tool Executors ──────────────────────────────────────────────

async fn exec_catalog_search(ctx: &PluginContext, args: &Value) -> Result<Value, String> {
    let query = required_string(args, "query")?;
    let media_type = args
        .get("media_type")
        .and_then(Value::as_str)
        .unwrap_or("multi");
    let limit = bounded_limit(args, 10, 100);
    let params = [
        ("query", query.to_string()),
        ("media_type", media_type.to_string()),
    ];
    let result = api_get_query(ctx, ROUTE_CATALOG_SEARCH, &params).await?;
    truncate_array(result, limit)
}

async fn exec_subscriptions_list(ctx: &PluginContext, args: &Value) -> Result<Value, String> {
    let mut params = vec![("limit", bounded_limit(args, 20, 1_000).to_string())];
    if let Some(tmdb_id) = args.get("tmdb_id").and_then(Value::as_u64) {
        params.push(("tmdb_id", tmdb_id.to_string()));
    }
    if let Some(media_type) = args.get("media_type").and_then(Value::as_str) {
        params.push(("media_type", media_type.to_string()));
    }
    if let Some(season) = args.get("season").and_then(Value::as_u64) {
        params.push(("season", season.to_string()));
    }
    api_get_query(ctx, ROUTE_SUBSCRIPTIONS, &params).await
}

async fn exec_downloads_list(ctx: &PluginContext, args: &Value) -> Result<Value, String> {
    let status = args.get("status").and_then(Value::as_str).unwrap_or("all");
    let limit = bounded_limit(args, 20, 200);
    let result = api_get(ctx, ROUTE_DOWNLOADS).await?;
    filter_downloads(result, status, limit)
}

async fn exec_downloads_create(ctx: &PluginContext, args: &Value) -> Result<Value, String> {
    let url = required_string(args, "url")?;
    let save_path = args.get("save_path").and_then(Value::as_str).unwrap_or("");
    let category = args.get("category").and_then(Value::as_str).unwrap_or("");
    let mut payload = json!({ "torrent_url": url });
    if !save_path.is_empty() {
        payload["save_path"] = json!(save_path);
    }
    if !category.is_empty() {
        payload["category"] = json!(category);
    }
    api_post(ctx, ROUTE_ADD_DOWNLOAD, payload).await
}

async fn exec_sites_list(ctx: &PluginContext) -> Result<Value, String> {
    api_get(ctx, ROUTE_SITES).await
}

async fn exec_downloader_status(ctx: &PluginContext) -> Result<Value, String> {
    api_get(ctx, ROUTE_DOWNLOADER).await
}

async fn exec_torrents_list(ctx: &PluginContext, args: &Value) -> Result<Value, String> {
    let params = [("limit", bounded_limit(args, 20, 200).to_string())];
    api_get_query(ctx, ROUTE_TORRENTS, &params).await
}

async fn exec_subscriptions_create(ctx: &PluginContext, args: &Value) -> Result<Value, String> {
    let title = required_string(args, "title")?;
    let media_type = args
        .get("media_type")
        .and_then(Value::as_str)
        .unwrap_or("movie");
    if !matches!(media_type, "movie" | "tv") {
        return Err("media_type 仅支持 movie 或 tv".to_string());
    }
    let tmdb_id = required_positive_u64(args, "tmdb_id")?;
    let year = args.get("year").and_then(Value::as_u64);
    let season = args.get("season").and_then(Value::as_u64);
    if media_type == "tv" && season.is_none() {
        return Err("电视剧订阅缺少 season 参数".to_string());
    }

    let payload = subscription_payload(title, media_type, tmdb_id, year, season);
    api_post(ctx, ROUTE_SUBSCRIPTIONS, payload).await
}

async fn exec_subscriptions_delete(ctx: &PluginContext, args: &Value) -> Result<Value, String> {
    let id = required_positive_u64(args, "id")?;
    let path = format!("{ROUTE_SUBSCRIPTIONS}/{id}");
    api_delete(ctx, &path).await
}

async fn exec_downloads_delete(ctx: &PluginContext, args: &Value) -> Result<Value, String> {
    let hash = required_string(args, "hash")?;
    let delete_files = args
        .get("delete_files")
        .and_then(Value::as_bool)
        .unwrap_or(true);
    control_download(ctx, "delete", hash, Some(delete_files)).await
}

async fn exec_downloads_pause(ctx: &PluginContext, args: &Value) -> Result<Value, String> {
    let hash = required_string(args, "hash")?;
    control_download(ctx, "pause", hash, None).await
}

async fn exec_downloads_resume(ctx: &PluginContext, args: &Value) -> Result<Value, String> {
    let hash = required_string(args, "hash")?;
    control_download(ctx, "resume", hash, None).await
}

async fn exec_system_logs(ctx: &PluginContext, args: &Value) -> Result<Value, String> {
    let scope = args.get("scope").and_then(Value::as_str).unwrap_or("all");
    let params = [
        ("limit", bounded_limit(args, 50, 800).to_string()),
        ("scope", scope.to_string()),
    ];
    api_get_query(ctx, ROUTE_LOGS, &params).await
}

async fn exec_filters_list(ctx: &PluginContext) -> Result<Value, String> {
    api_get(ctx, ROUTE_FILTER_RULES).await
}

async fn exec_filters_create(ctx: &PluginContext, args: &Value) -> Result<Value, String> {
    let id = required_string(args, "id")?;
    let name = required_string(args, "name")?;
    let mut rules = load_custom_rules(ctx).await?;
    if rules
        .iter()
        .any(|rule| rule.get("id").and_then(Value::as_str) == Some(id))
    {
        return Err(format!("过滤规则 ID 已存在: {id}"));
    }
    rules.push(json!({
        "id": id,
        "name": name,
        "include": optional_string(args, "include")?,
        "exclude": optional_string(args, "exclude")?,
        "size_range": optional_string(args, "size_range")?,
        "seeders": optional_string(args, "seeders")?,
    }));
    save_custom_rules(ctx, rules).await
}

async fn exec_filters_update(ctx: &PluginContext, args: &Value) -> Result<Value, String> {
    let id = required_string(args, "id")?;
    let mut rules = load_custom_rules(ctx).await?;
    let rule = rules
        .iter_mut()
        .find(|rule| rule.get("id").and_then(Value::as_str) == Some(id))
        .ok_or_else(|| format!("过滤规则不存在: {id}"))?;
    for key in ["name", "include", "exclude", "size_range", "seeders"] {
        if let Some(value) = args.get(key) {
            if !value.is_null() && !value.is_string() {
                return Err(format!("{key} 必须是字符串或 null"));
            }
            rule[key] = value.clone();
        }
    }
    save_custom_rules(ctx, rules).await
}

async fn exec_filters_delete(ctx: &PluginContext, args: &Value) -> Result<Value, String> {
    let id = required_string(args, "id")?;
    let mut rules = load_custom_rules(ctx).await?;
    let previous_len = rules.len();
    rules.retain(|rule| rule.get("id").and_then(Value::as_str) != Some(id));
    if rules.len() == previous_len {
        return Err(format!("过滤规则不存在: {id}"));
    }
    save_custom_rules(ctx, rules).await
}

async fn exec_send_notification(ctx: &PluginContext, args: &Value) -> Result<Value, String> {
    let title = required_string(args, "title")?;
    let message = required_string(args, "message")?;
    let payload = json!({ "title": title, "content": message });
    api_post(ctx, ROUTE_NOTIFICATIONS, payload).await
}

fn required_string<'a>(args: &'a Value, key: &str) -> Result<&'a str, String> {
    args.get(key)
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .ok_or_else(|| format!("缺少 {key} 参数"))
}

fn required_positive_u64(args: &Value, key: &str) -> Result<u64, String> {
    args.get(key)
        .and_then(Value::as_u64)
        .filter(|value| *value > 0)
        .ok_or_else(|| format!("{key} 必须是正整数"))
}

fn optional_string(args: &Value, key: &str) -> Result<Option<String>, String> {
    match args.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(value)) => Ok(Some(value.trim().to_string())),
        Some(_) => Err(format!("{key} 必须是字符串")),
    }
}

fn bounded_limit(args: &Value, default: usize, maximum: usize) -> usize {
    args.get("limit")
        .and_then(Value::as_u64)
        .and_then(|value| usize::try_from(value).ok())
        .unwrap_or(default)
        .clamp(1, maximum)
}

fn truncate_array(mut value: Value, limit: usize) -> Result<Value, String> {
    let items = value
        .as_array_mut()
        .ok_or_else(|| "Mediary API 返回的不是列表".to_string())?;
    items.truncate(limit);
    Ok(value)
}

fn filter_downloads(mut value: Value, status: &str, limit: usize) -> Result<Value, String> {
    let items = value
        .as_array_mut()
        .ok_or_else(|| "Mediary 下载接口返回的不是列表".to_string())?;
    if status != "all" {
        items.retain(|item| item.get("status").and_then(Value::as_str) == Some(status));
    }
    items.truncate(limit);
    Ok(value)
}

fn subscription_payload(
    title: &str,
    media_type: &str,
    tmdb_id: u64,
    year: Option<u64>,
    season: Option<u64>,
) -> Value {
    let mut payload = json!({
        "name": title,
        "media_type": media_type,
        "tmdb_id": tmdb_id,
    });
    if let Some(value) = year {
        payload["year"] = json!(value);
    }
    if let Some(value) = season {
        payload["season"] = json!(value);
    }
    payload
}

fn downloader_control_payload(action: &str, hash: &str, delete_files: Option<bool>) -> Value {
    let mut payload = json!({
        "action": action,
        "hashes": [hash],
    });
    if let Some(value) = delete_files {
        payload["delete_files"] = json!(value);
    }
    payload
}

async fn control_download(
    ctx: &PluginContext,
    action: &str,
    hash: &str,
    delete_files: Option<bool>,
) -> Result<Value, String> {
    let payload = downloader_control_payload(action, hash, delete_files);
    api_post(ctx, ROUTE_CONTROL_DOWNLOADS, payload).await
}

async fn load_custom_rules(ctx: &PluginContext) -> Result<Vec<Value>, String> {
    let response = api_get(ctx, ROUTE_FILTER_RULES).await?;
    response
        .get("custom_filter_rules")
        .and_then(Value::as_array)
        .cloned()
        .ok_or_else(|| "Mediary 过滤规则响应缺少 custom_filter_rules".to_string())
}

async fn save_custom_rules(ctx: &PluginContext, rules: Vec<Value>) -> Result<Value, String> {
    api_post(
        ctx,
        ROUTE_FILTER_RULES,
        json!({ "custom_filter_rules": rules }),
    )
    .await
}

async fn call_tool(ctx: &PluginContext, name: &str, args: &Value) -> Result<Value, String> {
    match name {
        "catalog_search" => exec_catalog_search(ctx, args).await,
        "subscriptions_list" => exec_subscriptions_list(ctx, args).await,
        "subscriptions_create" => exec_subscriptions_create(ctx, args).await,
        "subscriptions_delete" => exec_subscriptions_delete(ctx, args).await,
        "downloads_list" => exec_downloads_list(ctx, args).await,
        "downloads_create" => exec_downloads_create(ctx, args).await,
        "downloads_delete" => exec_downloads_delete(ctx, args).await,
        "downloads_pause" => exec_downloads_pause(ctx, args).await,
        "downloads_resume" => exec_downloads_resume(ctx, args).await,
        "sites_list" => exec_sites_list(ctx).await,
        "downloader_status" => exec_downloader_status(ctx).await,
        "torrents_list" => exec_torrents_list(ctx, args).await,
        "system_logs" => exec_system_logs(ctx, args).await,
        "filters_list" => exec_filters_list(ctx).await,
        "filters_create" => exec_filters_create(ctx, args).await,
        "filters_update" => exec_filters_update(ctx, args).await,
        "filters_delete" => exec_filters_delete(ctx, args).await,
        "send_notification" => exec_send_notification(ctx, args).await,
        _ => Err(format!("未知工具: {name}")),
    }
}

fn tool_result_to_mcp(api_result: Result<Value, String>) -> CallToolResult {
    match api_result {
        Ok(value) => {
            let text = serde_json::to_string_pretty(&value).unwrap_or_else(|e| e.to_string());
            CallToolResult {
                content: vec![ToolContent {
                    content_type: "text",
                    text,
                }],
                is_error: None,
            }
        }
        Err(err) => CallToolResult {
            content: vec![ToolContent {
                content_type: "text",
                text: err,
            }],
            is_error: Some(true),
        },
    }
}

// ── JSON-RPC Dispatch ───────────────────────────────────────────

async fn dispatch_jsonrpc(ctx: &PluginContext, request: JsonRpcRequest) -> JsonRpcResponse {
    let id = request.id.clone();
    match request.method.as_str() {
        "initialize" => {
            let _params: InitializeParams = match serde_json::from_value(request.params.clone()) {
                Ok(p) => p,
                Err(e) => return JsonRpcResponse::err(id, -32602, format!("参数解析失败: {e}")),
            };
            let result = InitializeResult {
                protocol_version: "2024-11-05",
                capabilities: json!({ "tools": {} }),
                server_info: ServerInfo {
                    name: "mediary-mcp-server",
                    version: "0.2.0",
                },
                instructions: "通过 MCP 连接到 Mediary 媒体管理中心。可用工具包括搜索目录、管理订阅、查看下载等。",
            };
            JsonRpcResponse::ok(id, serde_json::to_value(result).unwrap_or_default())
        }
        "notifications/initialized" => JsonRpcResponse::ok(id, json!({})),
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
            let raw = call_tool(ctx, &params.name, &params.arguments).await;
            let mcp_result = tool_result_to_mcp(raw);
            JsonRpcResponse::ok(id, serde_json::to_value(mcp_result).unwrap_or_default())
        }
        "ping" => JsonRpcResponse::ok(id, json!({})),
        _ => JsonRpcResponse::err(id, -32601, format!("不支持的方法: {}", request.method)),
    }
}

// ── Main Entry Point ────────────────────────────────────────────

async fn run() -> Result<(), String> {
    let api_url = env::var("MEDIARY_PLUGIN_API_URL")
        .map_err(|_| "缺少 MEDIARY_PLUGIN_API_URL 环境变量".to_string())?;
    let api_token = env::var("MEDIARY_PLUGIN_TOKEN")
        .map_err(|_| "缺少 MEDIARY_PLUGIN_TOKEN 环境变量".to_string())?;
    let action = env::var("MEDIARY_PLUGIN_ACTION").unwrap_or_default();
    let trigger = env::var("MEDIARY_PLUGIN_TRIGGER").unwrap_or_default();

    let settings_json = env::var("MEDIARY_PLUGIN_SETTINGS_JSON").unwrap_or_default();
    let settings: Value = serde_json::from_str(&settings_json).unwrap_or(Value::Null);
    let public_url = settings
        .get("public_url")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty());

    let base_url = public_url
        .map(|s| s.trim_end_matches('/'))
        .unwrap_or_else(|| {
            let api = &api_url;
            if let Some(pos) = api.rfind("/api") {
                &api[..pos]
            } else {
                api
            }
        });

    let openclaw_template = format!(
        "{{\"mcpServers\":{{\"mediary\":{{\"type\":\"http\",\"url\":\"{base_url}/mcp\",\"headers\":{{\"Authorization\":\"Bearer <API Token>\"}}}}}}}}"
    );
    let hermes_template = format!(
        "{{\"mcpServers\":{{\"mediary\":{{\"type\":\"streamableHttp\",\"url\":\"{base_url}/mcp\",\"headers\":{{\"Authorization\":\"Bearer <API Token>\"}}}}}}}}"
    );

    let ctx = PluginContext {
        api_url: api_url.trim_end_matches('/').to_string(),
        api_token,
        http_client: reqwest::Client::new(),
    };

    match action.as_str() {
        "status" => {
            let result = json!({
                "notice": "MCP 服务已启动，端点: /mcp",
                "items": [
                    {
                        "title": "鉴权说明",
                        "subtitle": "外部调用 /mcp 需携带 Mediary API Token\nHeader: Authorization: Bearer <你的API Token>\nToken 在 Mediary 设置 → API Token 中获取",
                        "metadata": [],
                        "actions": []
                    },
                    {
                        "title": "OpenClaw 连接",
                        "subtitle": format!("在 MCP 设置中添加服务器，选择 Streamable HTTP 传输\n\n完整配置（直接复制使用）:\n\n{openclaw_template}"),
                        "metadata": [],
                        "actions": []
                    },
                    {
                        "title": "Hermes 连接",
                        "subtitle": format!("在 MCP 设置中添加服务器，类型选择 streamableHttp\n\n完整配置（直接复制使用）:\n\n{hermes_template}"),
                        "metadata": [],
                        "actions": []
                    }
                ],
                "report": {
                    "tools": 18,
                    "trigger": trigger
                }
            });
            println!("{result}");
        }
        _ => {
            let input = std::io::read_to_string(std::io::stdin())
                .map_err(|e| format!("读取请求失败: {e}"))?;
            let request: JsonRpcRequest =
                serde_json::from_str(&input).map_err(|e| format!("JSON-RPC 解析失败: {e}"))?;

            if request.id == Value::Null {
                dispatch_jsonrpc(&ctx, request).await;
                return Ok(());
            }

            let response = dispatch_jsonrpc(&ctx, request).await;
            let output =
                serde_json::to_string(&response).map_err(|e| format!("序列化响应失败: {e}"))?;
            println!("{output}");
        }
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

    #[test]
    fn exposes_eighteen_unique_tools_with_core_arguments() {
        let tools = define_tools();
        assert_eq!(tools.len(), 18);
        assert_eq!(
            tools
                .iter()
                .map(|tool| tool.name.as_str())
                .collect::<HashSet<_>>()
                .len(),
            18
        );

        let create = tools
            .iter()
            .find(|tool| tool.name == "subscriptions_create")
            .unwrap();
        assert_eq!(create.input_schema["required"], json!(["title", "tmdb_id"]));

        for name in ["downloads_delete", "downloads_pause", "downloads_resume"] {
            let tool = tools.iter().find(|tool| tool.name == name).unwrap();
            assert!(tool.input_schema["properties"].get("hash").is_some());
            assert!(tool.input_schema["properties"].get("id").is_none());
        }
    }

    #[test]
    fn uses_current_core_api_routes() {
        assert_eq!(ROUTE_CATALOG_SEARCH, "/search/tmdb");
        assert_eq!(ROUTE_SUBSCRIPTIONS, "/subscriptions");
        assert_eq!(ROUTE_DOWNLOADS, "/downloads");
        assert_eq!(ROUTE_SITES, "/sites");
        assert_eq!(ROUTE_DOWNLOADER, "/plugin/downloader");
        assert_eq!(ROUTE_TORRENTS, "/plugin/torrents");
        assert_eq!(ROUTE_ADD_DOWNLOAD, "/plugin/downloads");
        assert_eq!(
            ROUTE_CONTROL_DOWNLOADS,
            "/plugin/downloader/torrents/control"
        );
        assert_eq!(ROUTE_LOGS, "/logs");
        assert_eq!(ROUTE_FILTER_RULES, "/filter/rules");
        assert_eq!(ROUTE_NOTIFICATIONS, "/plugin/notifications");
    }

    #[test]
    fn builds_subscription_payload_with_core_field_names() {
        let payload = subscription_payload("龙族", "tv", 125_988, Some(2024), Some(2));
        assert_eq!(payload["name"], "龙族");
        assert_eq!(payload["media_type"], "tv");
        assert_eq!(payload["tmdb_id"], 125_988);
        assert_eq!(payload["season"], 2);
        assert!(payload.get("title").is_none());
    }

    #[test]
    fn builds_downloader_control_payload_with_hashes() {
        let payload = downloader_control_payload("delete", "abc123", Some(false));
        assert_eq!(payload["action"], "delete");
        assert_eq!(payload["hashes"], json!(["abc123"]));
        assert_eq!(payload["delete_files"], false);
    }

    #[test]
    fn filters_and_bounds_download_results_locally() {
        let result = filter_downloads(
            json!([
                {"hash": "1", "status": "downloading"},
                {"hash": "2", "status": "paused"},
                {"hash": "3", "status": "downloading"}
            ]),
            "downloading",
            1,
        )
        .unwrap();
        assert_eq!(result, json!([{"hash": "1", "status": "downloading"}]));
        assert_eq!(bounded_limit(&json!({"limit": 0}), 20, 200), 1);
        assert_eq!(bounded_limit(&json!({"limit": 999}), 20, 200), 200);
    }
}
