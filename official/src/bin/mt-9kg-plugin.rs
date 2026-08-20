use reqwest::{Client, Response};
use serde::Deserialize;
use serde_json::{Value, json};
use std::{env, io::Read, time::Duration};

const DEFAULT_RESULT_LIMIT: usize = 100;
const MAX_RESULT_LIMIT: usize = 100;

#[derive(Default, Deserialize)]
struct Settings {
    #[serde(default)]
    free_only: bool,
    #[serde(default)]
    result_limit: Option<usize>,
    #[serde(default)]
    save_path: String,
    #[serde(default)]
    category: String,
    #[serde(default)]
    paused: bool,
}

struct PluginContext {
    api_url: String,
    token: String,
    settings: Settings,
    client: Client,
}

#[tokio::main]
async fn main() {
    if let Err(error) = run().await {
        eprintln!("{error}");
        std::process::exit(1);
    }
}

async fn run() -> Result<(), String> {
    let context = PluginContext::from_env()?;
    let action = env::var("MEDIARY_PLUGIN_ACTION").unwrap_or_default();
    let payload = read_payload()?;
    let output = match action.as_str() {
        "resource_search" => resource_search(&context, &payload).await?,
        "download" => download(&context, &payload).await?,
        _ => return Err(format!("mt-9kg 不支持动作: {action}")),
    };
    println!("{output}");
    Ok(())
}

impl PluginContext {
    fn from_env() -> Result<Self, String> {
        let settings = env::var("MEDIARY_PLUGIN_SETTINGS_JSON")
            .ok()
            .and_then(|value| serde_json::from_str::<Settings>(&value).ok())
            .unwrap_or_default();
        let api_url = required_env("MEDIARY_PLUGIN_API_URL")?
            .trim_end_matches('/')
            .to_string();
        let token = required_env("MEDIARY_PLUGIN_TOKEN")?;
        let client = Client::builder()
            .connect_timeout(Duration::from_secs(8))
            .timeout(Duration::from_secs(80))
            .build()
            .map_err(|error| format!("创建 mt-9kg HTTP 客户端失败: {error}"))?;
        Ok(Self {
            api_url,
            token,
            settings,
            client,
        })
    }
}

async fn resource_search(context: &PluginContext, payload: &Value) -> Result<Value, String> {
    let query = required_text(payload, "query")?;
    let limit = context
        .settings
        .result_limit
        .unwrap_or(DEFAULT_RESULT_LIMIT)
        .clamp(1, MAX_RESULT_LIMIT);
    let limit_text = limit.to_string();
    let free_only_text = if context.settings.free_only {
        "true"
    } else {
        "false"
    };
    let response = context
        .client
        .get(format!("{}/plugin/torrents", context.api_url))
        .bearer_auth(&context.token)
        .query(&[
            ("keyword", query.as_str()),
            ("limit", limit_text.as_str()),
            ("free_only", free_only_text),
            ("mteam_mode", "adult"),
        ])
        .send()
        .await
        .map_err(|error| format!("搜索 mt-9kg 失败: {error}"))?;
    let value = parse_response(response, "搜索 mt-9kg").await?;
    let items = value
        .get("items")
        .and_then(Value::as_array)
        .ok_or_else(|| "Mediary 种子搜索响应缺少 items".to_string())?;
    let results = items.iter().filter_map(host_result).collect::<Vec<_>>();
    Ok(json!({"results": results}))
}

fn host_result(item: &Value) -> Option<Value> {
    let mut result = item.as_object()?.clone();
    let torrent_url = text(item, "download_url")?;
    if !valid_mteam_url(&torrent_url) {
        return None;
    }
    let site_id = item.get("site_id")?.as_i64().filter(|id| *id > 0)?;
    let site_name = text(item, "site_name").unwrap_or_else(|| "M-Team".to_string());
    result.insert("plugin_key".into(), json!(torrent_url));
    result.insert("plugin_action_kind".into(), json!("download"));
    result.insert("plugin_action_label".into(), json!("下载"));
    result.insert("plugin_action_pending_label".into(), json!("下载中"));
    result.insert(
        "plugin_payload".into(),
        json!({
            "torrent_url": torrent_url,
            "site_id": site_id,
            "site_name": site_name,
        }),
    );
    Some(Value::Object(result))
}

