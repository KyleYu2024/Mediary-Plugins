use crate::api::PluginContext;
use serde_json::{Value, json};

const ROUTE_CATALOG_SEARCH: &str = "/search/tmdb";
const ROUTE_SUBSCRIPTIONS: &str = "/subscriptions";
const ROUTE_DOWNLOADS: &str = "/downloads";
const ROUTE_SITES: &str = "/plugin/sites";
const ROUTE_DOWNLOADER: &str = "/plugin/downloader";
const ROUTE_TORRENTS: &str = "/plugin/downloader/torrents";
const ROUTE_ADD_DOWNLOAD: &str = "/plugin/downloads";
const ROUTE_CONTROL_DOWNLOADS: &str = "/plugin/downloader/torrents/control";
const ROUTE_LOGS: &str = "/logs";
const ROUTE_FILTER_RULES: &str = "/filter/rules";
const ROUTE_NOTIFICATIONS: &str = "/plugin/notifications";

pub(crate) async fn call_tool(
    ctx: &PluginContext,
    name: &str,
    args: &Value,
) -> Result<Value, String> {
    match name {
        "catalog_search" => catalog_search(ctx, args).await,
        "subscriptions_list" => subscriptions_list(ctx, args).await,
        "subscriptions_create" => subscriptions_create(ctx, args).await,
        "subscriptions_delete" => subscriptions_delete(ctx, args).await,
        "downloads_list" => downloads_list(ctx, args).await,
        "downloads_create" => downloads_create(ctx, args).await,
        "downloads_delete" => downloads_delete(ctx, args).await,
        "downloads_pause" => downloads_pause(ctx, args).await,
        "downloads_resume" => downloads_resume(ctx, args).await,
        "sites_list" => ctx.get(ROUTE_SITES).await,
        "downloader_status" => ctx.get(ROUTE_DOWNLOADER).await,
        "torrents_list" => torrents_list(ctx, args).await,
        "system_logs" => system_logs(ctx, args).await,
        "filters_list" => ctx.get(ROUTE_FILTER_RULES).await,
        "filters_create" => filters_create(ctx, args).await,
        "filters_update" => filters_update(ctx, args).await,
        "filters_delete" => filters_delete(ctx, args).await,
        "send_notification" => send_notification(ctx, args).await,
        _ => Err(format!("未知工具: {name}")),
    }
}

async fn catalog_search(ctx: &PluginContext, args: &Value) -> Result<Value, String> {
    let query = required_string(args, "query")?;
    let media_type = enum_string(args, "media_type", "multi", &["multi", "movie", "tv"])?;
    let limit = bounded_limit(args, 10, 100);
    let params = [
        ("query", query.to_string()),
        ("media_type", media_type.to_string()),
    ];
    truncate_array(ctx.get_query(ROUTE_CATALOG_SEARCH, &params).await?, limit)
}

async fn subscriptions_list(ctx: &PluginContext, args: &Value) -> Result<Value, String> {
    let mut params = vec![("limit", bounded_limit(args, 20, 1_000).to_string())];
    if let Some(tmdb_id) = optional_positive_i32(args, "tmdb_id")? {
        params.push(("tmdb_id", tmdb_id.to_string()));
    }
    if args.get("media_type").is_some() {
        params.push((
            "media_type",
            enum_string(args, "media_type", "movie", &["movie", "tv"])?.to_string(),
        ));
    }
    if let Some(season) = optional_positive_i32(args, "season")? {
        params.push(("season", season.to_string()));
    }
    ctx.get_query(ROUTE_SUBSCRIPTIONS, &params).await
}

async fn downloads_list(ctx: &PluginContext, args: &Value) -> Result<Value, String> {
    let status = enum_string(
        args,
        "status",
        "all",
        &[
            "all",
            "downloading",
            "paused",
            "completed",
            "failed",
            "organizing",
            "removed",
        ],
    )?;
    let limit = bounded_limit(args, 20, 200);
    filter_downloads(ctx.get(ROUTE_DOWNLOADS).await?, status, limit)
}

async fn downloads_create(ctx: &PluginContext, args: &Value) -> Result<Value, String> {
    let url = required_string(args, "url")?;
    let mut payload = json!({"torrent_url": url});
    for key in ["save_path", "category"] {
        if let Some(value) = optional_non_empty_string(args, key)? {
            payload[key] = json!(value);
        }
    }
    ctx.post(ROUTE_ADD_DOWNLOAD, payload).await
}

