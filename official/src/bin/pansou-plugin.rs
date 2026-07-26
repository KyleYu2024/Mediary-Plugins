use reqwest::header::{AUTHORIZATION, CONTENT_TYPE, HeaderMap, HeaderValue};
use serde_json::{Map, Value, json};
use std::{collections::HashSet, env, io::Read, time::Duration};

const MAX_RESULTS: usize = 500;
const MAX_RESULTS_JSON_BYTES: usize = 1536 * 1024;

#[tokio::main]
async fn main() {
    if let Err(error) = run().await {
        eprintln!("{error}");
        std::process::exit(1);
    }
}

async fn run() -> Result<(), String> {
    let action = env::var("MEDIARY_PLUGIN_ACTION").unwrap_or_default();
    let settings = env::var("MEDIARY_PLUGIN_SETTINGS_JSON")
        .ok()
        .and_then(|value| serde_json::from_str::<Map<String, Value>>(&value).ok())
        .unwrap_or_default();
    let mut input = String::new();
    std::io::stdin()
        .read_to_string(&mut input)
        .map_err(|error| format!("读取动作参数失败: {error}"))?;
    let payload: Value =
        serde_json::from_str(&input).map_err(|error| format!("动作参数不是有效 JSON: {error}"))?;

    match action.as_str() {
        "search" => {
            let value = search(&settings, &payload).await?;
            println!("{value}");
            Ok(())
        }
        _ => Err(format!("不支持的盘搜动作: {action}")),
    }
}

async fn search(settings: &Map<String, Value>, payload: &Value) -> Result<Value, String> {
    let base_url = setting(settings, "base_url")
        .trim()
        .trim_end_matches('/')
        .to_string();
    if base_url.is_empty() {
        return Err("请先配置 Pansou 地址".to_string());
    }
    let query = payload
        .get("query")
        .and_then(Value::as_str)
        .unwrap_or("")
        .trim();
    if query.is_empty() {
        return Err("搜索关键词不能为空".to_string());
    }

    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(35))
        .build()
        .map_err(|error| format!("创建 Pansou 客户端失败: {error}"))?;
    let mut headers = HeaderMap::new();
    headers.insert(CONTENT_TYPE, HeaderValue::from_static("application/json"));
    let username = setting(settings, "username");
    let password = setting(settings, "password");
    if !username.trim().is_empty()
        && !password.trim().is_empty()
        && let Ok(token) = login(&client, &base_url, username, password).await
        && !token.is_empty()
        && let Ok(value) = HeaderValue::from_str(&format!("Bearer {token}"))
    {
        headers.insert(AUTHORIZATION, value);
    }

    let response = client
        .post(format!("{base_url}/api/search"))
        .headers(headers)
        .json(&json!({"kw": query, "res": "merge"}))
        .send()
        .await
        .map_err(|error| format!("请求 Pansou 失败: {error}"))?;
    let status = response.status();
    let body = response
        .text()
        .await
        .map_err(|error| format!("读取 Pansou 响应失败: {error}"))?;
    if !status.is_success() {
        return Err(format!("Pansou 返回 HTTP {}", status.as_u16()));
    }
    let value: Value =
        serde_json::from_str(&body).map_err(|error| format!("解析 Pansou 响应失败: {error}"))?;
    let (items, truncated) = normalize_results(&value);
    if truncated {
        Ok(json!({
            "notice": format!("搜索结果较多，已显示前 {} 条。", items.len()),
            "items": items
        }))
    } else {
        Ok(json!({"items": items}))
    }
}

async fn login(
    client: &reqwest::Client,
    base_url: &str,
    username: &str,
    password: &str,
) -> Result<String, String> {
    let value = client
        .post(format!("{base_url}/api/auth/login"))
        .json(&json!({"username": username, "password": password}))
        .send()
        .await
        .map_err(|error| error.to_string())?
        .error_for_status()
        .map_err(|error| error.to_string())?
        .json::<Value>()
        .await
        .map_err(|error| error.to_string())?;
    Ok(find_text(&value, &["token", "access_token"]).unwrap_or_default())
}

