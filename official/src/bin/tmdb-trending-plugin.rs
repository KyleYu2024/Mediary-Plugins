use reqwest::Client;
use serde::Deserialize;
use serde_json::{Map, Value, json};
use std::{collections::HashMap, env, time::Duration};

const POSTER_BASE_URL: &str = "https://image.tmdb.org/t/p/w500";

struct PluginContext {
    api_url: String,
    token: String,
    client: Client,
}

#[derive(Clone, Deserialize)]
struct BrowseRequest {
    #[serde(default = "default_media_type")]
    media_type: String,
    #[serde(default = "default_time_window")]
    time_window: String,
    #[serde(default = "default_page")]
    page: i32,
}

#[derive(Deserialize)]
struct SubscribeRequest {
    tmdb_id: i32,
    media_type: String,
    #[serde(default = "default_media_type")]
    browse_media_type: String,
    #[serde(default = "default_time_window")]
    time_window: String,
    #[serde(default = "default_page")]
    page: i32,
}

#[derive(Deserialize)]
struct TrendingPage {
    #[serde(default)]
    page: i32,
    #[serde(default)]
    total_pages: i32,
    #[serde(default)]
    items: Vec<TmdbMedia>,
}

#[derive(Clone, Deserialize)]
struct TmdbMedia {
    id: i32,
    name: Option<String>,
    title: Option<String>,
    first_air_date: Option<String>,
    release_date: Option<String>,
    poster_path: Option<String>,
    vote_average: Option<f64>,
    media_type: Option<String>,
}

#[derive(Default, Deserialize)]
struct MediaStatuses {
    #[serde(default)]
    statuses: HashMap<String, MediaStatus>,
}

#[derive(Clone, Default, Deserialize)]
struct MediaStatus {
    #[serde(default)]
    is_subscribed: bool,
    #[serde(default)]
    is_fully_collected: bool,
    #[serde(default)]
    is_partially_collected: bool,
}

#[derive(Deserialize)]
struct TmdbDetailsResponse {
    details: TmdbDetails,
    title: Option<String>,
    year: Option<i32>,
}

#[derive(Deserialize)]
struct TmdbDetails {
    name: Option<String>,
    title: Option<String>,
    poster_path: Option<String>,
    backdrop_path: Option<String>,
    vote_average: Option<f64>,
    overview: Option<String>,
    #[serde(default)]
    seasons: Vec<TmdbSeason>,
}

#[derive(Deserialize)]
struct TmdbSeason {
    season_number: i32,
    episode_count: i32,
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
        "browse" => {
            let request = serde_json::from_value::<BrowseRequest>(payload)
                .map_err(|error| format!("热门浏览参数无效: {error}"))?;
            browse(&context, request, None).await?
        }
        "subscribe" => {
            let request = serde_json::from_value::<SubscribeRequest>(payload)
                .map_err(|error| format!("订阅参数无效: {error}"))?;
            subscribe(&context, request).await?
        }
        _ => return Err(format!("不支持的 TMDB 热门动作: {action}")),
    };
    println!("{output}");
    Ok(())
}

impl PluginContext {
    fn from_env() -> Result<Self, String> {
        let api_url = required_env("MEDIARY_PLUGIN_API_URL")?;
        let token = required_env("MEDIARY_PLUGIN_TOKEN")?;
        let client = Client::builder()
            .timeout(Duration::from_secs(35))
            .build()
            .map_err(|error| error.to_string())?;
        Ok(Self {
            api_url: api_url.trim_end_matches('/').to_string(),
            token,
            client,
        })
    }
}