async fn subscriptions_create(ctx: &PluginContext, args: &Value) -> Result<Value, String> {
    let title = required_string(args, "title")?;
    let media_type = enum_string(args, "media_type", "movie", &["movie", "tv"])?;
    let tmdb_id = required_positive_i32(args, "tmdb_id")?;
    let year = optional_positive_i32(args, "year")?;
    let season = optional_positive_i32(args, "season")?;
    if media_type == "tv" && season.is_none() {
        return Err("电视剧订阅缺少 season 参数".to_string());
    }
    ctx.post(
        ROUTE_SUBSCRIPTIONS,
        subscription_payload(title, media_type, tmdb_id, year, season),
    )
    .await
}

async fn subscriptions_delete(ctx: &PluginContext, args: &Value) -> Result<Value, String> {
    let id = required_positive_i64(args, "id")?;
    ctx.delete(&format!("{ROUTE_SUBSCRIPTIONS}/{id}")).await
}

async fn torrents_list(ctx: &PluginContext, args: &Value) -> Result<Value, String> {
    let mut params = Vec::new();
    if args.get("downloader").is_some() {
        params.push((
            "downloader",
            enum_string(
                args,
                "downloader",
                "qbittorrent",
                &["qbittorrent", "transmission"],
            )?
            .to_string(),
        ));
    }
    let result = ctx.get_query(ROUTE_TORRENTS, &params).await?;
    truncate_items(result, bounded_limit(args, 20, 200))
}

async fn downloads_delete(ctx: &PluginContext, args: &Value) -> Result<Value, String> {
    let hash = required_string(args, "hash")?;
    let delete_files = optional_bool(args, "delete_files")?.unwrap_or(true);
    control_download(ctx, "delete", hash, Some(delete_files)).await
}

async fn downloads_pause(ctx: &PluginContext, args: &Value) -> Result<Value, String> {
    control_download(ctx, "pause", required_string(args, "hash")?, None).await
}

async fn downloads_resume(ctx: &PluginContext, args: &Value) -> Result<Value, String> {
    control_download(ctx, "resume", required_string(args, "hash")?, None).await
}

async fn system_logs(ctx: &PluginContext, args: &Value) -> Result<Value, String> {
    let scope = enum_string(
        args,
        "scope",
        "all",
        &[
            "general",
            "cloudhub_broadcast",
            "pt_scheduled_fetch",
            "plugin",
            "all",
        ],
    )?;
    let params = [
        ("limit", bounded_limit(args, 50, 800).to_string()),
        ("scope", scope.to_string()),
    ];
    ctx.get_query(ROUTE_LOGS, &params).await
}

async fn filters_create(ctx: &PluginContext, args: &Value) -> Result<Value, String> {
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

async fn filters_update(ctx: &PluginContext, args: &Value) -> Result<Value, String> {
    let id = required_string(args, "id")?;
    let mut rules = load_custom_rules(ctx).await?;
    let rule = rules
        .iter_mut()
        .find(|rule| rule.get("id").and_then(Value::as_str) == Some(id))
        .ok_or_else(|| format!("过滤规则不存在: {id}"))?;
    for key in ["name", "include", "exclude", "size_range", "seeders"] {
        if let Some(value) = args.get(key) {
            if key == "name" {
                let name = value
                    .as_str()
                    .map(str::trim)
                    .filter(|name| !name.is_empty())
                    .ok_or_else(|| "name 必须是非空字符串".to_string())?;
                rule[key] = json!(name);
                continue;
            }
            if !value.is_null() && !value.is_string() {
                return Err(format!("{key} 必须是字符串或 null"));
            }
            rule[key] = value.clone();
        }
    }
    save_custom_rules(ctx, rules).await
}

async fn filters_delete(ctx: &PluginContext, args: &Value) -> Result<Value, String> {
    let id = required_string(args, "id")?;
    let mut rules = load_custom_rules(ctx).await?;
    let previous_len = rules.len();
    rules.retain(|rule| rule.get("id").and_then(Value::as_str) != Some(id));
    if rules.len() == previous_len {
        return Err(format!("过滤规则不存在: {id}"));
    }
    save_custom_rules(ctx, rules).await
}

async fn send_notification(ctx: &PluginContext, args: &Value) -> Result<Value, String> {
    ctx.post(
        ROUTE_NOTIFICATIONS,
        json!({
            "title": required_string(args, "title")?,
            "content": required_string(args, "message")?
        }),
    )
    .await
}

async fn control_download(
    ctx: &PluginContext,
    action: &str,
    hash: &str,
    delete_files: Option<bool>,
) -> Result<Value, String> {
    ctx.post(
        ROUTE_CONTROL_DOWNLOADS,
        downloader_control_payload(action, hash, delete_files),
    )
    .await
}

async fn load_custom_rules(ctx: &PluginContext) -> Result<Vec<Value>, String> {
    ctx.get(ROUTE_FILTER_RULES)
        .await?
        .get("custom_filter_rules")
        .and_then(Value::as_array)
        .cloned()
        .ok_or_else(|| "Mediary 过滤规则响应缺少 custom_filter_rules".to_string())
}

async fn save_custom_rules(ctx: &PluginContext, rules: Vec<Value>) -> Result<Value, String> {
    ctx.post(ROUTE_FILTER_RULES, json!({"custom_filter_rules": rules}))
        .await
}

fn required_string<'a>(args: &'a Value, key: &str) -> Result<&'a str, String> {
    args.get(key)
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .ok_or_else(|| format!("{key} 必须是非空字符串"))
}

