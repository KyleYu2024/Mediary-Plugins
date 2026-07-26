use reqwest::Client;
use serde::Deserialize;
use serde_json::{Map, Value, json};
use std::{collections::HashMap, env, time::Duration};

const POSTER_BASE_URL: &str = "https://image.tmdb.org/t/p/w500";
const CATEGORY_PAGE_SIZE: usize = 18;
const CATEGORIES: [(&str, &str); 5] = [
    ("domestic_tv", "国内电视剧"),
    ("foreign_tv", "国外电视剧"),
    ("domestic_movie", "国内电影"),
    ("foreign_movie", "国外电影"),
    ("anime", "动漫"),
];
const ADULT_TEXT_PATTERNS: [&str; 47] = [
    "成人",
    "情色",
    "色情",
    "伦理片",
    "伦理",
    "三级",
    "限制级",
    "禁片",
    "性爱",
    "性欲",
    "性奴",
    "性侵",
    "强奸",
    "强暴",
    "轮奸",
    "调教",
    "情欲",
    "人妻",
    "偷情",
    "不伦",
    "乱伦",
    "继母",
    "嫂子",
    "小姨子",
    "av女",
    "av女优",
    "adult movie",
    "jav",
    "hentai",
    "erotic",
    "erotica",
    "porn",
    "porno",
    "pornographic",
    "xxx",
    "x-rated",
    "milf",
    "mommy",
    "stepmom",
    "stepmother",
    "hotwife",
    "escort",
    "fetish",
    "bdsm",
    "blowjob",
    "orgy",
    "incest",
];

struct PluginContext {
    api_url: String,
    token: String,
    client: Client,
}

#[derive(Deserialize)]
struct DiscoverResponse {
    #[serde(default)]
    categories: HashMap<String, Vec<TmdbMedia>>,
    #[serde(default)]
    cache: DiscoverCache,
}

#[derive(Default, Deserialize)]
struct DiscoverCache {
    #[serde(default)]
    saved_at: u64,
    #[serde(default)]
    refreshing: bool,
    #[serde(default)]
    refresh_failed: bool,
}

#[derive(Clone, Default, Deserialize)]
struct TmdbMedia {
    id: i32,
    #[serde(default)]
    adult: bool,
    name: Option<String>,
    title: Option<String>,
    original_name: Option<String>,
    original_title: Option<String>,
    first_air_date: Option<String>,
    release_date: Option<String>,
    poster_path: Option<String>,
    vote_average: Option<f64>,
    media_type: Option<String>,
    overview: Option<String>,
    #[serde(default)]
    genre_ids: Vec<i32>,
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
    let _payload = read_payload()?;
    let output = match action.as_str() {
        "browse" => browse(&context).await?,
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
            .timeout(Duration::from_secs(43))
            .build()
            .map_err(|error| error.to_string())?;
        Ok(Self {
            api_url: api_url.trim_end_matches('/').to_string(),
            token,
            client,
        })
    }
}

async fn browse(context: &PluginContext) -> Result<Value, String> {
    let response = get_json(context, "/plugin/tmdb/discover", &[]).await?;
    let discover = serde_json::from_value::<DiscoverResponse>(response)
        .map_err(|error| format!("TMDB 热门响应格式无效: {error}"))?;
    let category_items = CATEGORIES
        .iter()
        .map(|(category, title)| {
            let mut items = discover
                .categories
                .get(*category)
                .into_iter()
                .flatten()
                .filter(|media| is_safe_category_card(media, category))
                .take(CATEGORY_PAGE_SIZE)
                .collect::<Vec<_>>();
            items.truncate((items.len() / 6) * 6);
            (*category, *title, items)
        })
        .collect::<Vec<_>>();
    let status_keys = category_items
        .iter()
        .flat_map(|(_, _, items)| items.iter())
        .filter_map(|media| media_identity(media))
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
    let items = category_items
        .into_iter()
        .flat_map(|(_, section, items)| {
            items.into_iter().filter_map(|media| {
                let identity = media_identity(media)?;
                Some(media_item(
                    media,
                    section,
                    statuses
                        .statuses
                        .get(&identity)
                        .cloned()
                        .unwrap_or_default(),
                ))
            })
        })
        .collect::<Vec<_>>();
    Ok(json!({
        "items": items,
        "refreshing": discover.cache.refreshing,
        "refresh_failed": discover.cache.refresh_failed,
        "refresh_after_ms": discover.cache.refreshing.then_some(2000),
        "cache_saved_at": discover.cache.saved_at,
    }))
}