fn normalize_results(value: &Value) -> (Vec<Value>, bool) {
    let Some(merged) = value
        .get("data")
        .and_then(|data| data.get("merged_by_type"))
        .or_else(|| value.get("merged_by_type"))
        .and_then(Value::as_object)
    else {
        return (Vec::new(), false);
    };
    let mut results = Vec::new();
    let mut seen = HashSet::new();
    let mut serialized_bytes = 0;
    let mut truncated = false;
    'groups: for items in merged.values() {
        let Some(items) = items.as_array() else {
            continue;
        };
        for item in items {
            if results.len() >= MAX_RESULTS {
                truncated = true;
                break 'groups;
            }
            let link = find_text(item, &["url", "link"]).unwrap_or_default();
            let Some(disk_type) = classify_link(&link) else {
                continue;
            };
            let dedupe_key = link_dedupe_key(&link, disk_type);
            if !seen.insert(dedupe_key) {
                continue;
            }
            let link = if disk_type == "115" {
                append_password(link, find_text(item, &["password"]).unwrap_or_default())
            } else {
                link
            };
            let title = find_text(item, &["note", "title"]).unwrap_or_else(|| "未命名资源".into());
            let metadata = [
                find_text(item, &["size"]).unwrap_or_else(|| "未知".into()),
                find_text(item, &["source", "remark"]).unwrap_or_default(),
                find_text(item, &["datetime", "date"]).unwrap_or_default(),
            ]
            .into_iter()
            .filter(|value| !value.trim().is_empty())
            .collect::<Vec<_>>();
            let (
                badge_label,
                badge_tone,
                submit_label,
                pending_label,
                submit_icon,
                action_tone,
                success_message,
                error_message,
            ) = match disk_type {
                "115" => (
                    "115",
                    "success",
                    "转存整理",
                    "转存中",
                    "folder-input",
                    "success",
                    "已提交转存整理。",
                    "转存提交失败，请检查 FlowLink 配置。",
                ),
                "ed2k" => (
                    "ed2k",
                    "info",
                    "离线下载",
                    "下载中",
                    "download",
                    "info",
                    "已提交到 115 离线下载。",
                    "离线提交失败，请检查 115 Web Cookie 和离线下载目录配置。",
                ),
                _ => (
                    "磁力",
                    "warning",
                    "离线下载",
                    "下载中",
                    "download",
                    "info",
                    "已提交到 115 离线下载。",
                    "离线提交失败，请检查 115 Web Cookie 和离线下载目录配置。",
                ),
            };
            let result = json!({
                "key": link,
                "title": title,
                "badges": [{
                    "label": badge_label,
                    "tone": badge_tone
                }],
                "metadata": metadata,
                "actions": [{
                    "type": "copy",
                    "label": "复制链接",
                    "icon": "copy",
                    "icon_only": true,
                    "value": link,
                    "success_message": "资源链接已复制。",
                    "error_message": "复制失败，请检查浏览器剪贴板权限。"
                }, {
                    "type": "link_submit",
                    "label": submit_label,
                    "pending_label": pending_label,
                    "icon": submit_icon,
                    "tone": action_tone,
                    "value": link,
                    "success_message": success_message,
                    "error_message": error_message
                }]
            });
            let result_bytes = serde_json::to_vec(&result)
                .map(|value| value.len() + 1)
                .unwrap_or(MAX_RESULTS_JSON_BYTES);
            if serialized_bytes + result_bytes > MAX_RESULTS_JSON_BYTES {
                truncated = true;
                break 'groups;
            }
            serialized_bytes += result_bytes;
            results.push(result);
        }
    }
    (results, truncated)
}

fn classify_link(link: &str) -> Option<&'static str> {
    let link = link.trim();
    let lower = link.to_ascii_lowercase();
    if lower.starts_with("ed2k://") {
        return Some("ed2k");
    }
    if lower.starts_with("magnet:?") {
        return Some("magnet");
    }

    let candidate = link.split_whitespace().next().unwrap_or_default();
    let url = reqwest::Url::parse(candidate).ok()?;
    if !matches!(url.scheme(), "http" | "https") || !url.path().starts_with("/s/") {
        return None;
    }
    match url.host_str()?.to_ascii_lowercase().as_str() {
        "115.com" | "115cdn.com" | "115v.com" => Some("115"),
        _ => None,
    }
}

fn link_dedupe_key(link: &str, disk_type: &str) -> String {
    let link = link.trim();
    if disk_type == "115" {
        link.split_whitespace()
            .next()
            .unwrap_or(link)
            .to_ascii_lowercase()
    } else {
        link.to_ascii_lowercase()
    }
}

fn append_password(link: String, password: String) -> String {
    let password = password.trim();
    let lower = link.to_ascii_lowercase();
    if password.is_empty() || lower.contains("password=") || link.contains("提取码") {
        link
    } else {
        format!("{link} 提取码: {password}")
    }
}

fn find_text(value: &Value, keys: &[&str]) -> Option<String> {
    for key in keys {
        if let Some(text) = value.get(key).and_then(Value::as_str) {
            let text = text.trim();
            if !text.is_empty() {
                return Some(text.to_string());
            }
        }
    }
    value
        .as_object()
        .into_iter()
        .flat_map(|object| object.values())
        .find_map(|child| find_text(child, keys))
}

fn setting<'a>(settings: &'a Map<String, Value>, key: &str) -> &'a str {
    settings.get(key).and_then(Value::as_str).unwrap_or("")
}