fn optional_non_empty_string<'a>(args: &'a Value, key: &str) -> Result<Option<&'a str>, String> {
    match args.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(value)) => Ok((!value.trim().is_empty()).then_some(value.trim())),
        Some(_) => Err(format!("{key} 必须是字符串")),
    }
}

fn optional_string(args: &Value, key: &str) -> Result<Option<String>, String> {
    match args.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(value)) => Ok(Some(value.trim().to_string())),
        Some(_) => Err(format!("{key} 必须是字符串或 null")),
    }
}

fn optional_bool(args: &Value, key: &str) -> Result<Option<bool>, String> {
    match args.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::Bool(value)) => Ok(Some(*value)),
        Some(_) => Err(format!("{key} 必须是布尔值")),
    }
}

fn required_positive_i64(args: &Value, key: &str) -> Result<i64, String> {
    args.get(key)
        .and_then(Value::as_i64)
        .filter(|value| *value > 0)
        .ok_or_else(|| format!("{key} 必须是正整数"))
}

fn required_positive_i32(args: &Value, key: &str) -> Result<i32, String> {
    required_positive_i64(args, key)
        .and_then(|value| i32::try_from(value).map_err(|_| format!("{key} 超出有效范围")))
}

fn optional_positive_i32(args: &Value, key: &str) -> Result<Option<i32>, String> {
    match args.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(_) => required_positive_i32(args, key).map(Some),
    }
}

fn enum_string<'a>(
    args: &'a Value,
    key: &str,
    default: &'a str,
    allowed: &[&str],
) -> Result<&'a str, String> {
    let value = match args.get(key) {
        None | Some(Value::Null) => default,
        Some(Value::String(value)) => value.as_str(),
        Some(_) => return Err(format!("{key} 必须是字符串")),
    };
    allowed
        .contains(&value)
        .then_some(value)
        .ok_or_else(|| format!("{key} 仅支持 {}", allowed.join("、")))
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

fn truncate_items(mut value: Value, limit: usize) -> Result<Value, String> {
    let items = value
        .get_mut("items")
        .and_then(Value::as_array_mut)
        .ok_or_else(|| "Mediary API 响应缺少 items 列表".to_string())?;
    items.truncate(limit);
    let count = items.len();
    value["count"] = json!(count);
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
    tmdb_id: i32,
    year: Option<i32>,
    season: Option<i32>,
) -> Value {
    let mut payload = json!({"name": title, "media_type": media_type, "tmdb_id": tmdb_id});
    if let Some(value) = year {
        payload["year"] = json!(value);
    }
    if let Some(value) = season {
        payload["season"] = json!(value);
    }
    payload
}

fn downloader_control_payload(action: &str, hash: &str, delete_files: Option<bool>) -> Value {
    let mut payload = json!({"action": action, "hashes": [hash]});
    if let Some(value) = delete_files {
        payload["delete_files"] = json!(value);
    }
    payload
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn uses_safe_current_core_routes() {
        assert_eq!(ROUTE_SITES, "/plugin/sites");
        assert_eq!(ROUTE_TORRENTS, "/plugin/downloader/torrents");
        assert_ne!(ROUTE_TORRENTS, "/plugin/torrents");
    }

    #[test]
    fn truncates_downloader_items_and_updates_count() {
        let result = truncate_items(json!({"count": 3, "items": [1, 2, 3]}), 2).unwrap();
        assert_eq!(result, json!({"count": 2, "items": [1, 2]}));
    }

    #[test]
    fn validates_enums_and_integer_ranges() {
        assert!(enum_string(&json!({"status": "mystery"}), "status", "all", &["all"]).is_err());
        assert!(required_positive_i32(&json!({"id": i64::MAX}), "id").is_err());
        assert!(optional_bool(&json!({"enabled": "true"}), "enabled").is_err());
    }

    #[test]
    fn builds_core_payload_field_names() {
        let subscription = subscription_payload("龙族", "tv", 125_988, Some(2024), Some(2));
        assert_eq!(subscription["name"], "龙族");
        assert!(subscription.get("title").is_none());
        let control = downloader_control_payload("delete", "abc123", Some(false));
        assert_eq!(control["hashes"], json!(["abc123"]));
        assert_eq!(control["delete_files"], false);
    }
}