async fn download(context: &PluginContext, payload: &Value) -> Result<Value, String> {
    let torrent_url = required_text(payload, "torrent_url")?;
    if !valid_mteam_url(&torrent_url) {
        return Err("只允许下载本次 mt-9kg 搜索返回的 M-Team 种子".to_string());
    }
    let site_id = payload
        .get("site_id")
        .and_then(Value::as_i64)
        .filter(|id| *id > 0)
        .ok_or_else(|| "下载参数缺少有效的 M-Team 站点 ID".to_string())?;
    let encoded_site_id = torrent_url
        .strip_prefix("mteam://")
        .and_then(|value| value.split('/').next())
        .and_then(|value| value.parse::<i64>().ok());
    if encoded_site_id != Some(site_id) {
        return Err("M-Team 下载链接与站点 ID 不匹配".to_string());
    }
    let site_name = text(payload, "site_name").unwrap_or_else(|| "M-Team".to_string());
    let request = json!({
        "torrent_url": torrent_url,
        "site_id": site_id,
        "site_name": site_name,
        "save_path": non_empty(&context.settings.save_path),
        "category": non_empty(&context.settings.category),
        "tags": ["mt-9kg"],
        "paused": context.settings.paused,
        "skip_download_tips": false,
    });
    let response = context
        .client
        .post(format!("{}/plugin/downloads", context.api_url))
        .bearer_auth(&context.token)
        .json(&request)
        .send()
        .await
        .map_err(|error| format!("提交 mt-9kg 下载失败: {error}"))?;
    let value = parse_response(response, "提交 mt-9kg 下载").await?;
    let downloader = text(&value, "downloader").unwrap_or_else(|| "下载器".to_string());
    Ok(json!({
        "notice": format!("已提交到 {downloader}"),
        "report": value,
    }))
}

async fn parse_response(response: Response, action: &str) -> Result<Value, String> {
    let status = response.status();
    let body = response
        .text()
        .await
        .map_err(|error| format!("读取 {action} 响应失败: {error}"))?;
    let value = serde_json::from_str::<Value>(&body)
        .map_err(|error| format!("解析 {action} 响应失败: {error}"))?;
    if status.is_success() {
        Ok(value)
    } else {
        let detail = text(&value, "error").unwrap_or_else(|| format!("HTTP {}", status.as_u16()));
        Err(format!("{action} 失败: {detail}"))
    }
}

fn read_payload() -> Result<Value, String> {
    let mut input = String::new();
    std::io::stdin()
        .read_to_string(&mut input)
        .map_err(|error| format!("读取动作参数失败: {error}"))?;
    serde_json::from_str(&input).map_err(|error| format!("动作参数不是有效 JSON: {error}"))
}

fn required_env(key: &str) -> Result<String, String> {
    env::var(key)
        .ok()
        .filter(|value| !value.trim().is_empty())
        .ok_or_else(|| format!("缺少 {key}"))
}

fn required_text(value: &Value, key: &str) -> Result<String, String> {
    text(value, key).ok_or_else(|| format!("{key} 不能为空"))
}

fn text(value: &Value, key: &str) -> Option<String> {
    value
        .get(key)
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_string)
}

fn non_empty(value: &str) -> Option<&str> {
    let value = value.trim();
    (!value.is_empty()).then_some(value)
}

fn valid_mteam_url(value: &str) -> bool {
    let Some(rest) = value.strip_prefix("mteam://") else {
        return false;
    };
    let mut parts = rest.split('/');
    matches!(
        (parts.next(), parts.next(), parts.next()),
        (Some(site_id), Some(torrent_id), None)
            if site_id.parse::<i64>().is_ok_and(|id| id > 0)
                && torrent_id.parse::<i64>().is_ok_and(|id| id > 0)
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_accepts_scoped_mteam_download_urls() {
        assert!(valid_mteam_url("mteam://7/12345"));
        assert!(!valid_mteam_url("https://m-team.cc/detail/12345"));
        assert!(!valid_mteam_url("mteam://0/12345"));
        assert!(!valid_mteam_url("mteam://7/12345/extra"));
    }

    #[test]
    fn host_results_preserve_torrent_fields_and_capture_real_site_identity() {
        let item = json!({
            "title": "Example",
            "site_id": 7,
            "site_name": "M-Team",
            "download_url": "mteam://7/12345",
            "seeders": 9,
            "leechers": 1,
            "parsed": {"title": "Example"}
        });
        let result = host_result(&item).unwrap();
        assert_eq!(result["title"], "Example");
        assert_eq!(result["seeders"], 9);
        assert_eq!(result["plugin_action_kind"], "download");
        assert_eq!(result["plugin_payload"]["site_id"], 7);
        assert_eq!(result["plugin_payload"]["torrent_url"], "mteam://7/12345");
    }

    #[test]
    fn host_results_reject_non_mteam_links() {
        assert!(
            host_result(&json!({
                "site_id": 7,
                "site_name": "M-Team",
                "download_url": "https://example.test/file.torrent"
            }))
            .is_none()
        );
    }
}