async fn browse(
    context: &PluginContext,
    request: BrowseRequest,
    notice: Option<String>,
) -> Result<Value, String> {
    let media_type = normalize_media_type(&request.media_type);
    let time_window = normalize_time_window(&request.time_window);
    let page = request.page.clamp(1, 500);
    let trending = get_json(
        context,
        "/plugin/tmdb/trending",
        &[
            ("media_type", media_type.to_string()),
            ("time_window", time_window.to_string()),
            ("page", page.to_string()),
        ],
    )
    .await?;
    let trending = serde_json::from_value::<TrendingPage>(trending)
        .map_err(|error| format!("TMDB 热门响应格式无效: {error}"))?;
    let status_keys = trending
        .items
        .iter()
        .filter_map(media_identity)
        .collect::<Vec<_>>();
    let statuses = if status_keys.is_empty() {
        MediaStatuses::default()
    } else {
        let payload = get_json(
            context,
            "/plugin/media-status",
            &[("items", status_keys.join(","))],
        )
        .await?;
        serde_json::from_value::<MediaStatuses>(payload)
            .map_err(|error| format!("媒体状态响应格式无效: {error}"))?
    };
    let current_page = trending.page.max(page);
    let total_pages = trending.total_pages.max(current_page);
    let items = trending
        .items
        .iter()
        .filter_map(|media| {
            let identity = media_identity(media)?;
            Some(media_item(
                media,
                statuses
                    .statuses
                    .get(&identity)
                    .cloned()
                    .unwrap_or_default(),
                media_type,
                time_window,
                current_page,
            ))
        })
        .collect::<Vec<_>>();
    let actions = pagination_actions(media_type, time_window, current_page, total_pages);
    Ok(json!({
        "notice": notice.unwrap_or_else(|| format!("第 {current_page} / {total_pages} 页")),
        "items": items,
        "actions": actions,
    }))
}

async fn subscribe(context: &PluginContext, request: SubscribeRequest) -> Result<Value, String> {
    let media_type = normalize_item_media_type(&request.media_type)
        .ok_or_else(|| "media_type 仅支持 movie 或 tv".to_string())?;
    let status_key = format!("{media_type}:{}", request.tmdb_id);
    let current_status = get_json(
        context,
        "/plugin/media-status",
        &[("items", status_key.clone())],
    )
    .await?;
    let statuses = serde_json::from_value::<MediaStatuses>(current_status)
        .map_err(|error| format!("媒体状态响应格式无效: {error}"))?;
    if statuses
        .statuses
        .get(&status_key)
        .is_some_and(|status| status.is_subscribed || status.is_fully_collected)
    {
        return browse(
            context,
            browse_request(&request),
            Some("该条目已订阅或已入库。".to_string()),
        )
        .await;
    }

    let details = get_json(
        context,
        "/search/tmdb/details",
        &[
            ("id", request.tmdb_id.to_string()),
            ("media_type", media_type.to_string()),
        ],
    )
    .await?;
    let details = serde_json::from_value::<TmdbDetailsResponse>(details)
        .map_err(|error| format!("TMDB 详情响应格式无效: {error}"))?;
    let title = details
        .title
        .clone()
        .or_else(|| details.details.title.clone())
        .or_else(|| details.details.name.clone())
        .filter(|value| !value.trim().is_empty())
        .unwrap_or_else(|| format!("TMDB {}", request.tmdb_id));
    let expected_episodes = if media_type == "tv" {
        details
            .details
            .seasons
            .iter()
            .find(|season| season.season_number == 1)
            .map(|season| season.episode_count)
            .filter(|value| *value > 0)
    } else {
        Some(1)
    };
    let body = json!({
        "tmdb_id": request.tmdb_id,
        "name": title,
        "year": details.year,
        "media_type": media_type,
        "season": (media_type == "tv").then_some(1),
        "season_start_episode": (media_type == "tv").then_some(1),
        "expected_episodes": expected_episodes,
        "poster_path": details.details.poster_path,
        "backdrop_path": details.details.backdrop_path,
        "vote_average": details.details.vote_average,
        "description": details.details.overview,
    });
    post_json(context, "/subscriptions", body).await?;
    browse(
        context,
        browse_request(&request),
        Some(format!("《{title}》已加入订阅。")),
    )
    .await
}