#[cfg(test)]
mod tests {
    use super::{MAX_RESULTS, append_password, classify_link, normalize_results};
    use serde_json::json;

    #[test]
    fn classifies_supported_links_by_actual_url() {
        let value = json!({
            "data": {
                "merged_by_type": {
                    "115": [{
                        "url": "https://115.com/s/example",
                        "password": "1234",
                        "note": "115 电影",
                        "size": "10 GB",
                        "source": "source-a"
                    }],
                    "magnet": [{
                        "link": "magnet:?xt=urn:btih:ABC",
                        "title": "磁力电影",
                        "date": "2026-07-25"
                    }, {
                        "link": "https://115cdn.com/s/misgrouped",
                        "password": "5678",
                        "title": "分组错误的 115 资源"
                    }],
                    "ed2k": [{
                        "link": "ed2k://|file|%5B%E9%98%BF%E5%87%A1%E8%BE%BE%5D.Avatar.mkv|1|ABC|/",
                        "title": "阿凡达"
                    }],
                    "aliyun": [{
                        "url": "https://example.com/ignored"
                    }]
                }
            }
        });

        let (items, truncated) = normalize_results(&value);
        assert!(!truncated);
        assert_eq!(items.len(), 4);
        assert!(items.iter().any(|item| item["badges"][0]["label"] == "115"
            && item["actions"][1]["value"] == "https://115.com/s/example 提取码: 1234"));
        assert!(items.iter().any(|item| {
            item["badges"][0]["label"] == "115"
                && item["actions"][1]["value"] == "https://115cdn.com/s/misgrouped 提取码: 5678"
        }));
        assert!(items.iter().any(|item| {
            item["badges"][0]["label"] == "磁力"
                && item["actions"][1]["value"] == "magnet:?xt=urn:btih:ABC"
        }));
        assert!(items.iter().any(|item| {
            item["badges"][0]["label"] == "ed2k"
                && item["actions"][1]["value"]
                    == "ed2k://|file|%5B%E9%98%BF%E5%87%A1%E8%BE%BE%5D.Avatar.mkv|1|ABC|/"
        }));
    }

    #[test]
    fn repairs_wrong_groups_and_deduplicates_links() {
        let value = json!({
            "data": {
                "merged_by_type": {
                    "115": [{
                        "url": "ed2k://|file|movie.mkv|1|ABC|/",
                        "note": "错误放在 115 分组"
                    }],
                    "magnet": [{
                        "url": "ed2k://|file|movie.mkv|1|ABC|/",
                        "note": "重复结果"
                    }, {
                        "url": "https://example.com/not-supported",
                        "note": "不支持的链接"
                    }]
                }
            }
        });

        let (items, truncated) = normalize_results(&value);
        assert!(!truncated);
        assert_eq!(items.len(), 1);
        assert_eq!(items[0]["badges"][0]["label"], "ed2k");
        assert_eq!(
            items[0]["actions"][1]["value"],
            "ed2k://|file|movie.mkv|1|ABC|/"
        );
    }

    #[test]
    fn recognizes_only_supported_link_shapes() {
        assert_eq!(
            classify_link("ed2k://|file|movie.mkv|1|ABC|/"),
            Some("ed2k")
        );
        assert_eq!(classify_link("MAGNET:?xt=urn:btih:ABC"), Some("magnet"));
        assert_eq!(
            classify_link("https://115v.com/s/example 提取码: 1234"),
            Some("115")
        );
        assert_eq!(classify_link("https://example.com/115.com/s/fake"), None);
        assert_eq!(classify_link("https://115.com/not-a-share"), None);
    }

    #[test]
    fn does_not_append_duplicate_share_password() {
        let link = "https://115.com/s/example?password=1234".to_string();
        assert_eq!(append_password(link.clone(), "5678".into()), link);
        let uppercase = "https://115.com/s/example?PASSWORD=1234".to_string();
        assert_eq!(append_password(uppercase.clone(), "5678".into()), uppercase);
    }

    #[test]
    fn caps_large_result_sets_below_the_host_output_limit() {
        let source_items = (0..MAX_RESULTS + 100)
            .map(|index| {
                json!({
                    "link": format!("magnet:?xt=urn:btih:{index:040x}"),
                    "title": format!("测试资源 {index}")
                })
            })
            .collect::<Vec<_>>();
        let value = json!({"merged_by_type": {"magnet": source_items}});

        let (items, truncated) = normalize_results(&value);
        let response = json!({"notice": "搜索结果较多，已截断。", "items": items});

        assert!(truncated);
        assert_eq!(response["items"].as_array().unwrap().len(), MAX_RESULTS);
        assert!(serde_json::to_vec(&response).unwrap().len() < 2 * 1024 * 1024);
    }
}
