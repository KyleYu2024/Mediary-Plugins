use chrono::Local;
use regex::Regex;
use reqwest::Client;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};
use std::{
    collections::{HashSet, VecDeque},
    env, fs,
    path::{Path, PathBuf},
    time::Duration,
};

const TELEGRAM_CHANNEL: &str = "oneonefivewpfx";
const TELEGRAM_URL: &str = "https://t.me/s/oneonefivewpfx";
const USER_AGENT: &str =
    "Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) AppleWebKit/537.36 Chrome/121 Safari/537.36";
const MAX_RESPONSE_BYTES: usize = 2 * 1024 * 1024;
const MAX_HISTORY_ITEMS: usize = 2_000;
const DEFAULT_MAX_MESSAGES: usize = 50;
const DEFAULT_PUBLISHERS: [&str; 5] = [
    "白可乐",
    "冷",
    "新疆美味三文鱼",
    "Pluto",
    "最爱你的人望眼欲穿",
];

#[derive(Clone)]
struct PluginContext {
    api_url: String,
    token: String,
    data_dir: PathBuf,
    client: Client,
    settings: Map<String, Value>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct TelegramMessage {
    id: String,
    published_at: String,
    title: String,
    media_type: String,
    publisher: String,
    tmdb_id: i64,
    season: Option<i32>,
}

#[derive(Default, Deserialize, Serialize)]
struct State {
    #[serde(default)]
    processed: VecDeque<String>,
}

#[derive(Default, Deserialize, Serialize)]
struct Records {
    #[serde(default)]
    updated_at: String,
    #[serde(default)]
    summary: Summary,
    #[serde(default)]
    items: VecDeque<Record>,
}

#[derive(Default, Deserialize, Serialize)]
struct Summary {
    #[serde(default)]
    fetched: usize,
    #[serde(default)]
    official_messages: usize,
    #[serde(default)]
    unique_candidates: usize,
    #[serde(default)]
    duplicate_messages: usize,
    #[serde(default)]
    subscribed: usize,
    #[serde(default)]
    skipped_existing: usize,
    #[serde(default)]
    skipped_category: usize,
    #[serde(default)]
    skipped_history: usize,
    #[serde(default)]
    failures: usize,
    #[serde(default)]
    last_result: String,
}

#[derive(Serialize, Deserialize)]
struct Record {
    title: String,
    publisher: String,
    tmdb_id: i64,
    media_type: String,
    secondary_category: String,
    status: String,
    updated_at: String,
}

#[derive(Default, Deserialize)]
struct FilterCategories {
    #[serde(default)]
    movie: Vec<String>,
    #[serde(default)]
    tv: Vec<String>,
}

#[derive(Default, Deserialize)]
struct TmdbDetailsResponse {
    #[serde(default)]
    title: String,
    year: Option<i32>,
    suggested_secondary_category: Option<String>,
    #[serde(default)]
    details: TmdbDetails,
}

#[derive(Default, Deserialize)]
struct TmdbDetails {
    poster_path: Option<String>,
    backdrop_path: Option<String>,
    vote_average: Option<f64>,
    overview: Option<String>,
    #[serde(default)]
    seasons: Vec<TmdbSeason>,
}

#[derive(Default, Deserialize)]
struct TmdbSeason {
    season_number: i32,
    episode_count: i32,
}

#[derive(Default, Deserialize)]
struct Subscription {
    #[serde(default)]
    media_type: String,
    #[serde(default)]
    tmdb_id: Value,
}

#[derive(Clone, Debug)]
struct ResolvedMedia {
    title: String,
    year: Option<i32>,
    category: Option<String>,
    poster_path: Option<String>,
    backdrop_path: Option<String>,
    vote_average: Option<f64>,
    description: Option<String>,
    expected_episodes: Option<i32>,
}

#[derive(Serialize)]
struct RunReport {
    ran_at: String,
    fetched: usize,
    official_messages: usize,
    unique_candidates: usize,
    duplicate_messages: usize,
    subscribed: usize,
    skipped_history: usize,
    skipped_existing: usize,
    skipped_category: usize,
    failures: Vec<String>,
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
    if action == "status" {
        println!("{}", category_status(&context).await?);
        return Ok(());
    }
    if action != "refresh" {
        return Err(format!("不支持的影巢趋势动作: {action}"));
    }