fn media_item(
    media: &TmdbMedia,
    status: MediaStatus,
    browse_media_type: &str,
    time_window: &str,
    page: i32,
) -> Value {
    let media_type = media
        .media_type
        .as_deref()
        .and_then(normalize_item_media_type)
        .unwrap_or("movie");
    let title = media
        .title
        .as_deref()
        .or(media.name.as_deref())
        .unwrap_or("未命名条目");
    let date = if media_type == "tv" {
        media.first_air_date.as_deref()
    } else {
        media.release_date.as_deref()
    };
    let year = date
        .filter(|value| value.len() >= 4)
        .map(|value| &value[..4])
        .unwrap_or("");
    let mut badges = vec![json!({
        "label": if media_type == "tv" { "剧集" } else { "电影" },
        "tone": if media_type == "tv" { "info" } else { "neutral" },
    })];
    if let Some(label) = status_label(&status) {
        badges.push(json!({
            "label": label,
            "tone": status_tone(&status),
        }));
    }
    if let Some(score) = media.vote_average.filter(|score| *score > 0.0) {
        badges.push(json!({"label": format!("{score:.1}"), "tone": "warning"}));
    }
    let actions = if status.is_subscribed || status.is_fully_collected {
        Vec::new()
    } else {
        vec![json!({
            "type": "plugin_action",
            "label": "订阅",
            "pending_label": "订阅中",
            "icon": "plus",
            "tone": "success",
            "action": "subscribe",
            "payload": {
                "tmdb_id": media.id,
                "media_type": media_type,
                "browse_media_type": browse_media_type,
                "time_window": time_window,
                "page": page,
            },
            "error_message": "创建订阅失败。",
        })]
    };
    json!({
        "key": format!("{media_type}:{}", media.id),
        "title": title,
        "image_url": media.poster_path.as_deref().map(|path| format!("{POSTER_BASE_URL}{}", normalized_path(path))),
        "image_alt": format!("{title}海报"),
        "badges": badges,
        "metadata": if year.is_empty() { Vec::<Value>::new() } else { vec![json!(year)] },
        "actions": actions,
    })
}

fn pagination_actions(
    media_type: &str,
    time_window: &str,
    page: i32,
    total_pages: i32,
) -> Vec<Value> {
    let mut actions = Vec::new();
    if page > 1 {
        actions.push(page_action(
            "上一页",
            "arrow-left",
            media_type,
            time_window,
            page - 1,
        ));
    }
    if page < total_pages {
        actions.push(page_action(
            "下一页",
            "arrow-right",
            media_type,
            time_window,
            page + 1,
        ));
    }
    actions
}

fn page_action(label: &str, icon: &str, media_type: &str, time_window: &str, page: i32) -> Value {
    json!({
        "type": "plugin_action",
        "label": label,
        "pending_label": "加载中",
        "icon": icon,
        "action": "browse",
        "payload": {
            "media_type": media_type,
            "time_window": time_window,
            "page": page,
        }
    })
}

fn browse_request(request: &SubscribeRequest) -> BrowseRequest {
    BrowseRequest {
        media_type: request.browse_media_type.clone(),
        time_window: request.time_window.clone(),
        page: request.page,
    }
}

fn status_label(status: &MediaStatus) -> Option<&'static str> {
    if status.is_fully_collected {
        Some("已入库")
    } else if status.is_subscribed {
        Some("订阅中")
    } else if status.is_partially_collected {
        Some("部分入库")
    } else {
        None
    }
}

fn status_tone(status: &MediaStatus) -> &'static str {
    if status.is_fully_collected {
        "success"
    } else if status.is_subscribed {
        "info"
    } else {
        "warning"
    }
}

fn media_identity(media: &TmdbMedia) -> Option<String> {
    let media_type = media
        .media_type
        .as_deref()
        .and_then(normalize_item_media_type)?;
    (media.id > 0).then(|| format!("{media_type}:{}", media.id))
}