fn media_item(media: &TmdbMedia, section: &str, status: MediaStatus) -> Value {
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
            "type": "subscription_create",
            "label": "订阅",
            "icon": "plus",
            "tone": "success",
            "payload": {
                "tmdb_id": media.id,
                "media_type": media_type,
                "title": title,
                "poster_path": media.poster_path,
                "release_date": media.release_date,
                "first_air_date": media.first_air_date,
                "vote_average": media.vote_average,
            },
            "error_message": "无法打开订阅设置。",
        })]
    };
    json!({
        "key": format!("{media_type}:{}", media.id),
        "section": section,
        "title": title,
        "image_url": media.poster_path.as_deref().map(|path| format!("{POSTER_BASE_URL}{}", normalized_path(path))),
        "image_alt": format!("{title}海报"),
        "badges": badges,
        "metadata": if year.is_empty() { Vec::<Value>::new() } else { vec![json!(year)] },
        "actions": actions,
    })
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

fn is_safe_category_card(media: &TmdbMedia, category: &str) -> bool {
    media.id > 0
        && !is_adult_media(media)
        && media_title(media).is_some()
        && media
            .poster_path
            .as_deref()
            .is_some_and(|path| !path.is_empty())
        && media_year(media).is_some_and(|year| year >= 1900)
        && !media.genre_ids.is_empty()
        && (!matches!(category, "domestic_movie" | "domestic_tv") || media_has_cjk_title(media))
}

fn normalize_item_media_type(value: &str) -> Option<&'static str> {
    match value.trim() {
        "movie" => Some("movie"),
        "tv" => Some("tv"),
        _ => None,
    }
}

fn media_title(media: &TmdbMedia) -> Option<&str> {
    media
        .title
        .as_deref()
        .or(media.name.as_deref())
        .filter(|title| !title.trim().is_empty())
}

fn media_year(media: &TmdbMedia) -> Option<i32> {
    media
        .release_date
        .as_deref()
        .or(media.first_air_date.as_deref())
        .and_then(|date| date.get(..4))
        .and_then(|year| year.parse().ok())
}

fn media_has_cjk_title(media: &TmdbMedia) -> bool {
    [
        media.title.as_deref(),
        media.name.as_deref(),
        media.original_title.as_deref(),
        media.original_name.as_deref(),
    ]
    .into_iter()
    .flatten()
    .any(|title| {
        title
            .chars()
            .any(|character| ('\u{3400}'..='\u{9fff}').contains(&character))
    })
}

fn is_adult_media(media: &TmdbMedia) -> bool {
    if media.adult {
        return true;
    }
    let text = [
        media.title.as_deref(),
        media.name.as_deref(),
        media.original_title.as_deref(),
        media.original_name.as_deref(),
        media.overview.as_deref(),
    ]
    .into_iter()
    .flatten()
    .collect::<Vec<_>>()
    .join(" ")
    .to_ascii_lowercase();
    let compact = text.split_whitespace().collect::<String>();
    ADULT_TEXT_PATTERNS.iter().any(|pattern| {
        let pattern = pattern.to_ascii_lowercase();
        if pattern.is_ascii() {
            contains_ascii_phrase(&text, &pattern)
        } else {
            compact.contains(&pattern)
        }
    })
}

fn contains_ascii_phrase(text: &str, pattern: &str) -> bool {
    text.match_indices(pattern).any(|(start, matched)| {
        let before = text[..start].chars().next_back();
        let after = text[start + matched.len()..].chars().next();
        before.is_none_or(|character| !character.is_ascii_alphanumeric())
            && after.is_none_or(|character| !character.is_ascii_alphanumeric())
    })
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
    fn category_cards_require_the_same_core_fields_as_115sub() {
        let media = TmdbMedia {
            id: 42,
            title: Some("测试电影".to_string()),
            release_date: Some("2026-01-01".to_string()),
            poster_path: Some("/poster.jpg".to_string()),
            media_type: Some("movie".to_string()),
            genre_ids: vec![18],
            ..TmdbMedia::default()
        };
        assert!(is_safe_category_card(&media, "domestic_movie"));
        assert!(!is_safe_category_card(
            &TmdbMedia {
                poster_path: None,
                ..media.clone()
            },
            "domestic_movie"
        ));
        assert!(!is_safe_category_card(
            &TmdbMedia {
                title: Some("Adult Movie".to_string()),
                ..media
            },
            "foreign_movie"
        ));
    }

    #[test]
    fn subscribable_items_open_the_host_subscription_dialog() {
        let item = media_item(
            &TmdbMedia {
                id: 42,
                title: Some("测试电影".to_string()),
                release_date: Some("2026-01-01".to_string()),
                poster_path: Some("/poster.jpg".to_string()),
                vote_average: Some(8.2),
                media_type: Some("movie".to_string()),
                genre_ids: vec![18],
                ..TmdbMedia::default()
            },
            "国内电影",
            MediaStatus::default(),
        );
        assert_eq!(item["actions"][0]["type"], "subscription_create");
        assert_eq!(item["actions"][0]["payload"]["tmdb_id"], 42);
        assert_eq!(item["actions"][0]["payload"]["media_type"], "movie");
    }
}