    let state_path = context.data_dir.join("state.json");
    let records_path = context.data_dir.join("records.json");
    let mut state = load_json::<State>(&state_path);
    let mut records = load_json::<Records>(&records_path);
    let report = refresh(&context, &mut state, &mut records).await?;

    records.summary = Summary {
        fetched: report.fetched,
        official_messages: report.official_messages,
        unique_candidates: report.unique_candidates,
        duplicate_messages: report.duplicate_messages,
        subscribed: report.subscribed,
        skipped_existing: report.skipped_existing,
        skipped_category: report.skipped_category,
        skipped_history: report.skipped_history,
        failures: report.failures.len(),
        last_result: format!(
            "频道 {} 条，官组消息 {} 条，唯一影视 {} 部，合并重复 {} 条，新增订阅 {} 部，已有订阅 {} 部，分类跳过 {} 部，历史跳过 {} 部，失败 {} 部",
            report.fetched,
            report.official_messages,
            report.unique_candidates,
            report.duplicate_messages,
            report.subscribed,
            report.skipped_existing,
            report.skipped_category,
            report.skipped_history,
            report.failures.len()
        ),
    };
    records.updated_at = Local::now().to_rfc3339();
    trim_queue(&mut state.processed, MAX_HISTORY_ITEMS);
    trim_queue(&mut records.items, MAX_HISTORY_ITEMS);
    write_json(&state_path, &state)?;
    write_json(&records_path, &records)?;
    write_json(&context.data_dir.join("last-run.json"), &report)?;
    println!(
        "{}",
        json!({
            "notice": format!("影巢趋势检查完成：{}", records.summary.last_result),
            "report": report,
        })
    );
    Ok(())
}

impl PluginContext {
    fn from_env() -> Result<Self, String> {
        let api_url = required_env("MEDIARY_PLUGIN_API_URL")?;
        let token = required_env("MEDIARY_PLUGIN_TOKEN")?;
        let data_dir = PathBuf::from(required_env("MEDIARY_PLUGIN_DATA_DIR")?);
        fs::create_dir_all(&data_dir).map_err(|error| format!("创建数据目录失败: {error}"))?;
        let settings = env::var("MEDIARY_PLUGIN_SETTINGS_JSON")
            .ok()
            .filter(|value| !value.trim().is_empty())
            .and_then(|value| serde_json::from_str::<Map<String, Value>>(&value).ok())
            .unwrap_or_default();
        let client = Client::builder()
            .user_agent(USER_AGENT)
            .connect_timeout(Duration::from_secs(8))
            .timeout(Duration::from_secs(35))
            .redirect(reqwest::redirect::Policy::limited(3))
            .build()
            .map_err(|error| format!("创建 HTTP 客户端失败: {error}"))?;
        Ok(Self {
            api_url: api_url.trim_end_matches('/').to_string(),
            token,
            data_dir,
            client,
            settings,
        })
    }
}

async fn refresh(
    context: &PluginContext,
    state: &mut State,
    records: &mut Records,
) -> Result<RunReport, String> {
    let messages = fetch_telegram_messages(context).await?;
    let max_messages =
        setting_usize(&context.settings, "max_messages", DEFAULT_MAX_MESSAGES).clamp(1, 100);
    let publishers = {
        let values = setting_list(&context.settings, "publishers");
        if values.is_empty() {
            DEFAULT_PUBLISHERS
                .iter()
                .map(|value| value.to_string())
                .collect::<HashSet<_>>()
        } else {
            values.into_iter().collect::<HashSet<_>>()
        }
    };
    let requested_categories = setting_list(&context.settings, "secondary_categories");
    let configured_categories = fetch_filter_categories(context).await.unwrap_or_default();
    let configured_categories = configured_categories
        .movie
        .into_iter()
        .chain(configured_categories.tv)
        .collect::<HashSet<_>>();
    let selected_categories = if requested_categories.is_empty() {
        configured_categories.clone()
    } else if configured_categories.is_empty() {
        requested_categories.into_iter().collect::<HashSet<_>>()
    } else {
        let filtered = requested_categories
            .iter()
            .filter(|category| configured_categories.contains(*category))
            .cloned()
            .collect::<HashSet<_>>();
        if filtered.is_empty() {
            requested_categories.into_iter().collect::<HashSet<_>>()
        } else {
            filtered
        }
    };

    let messages = messages.into_iter().take(max_messages).collect::<Vec<_>>();
    let fetched = messages.len();
    let (messages, official_messages) = unique_official_messages(messages, &publishers);
    let unique_candidates = messages.len();
    let duplicate_messages = official_messages.saturating_sub(unique_candidates);
    let mut report = RunReport {
        ran_at: Local::now().to_rfc3339(),
        fetched,
        official_messages,
        unique_candidates,
        duplicate_messages,
        subscribed: 0,
        skipped_history: 0,
        skipped_existing: 0,
        skipped_category: 0,
        failures: Vec::new(),
    };
    let mut processed = state.processed.iter().cloned().collect::<HashSet<_>>();
    let mut existing = fetch_existing_subscription_keys(context).await?;

    for message in messages {
        if processed.contains(&message.id) {
            report.skipped_history += 1;
            continue;
        }
        let resolved = match fetch_tmdb_details(context, &message).await {
            Ok(value) => value,
            Err(error) => {
                report
                    .failures
                    .push(format!("{} / {}: {error}", message.title, message.tmdb_id));
                continue;
            }
        };
        let category = resolved.category.clone().unwrap_or_default();
        if !selected_categories.is_empty() && !selected_categories.contains(&category) {
            report.skipped_category += 1;
            processed_message(state, &mut processed, &message.id);
            add_record(records, &message, &category, "跳过：未选择该二级分类");
            continue;
        }

        let identity = media_identity(&message.media_type, message.tmdb_id);
        if existing.contains(&identity) {
            report.skipped_existing += 1;
            processed_message(state, &mut processed, &message.id);
            add_record(records, &message, &category, "跳过：已有订阅");
            continue;
        }

        match create_subscription(context, &message, &resolved).await {
            Ok(()) => {
                report.subscribed += 1;
                existing.insert(identity);
                processed_message(state, &mut processed, &message.id);
                add_record(records, &message, &category, "已创建订阅");
            }
            Err(error) => report
                .failures
                .push(format!("{} / {}: {error}", message.title, message.tmdb_id)),
        }
    }
    Ok(report)
}

async fn category_status(context: &PluginContext) -> Result<Value, String> {
    let categories = fetch_filter_categories(context).await?;
    let movie_count = categories.movie.len();
    let tv_count = categories.tv.len();
    let mut seen = HashSet::new();
    let options = categories
        .movie
        .into_iter()
        .map(|category| ("电影", category))
        .chain(categories.tv.into_iter().map(|category| ("剧集", category)))
        .filter(|(_, category)| seen.insert(category.clone()))
        .map(|(media_type, category)| {
            json!({
                "label": format!("{media_type} · {category}"),
                "value": category,
            })
        })
        .collect::<Vec<_>>();
    Ok(json!({
        "notice": format!("已读取当前 Mediary 的 {} 个二级分类。", options.len()),
        "items": [{
            "key": "secondary-categories",
            "title": "Mediary 二级分类",
            "subtitle": "以下选项来自当前实例的 category.yaml，不再使用插件内置列表。",
            "metadata": [
                {"label": "电影", "value": format!("{movie_count} 个")},
                {"label": "剧集", "value": format!("{tv_count} 个")}
            ]
        }],
        "form_options": {
            "secondary_categories": options
        }
    }))
}

fn unique_official_messages(
    messages: Vec<TelegramMessage>,
    publishers: &HashSet<String>,
) -> (Vec<TelegramMessage>, usize) {
    let mut seen = HashSet::new();
    let official = messages
        .into_iter()
        .filter(|message| publishers.contains(&message.publisher))
        .collect::<Vec<_>>();
    let official_count = official.len();
    let unique = official
        .into_iter()
        .filter(|message| seen.insert(media_identity(&message.media_type, message.tmdb_id)))
        .collect();
    (unique, official_count)
}

async fn fetch_telegram_messages(context: &PluginContext) -> Result<Vec<TelegramMessage>, String> {
    let response = context
        .client
        .get(TELEGRAM_URL)
        .send()
        .await
        .map_err(|error| format!("读取 TG 频道失败: {error}"))?;
    let status = response.status();
    let body = response
        .bytes()
        .await
        .map_err(|error| format!("读取 TG 频道响应失败: {error}"))?;
    if !status.is_success() {
        return Err(format!("TG 频道返回 HTTP {}", status.as_u16()));
    }
    if body.len() > MAX_RESPONSE_BYTES {
        return Err("TG 频道响应超过 2 MB 上限".to_string());
    }
    parse_telegram_messages(&String::from_utf8_lossy(&body))
}

fn parse_telegram_messages(html: &str) -> Result<Vec<TelegramMessage>, String> {
    let message_re = Regex::new(&format!(
        r#"(?s)<div class="tgme_widget_message\b[^>]*data-post="{TELEGRAM_CHANNEL}/(\d+)"[^>]*>.*?<div class="tgme_widget_message_text\b[^>]*>(.*?)</div>.*?<a class="tgme_widget_message_date"[^>]*><time datetime="([^"]+)""#
    ))
    .map_err(|error| format!("构造 TG 消息解析器失败: {error}"))?;
    let title_re = Regex::new(r#"(?s)<b>\[(电影|剧集)[^]]*\]\s*(.*?)</b>"#)
        .map_err(|error| format!("构造标题解析器失败: {error}"))?;
    let publisher_re = Regex::new(r#"分享：\s*<a[^>]*>(.*?)</a>"#)
        .map_err(|error| format!("构造发布者解析器失败: {error}"))?;
    let tmdb_re = Regex::new(r#"https://www\.themoviedb\.org/(tv|movie)/(\d+)"#)
        .map_err(|error| format!("构造 TMDB 解析器失败: {error}"))?;
    let season_re = Regex::new(r#"(?i)(?:\bS|Season\s*)(\d{1,2})"#)
        .map_err(|error| format!("构造季解析器失败: {error}"))?;
    let mut messages = Vec::new();

    for captures in message_re.captures_iter(html) {
        let Some(id) = captures.get(1).map(|value| value.as_str().to_string()) else {
            continue;
        };
        let Some(body) = captures.get(2).map(|value| value.as_str()) else {
            continue;
        };
        let Some(title_capture) = title_re.captures(body) else {
            continue;
        };
        let title = title_capture
            .get(2)
            .map(|value| decode_html(value.as_str()))
            .filter(|value| !value.is_empty());
        let Some(title) = title else {
            continue;
        };
        let media_type = if title_capture.get(1).map(|value| value.as_str()) == Some("剧集") {
            "tv"
        } else {
            "movie"
        };
        let Some(publisher) = publisher_re
            .captures(body)
            .and_then(|capture| capture.get(1))
            .map(|value| decode_html(value.as_str()))
            .filter(|value| !value.is_empty())
        else {
            continue;
        };
        let Some(tmdb_capture) = tmdb_re.captures(body) else {
            continue;
        };
        let Some(tmdb_id) = tmdb_capture
            .get(2)
            .and_then(|value| value.as_str().parse::<i64>().ok())
            .filter(|value| *value > 0)
        else {
            continue;
        };
        if (media_type == "tv") != (tmdb_capture.get(1).map(|value| value.as_str()) == Some("tv")) {
            continue;
        }
        let season = (media_type == "tv")
            .then(|| {
                season_re
                    .captures(body)
                    .and_then(|capture| capture.get(1)?.as_str().parse::<i32>().ok())
            })
            .flatten();
        let published_at = captures
            .get(3)
            .map(|value| value.as_str().to_string())
            .unwrap_or_default();
        messages.push(TelegramMessage {
            id,
            published_at,
            title,
            media_type: media_type.to_string(),
            publisher,
            tmdb_id,
            season,
        });
    }
    messages.sort_by(|left, right| right.id.cmp(&left.id));
    Ok(messages)
}

async fn fetch_filter_categories(context: &PluginContext) -> Result<FilterCategories, String> {
    let payload = api_get(context, "/filter/categories").await?;
    serde_json::from_value(payload)
        .map_err(|error| format!("Mediary 二级分类响应格式无效: {error}"))
}

async fn fetch_tmdb_details(
    context: &PluginContext,
    message: &TelegramMessage,
) -> Result<ResolvedMedia, String> {
    let path = format!(
        "/search/tmdb/details?id={}&media_type={}",
        message.tmdb_id, message.media_type
    );
    let payload = api_get(context, &path).await?;
    let details: TmdbDetailsResponse = serde_json::from_value(payload)
        .map_err(|error| format!("TMDB 详情响应格式无效: {error}"))?;
    let expected_episodes = message.season.and_then(|season| {
        details
            .details
            .seasons
            .iter()
            .find(|item| item.season_number == season)
            .map(|item| item.episode_count)
            .filter(|count| *count > 0)
    });
    Ok(ResolvedMedia {
        title: if details.title.trim().is_empty() {
            message.title.clone()
        } else {
            details.title
        },
        year: details.year,
        category: details
            .suggested_secondary_category
            .filter(|value| !value.trim().is_empty()),
        poster_path: details.details.poster_path,
        backdrop_path: details.details.backdrop_path,
        vote_average: details.details.vote_average,
        description: details.details.overview,
        expected_episodes,
    })
}

async fn fetch_existing_subscription_keys(
    context: &PluginContext,
) -> Result<HashSet<String>, String> {
    let payload = api_get(context, "/subscriptions").await?;
    let values = payload
        .as_array()
        .or_else(|| payload.get("subscriptions").and_then(Value::as_array))
        .or_else(|| payload.get("items").and_then(Value::as_array))
        .ok_or_else(|| "Mediary 订阅列表响应格式无效".to_string())?;
    Ok(values
        .iter()
        .filter_map(|value| serde_json::from_value::<Subscription>(value.clone()).ok())
        .filter_map(|subscription| {
            let tmdb_id = value_as_i64(&subscription.tmdb_id)?;
            let media_type = subscription.media_type.trim();
            matches!(media_type, "movie" | "tv").then(|| media_identity(media_type, tmdb_id))
        })
        .collect())
}

async fn create_subscription(
    context: &PluginContext,
    message: &TelegramMessage,
    resolved: &ResolvedMedia,
) -> Result<(), String> {
    let payload = json!({
        "tmdb_id": message.tmdb_id,
        "name": resolved.title,
        "year": resolved.year,
        "media_type": message.media_type,
        "season": message.season,
        "season_start_episode": (message.media_type == "tv").then_some(1),
        "expected_episodes": resolved.expected_episodes,
        "poster_path": resolved.poster_path,
        "backdrop_path": resolved.backdrop_path,
        "vote_average": resolved.vote_average,
        "description": resolved.description,
        "secondary_category": resolved.category,
    });
    api_post(context, "/subscriptions", payload)
        .await
        .map(|_| ())
}

async fn api_get(context: &PluginContext, path: &str) -> Result<Value, String> {
    let response = context
        .client
        .get(format!("{}{}", context.api_url, path))
        .bearer_auth(&context.token)
        .send()
        .await
        .map_err(|error| format!("Mediary API 请求失败: {error}"))?;
    parse_api_response(response).await
}

async fn api_post(context: &PluginContext, path: &str, payload: Value) -> Result<Value, String> {
    let response = context
        .client
        .post(format!("{}{}", context.api_url, path))
        .bearer_auth(&context.token)
        .json(&payload)
        .send()
        .await
        .map_err(|error| format!("Mediary API 请求失败: {error}"))?;
    parse_api_response(response).await
}

async fn parse_api_response(response: reqwest::Response) -> Result<Value, String> {
    let status = response.status();
    let body = response.text().await.unwrap_or_default();
    if !status.is_success() {
        let detail = serde_json::from_str::<Value>(&body)
            .ok()
            .and_then(|value| {
                value
                    .get("error")
                    .or_else(|| value.get("message"))
                    .and_then(Value::as_str)
                    .map(str::to_string)
            })
            .unwrap_or_else(|| body.chars().take(180).collect());
        return Err(format!("Mediary API HTTP {}: {}", status.as_u16(), detail));
    }
    serde_json::from_str(&body).map_err(|error| format!("Mediary API 响应不是有效 JSON: {error}"))
}

fn media_identity(media_type: &str, tmdb_id: i64) -> String {
    format!("{}:{}", media_type.trim().to_ascii_lowercase(), tmdb_id)
}

fn setting_list(settings: &Map<String, Value>, key: &str) -> Vec<String> {
    match settings.get(key) {
        Some(Value::Array(values)) => values
            .iter()
            .filter_map(Value::as_str)
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(str::to_string)
            .collect(),
        Some(Value::String(value)) => value
            .split([',', '，', '\n', '\r'])
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(str::to_string)
            .collect(),
        _ => Vec::new(),
    }
}

fn setting_usize(settings: &Map<String, Value>, key: &str, default: usize) -> usize {
    settings
        .get(key)
        .and_then(Value::as_u64)
        .and_then(|value| usize::try_from(value).ok())
        .unwrap_or(default)
}

fn processed_message(state: &mut State, processed: &mut HashSet<String>, id: &str) {
    if processed.insert(id.to_string()) {
        state.processed.push_front(id.to_string());
    }
}

fn add_record(records: &mut Records, message: &TelegramMessage, category: &str, status: &str) {
    records.items.push_front(Record {
        title: message.title.clone(),
        publisher: message.publisher.clone(),
        tmdb_id: message.tmdb_id,
        media_type: message.media_type.clone(),
        secondary_category: category.to_string(),
        status: status.to_string(),
        updated_at: Local::now().to_rfc3339(),
    });
}

fn value_as_i64(value: &Value) -> Option<i64> {
    value
        .as_i64()
        .or_else(|| value.as_str()?.trim().parse::<i64>().ok())
        .filter(|value| *value > 0)
}

fn decode_html(value: &str) -> String {
    value
        .replace("&amp;", "&")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&#39;", "'")
        .replace("&#x27;", "'")
        .replace("&nbsp;", " ")
        .trim()
        .to_string()
}

fn load_json<T>(path: &Path) -> T
where
    T: Default + for<'de> Deserialize<'de>,
{
    fs::read_to_string(path)
        .ok()
        .and_then(|value| serde_json::from_str(&value).ok())
        .unwrap_or_default()
}

fn write_json<T: Serialize>(path: &Path, value: &T) -> Result<(), String> {
    let encoded = serde_json::to_string_pretty(value).map_err(|error| error.to_string())?;
    let temporary = path.with_extension("json.tmp");
    fs::write(&temporary, format!("{encoded}\n")).map_err(|error| error.to_string())?;
    fs::rename(temporary, path).map_err(|error| error.to_string())
}

fn trim_queue<T>(queue: &mut VecDeque<T>, limit: usize) {
    while queue.len() > limit {
        queue.pop_back();
    }
}

fn required_env(name: &str) -> Result<String, String> {
    env::var(name).map_err(|_| format!("缺少运行时环境变量 {name}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_channel_messages_with_publisher_and_tmdb() {
        let html = r#"
        <div class="tgme_widget_message text_not_supported_wrap" data-post="oneonefivewpfx/41138">
          <div class="tgme_widget_message_text js-message_text" dir="auto"><b>[剧集·115] 金色 (2026)</b><br/><blockquote>S01E01-E09 4K WEB-DL</blockquote><br/>分享：<a href="https://hdhive.com/user/3">冷</a><br/>TMDB: <a href="https://www.themoviedb.org/tv/294487">294487</a></div>
          <span><a class="tgme_widget_message_date" href="https://t.me/oneonefivewpfx/41138"><time datetime="2026-08-31T03:35:13+00:00">03:35</time></a></span>
        </div>
        "#;
        let messages = parse_telegram_messages(html).unwrap();
        assert_eq!(messages.len(), 1);
        assert_eq!(messages[0].id, "41138");
        assert_eq!(messages[0].title, "金色 (2026)");
        assert_eq!(messages[0].publisher, "冷");
        assert_eq!(messages[0].media_type, "tv");
        assert_eq!(messages[0].tmdb_id, 294487);
        assert_eq!(messages[0].season, Some(1));
    }

    #[test]
    fn parses_movie_and_html_entities() {
        let html = r#"
        <div class="tgme_widget_message" data-post="oneonefivewpfx/9">
          <div class="tgme_widget_message_text js-message_text"><b>[电影·115] A&amp;B (2025)</b>分享：<a>最爱你的人望眼欲穿</a> TMDB: <a href="https://www.themoviedb.org/movie/123">123</a></div>
          <a class="tgme_widget_message_date"><time datetime="2026-01-01T00:00:00+00:00">00:00</time></a>
        </div>
        "#;
        let messages = parse_telegram_messages(html).unwrap();
        assert_eq!(messages[0].title, "A&B (2025)");
        assert_eq!(messages[0].media_type, "movie");
        assert_eq!(messages[0].season, None);
    }

    #[test]
    fn settings_accept_array_and_comma_separated_values() {
        let mut settings = Map::new();
        settings.insert("publishers".to_string(), json!("冷, 新疆美味三文鱼"));
        assert_eq!(
            setting_list(&settings, "publishers"),
            vec!["冷", "新疆美味三文鱼"]
        );
        settings.insert(
            "secondary_categories".to_string(),
            json!(["电视剧-中文", "综艺"]),
        );
        assert_eq!(
            setting_list(&settings, "secondary_categories"),
            vec!["电视剧-中文", "综艺"]
        );
    }

    #[test]
    fn subscription_identity_ignores_season_for_simple_deduplication() {
        assert_eq!(media_identity("tv", 294487), "tv:294487");
        assert_eq!(media_identity("movie", 123), "movie:123");
    }

    #[test]
    fn official_messages_are_deduplicated_by_media_identity() {
        let message = |id: &str, publisher: &str, tmdb_id| TelegramMessage {
            id: id.to_string(),
            published_at: String::new(),
            title: format!("媒体 {tmdb_id}"),
            media_type: "tv".to_string(),
            publisher: publisher.to_string(),
            tmdb_id,
            season: Some(1),
        };
        let publishers = ["冷".to_string(), "白可乐".to_string()]
            .into_iter()
            .collect::<HashSet<_>>();
        let (unique, official_count) = unique_official_messages(
            vec![
                message("3", "冷", 100),
                message("2", "白可乐", 100),
                message("1", "普通用户", 200),
            ],
            &publishers,
        );

        assert_eq!(official_count, 2);
        assert_eq!(unique.len(), 1);
        assert_eq!(unique[0].id, "3");
    }

    #[test]
    fn processed_history_keeps_the_newest_message_ids() {
        let mut state = State::default();
        let mut processed = HashSet::new();
        for id in ["1", "2", "3"] {
            processed_message(&mut state, &mut processed, id);
        }
        trim_queue(&mut state.processed, 2);
        assert_eq!(
            state.processed.into_iter().collect::<Vec<_>>(),
            vec!["3", "2"]
        );
    }
}