fn normalize_media_type(value: &str) -> &'static str {
    match value.trim() {
        "movie" => "movie",
        "tv" => "tv",
        _ => "all",
    }
}

fn normalize_item_media_type(value: &str) -> Option<&'static str> {
    match value.trim() {
        "movie" => Some("movie"),
        "tv" => Some("tv"),
        _ => None,
    }
}

fn normalize_time_window(value: &str) -> &'static str {
    if value.trim() == "week" {
        "week"
    } else {
        "day"
    }
}

fn normalized_path(path: &str) -> String {
    if path.starts_with('/') {
        path.to_string()
    } else {
        format!("/{path}")
    }
}

async fn get_json(
    context: &PluginContext,
    path: &str,
    query: &[(&str, String)],
) -> Result<Value, String> {
    let response = context
        .client
        .get(format!("{}{}", context.api_url, path))
        .query(query)
        .bearer_auth(&context.token)
        .send()
        .await
        .map_err(|error| error.to_string())?;
    parse_response(response).await
}

async fn post_json(context: &PluginContext, path: &str, body: Value) -> Result<Value, String> {
    let response = context
        .client
        .post(format!("{}{}", context.api_url, path))
        .bearer_auth(&context.token)
        .json(&body)
        .send()
        .await
        .map_err(|error| error.to_string())?;
    parse_response(response).await
}

async fn parse_response(response: reqwest::Response) -> Result<Value, String> {
    let status = response.status();
    let body = response.text().await.unwrap_or_default();
    if !status.is_success() {
        let message = serde_json::from_str::<Value>(&body)
            .ok()
            .and_then(|value| {
                value
                    .get("error")
                    .or_else(|| value.get("message"))
                    .and_then(Value::as_str)
                    .map(ToString::to_string)
            })
            .unwrap_or_else(|| truncate(&body, 220));
        return Err(format!("Mediary API {status}: {message}"));
    }
    serde_json::from_str(&body).map_err(|error| error.to_string())
}

fn read_payload() -> Result<Value, String> {
    let input = std::io::read_to_string(std::io::stdin())
        .map_err(|error| format!("读取动作参数失败: {error}"))?;
    if input.trim().is_empty() {
        return Ok(Value::Object(Map::new()));
    }
    serde_json::from_str(&input).map_err(|error| format!("动作参数不是有效 JSON: {error}"))
}

fn required_env(name: &str) -> Result<String, String> {
    env::var(name).map_err(|_| format!("缺少运行时环境变量 {name}"))
}

fn default_media_type() -> String {
    "all".to_string()
}

fn default_time_window() -> String {
    "day".to_string()
}

fn default_page() -> i32 {
    1
}

fn truncate(value: &str, max_chars: usize) -> String {
    let text = value.chars().take(max_chars).collect::<String>();
    if value.chars().count() > max_chars {
        format!("{text}...")
    } else {
        text
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn status_labels_follow_library_subscription_partial_order() {
        assert_eq!(
            status_label(&MediaStatus {
                is_subscribed: true,
                is_fully_collected: true,
                is_partially_collected: true,
            }),
            Some("已入库")
        );
        assert_eq!(
            status_label(&MediaStatus {
                is_subscribed: true,
                ..MediaStatus::default()
            }),
            Some("订阅中")
        );
    }

    #[test]
    fn pagination_stays_inside_available_pages() {
        assert_eq!(pagination_actions("all", "day", 1, 3).len(), 1);
        assert_eq!(pagination_actions("all", "day", 2, 3).len(), 2);
        assert_eq!(pagination_actions("all", "day", 3, 3).len(), 1);
    }

    #[test]
    fn normalizes_supported_browse_values() {
        assert_eq!(normalize_media_type("movie"), "movie");
        assert_eq!(normalize_media_type("person"), "all");
        assert_eq!(normalize_time_window("week"), "week");
        assert_eq!(normalize_time_window("month"), "day");
    }
}
