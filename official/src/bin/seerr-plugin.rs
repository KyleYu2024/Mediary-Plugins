use chrono::Local;
use fs2::FileExt;
use reqwest::{Client, StatusCode};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{
    collections::HashMap,
    env, fs,
    fs::{File, OpenOptions},
    io::Write,
    path::{Path, PathBuf},
    time::Duration,
};

const PLUGIN_ID: &str = "seerr";
const WEBHOOK_ACTION: &str = "webhook";
const MAX_HISTORY_LIMIT: usize = 2000;

#[derive(Clone, Deserialize)]
struct Settings {
    #[serde(default = "default_true")]
    sync_declines: bool,
    #[serde(default = "default_quality")]
    quality: String,
    #[serde(default)]
    standard_resolution: String,
    #[serde(default = "default_four_k_resolution")]
    four_k_resolution: String,
    #[serde(default)]
    include_rules: String,
    #[serde(default)]
    exclude_rules: String,
    #[serde(default)]
    save_path: String,
    #[serde(default)]
    secondary_category: String,
    #[serde(default = "default_history_limit")]
    history_limit: usize,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            sync_declines: true,
            quality: default_quality(),
            standard_resolution: String::new(),
            four_k_resolution: default_four_k_resolution(),
            include_rules: String::new(),
            exclude_rules: String::new(),
            save_path: String::new(),
            secondary_category: String::new(),
            history_limit: default_history_limit(),
        }
    }
}

struct PluginContext {
    api_url: String,
    token: String,
    data_dir: PathBuf,
    settings: Settings,
    client: Client,
}

#[derive(Default, Deserialize)]
struct SeerrWebhook {
    #[serde(default)]
    notification_type: String,
    #[serde(default)]
    event: String,
    #[serde(default)]
    subject: String,
    #[serde(default)]
    message: String,
    #[serde(default)]
    image: String,
    media: Option<SeerrMedia>,
    request: Option<SeerrRequest>,
    #[serde(default)]
    extra: Vec<SeerrExtra>,
}

#[derive(Default, Deserialize)]
struct SeerrMedia {
    #[serde(default, alias = "mediaType")]
    media_type: String,
    #[serde(default, rename = "tmdbId", alias = "tmdb_id")]
    tmdb_id: Value,
    #[serde(default)]
    status: String,
    #[serde(default)]
    status4k: String,
}

#[derive(Default, Deserialize)]
struct SeerrRequest {
    #[serde(default, alias = "id")]
    request_id: Value,
    #[serde(
        default,
        rename = "requestedBy_username",
        alias = "requested_by_username"
    )]
    requested_by_username: String,
}

#[derive(Default, Deserialize)]
struct SeerrExtra {
    #[serde(default)]
    name: String,
    #[serde(default)]
    value: Value,
}

#[derive(Default, Serialize, Deserialize)]
struct PluginState {
    #[serde(default)]
    mappings: HashMap<String, SubscriptionMapping>,
    #[serde(default)]
    summary: HistorySummary,
    #[serde(default)]
    items: Vec<HistoryItem>,
}

#[derive(Clone, Serialize, Deserialize)]
struct SubscriptionMapping {
    request_id: String,
    media_type: String,
    tmdb_id: i32,
    season: Option<i32>,
    subscription_id: i64,
    title: String,
    owned: bool,
    created_at: String,
}

#[derive(Default, Serialize, Deserialize)]
struct HistorySummary {
    #[serde(default)]
    received: usize,
    #[serde(default)]
    created: usize,
    #[serde(default)]
    deleted: usize,
    #[serde(default)]
    errors: usize,
}

#[derive(Serialize, Deserialize)]
struct HistoryItem {
    title: String,
    event: String,
    result: String,
    received_at: String,
}

#[derive(Default, Deserialize)]
struct TmdbDetailsEnvelope {
    title: Option<String>,
    year: Option<i32>,
    suggested_secondary_category: Option<String>,
    details: Option<TmdbDetails>,
}

#[derive(Default, Deserialize)]
struct TmdbDetails {
    title: Option<String>,
    name: Option<String>,
    release_date: Option<String>,
    first_air_date: Option<String>,
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

#[derive(Clone, Deserialize)]
struct SubscriptionRef {
    id: i64,
    state: String,
}

#[derive(Debug)]
struct ApprovedMedia {
    request_id: String,
    media_type: String,
    tmdb_id: i32,
    seasons: Vec<Option<i32>>,
    requested_by: Option<String>,
    is_4k: bool,
}

#[tokio::main]
async fn main() {
    if let Err(error) = run().await {
        eprintln!("{error}");
        std::process::exit(1);
    }
}

async fn run() -> Result<(), String> {
    let action = env::var("MEDIARY_PLUGIN_ACTION").unwrap_or_default();
    let context = PluginContext::from_env()?;
    let output = match action.as_str() {
        WEBHOOK_ACTION => {
            let payload = read_stdin_json()?;
            let webhook = serde_json::from_value::<SeerrWebhook>(payload)
                .map_err(|error| format!("Seerr Webhook 格式无效: {error}"))?;
            handle_webhook(&context, webhook).await?
        }
        "status" => status(&context)?,
        _ => return Err(format!("不支持的 Seerr 插件动作: {action}")),
    };
    println!("{output}");
    Ok(())
}

impl PluginContext {
    fn from_env() -> Result<Self, String> {
        let api_url = required_env("MEDIARY_PLUGIN_API_URL")?;
        let token = required_env("MEDIARY_PLUGIN_TOKEN")?;
        let data_dir = PathBuf::from(required_env("MEDIARY_PLUGIN_DATA_DIR")?);
        let settings = env::var("MEDIARY_PLUGIN_SETTINGS_JSON")
            .ok()
            .and_then(|raw| serde_json::from_str::<Settings>(&raw).ok())
            .unwrap_or_default();
        fs::create_dir_all(&data_dir)
            .map_err(|error| format!("创建 Seerr 插件数据目录失败: {error}"))?;
        let client = Client::builder()
            .timeout(Duration::from_secs(50))
            .user_agent("Mediary-Seerr-Plugin/0.1.0")
            .build()
            .map_err(|error| format!("创建 HTTP 客户端失败: {error}"))?;
        Ok(Self {
            api_url: api_url.trim_end_matches('/').to_string(),
            token,
            data_dir,
            settings,
            client,
        })
    }
}

async fn handle_webhook(context: &PluginContext, webhook: SeerrWebhook) -> Result<Value, String> {
    let _lock = lock_state(&context.data_dir)?;
    let mut state = load_state(&context.data_dir);
    state.summary.received += 1;

    let event = webhook.notification_type.trim().to_ascii_uppercase();
    let title = webhook_title(&webhook);
    let result = match event.as_str() {
        "MEDIA_APPROVED" | "MEDIA_AUTO_APPROVED" => {
            process_approved(context, &webhook, &mut state).await
        }
        "MEDIA_DECLINED" => process_declined(context, &webhook, &mut state).await,
        _ => Ok(json!({
            "success": true,
            "skipped": true,
            "message": format!("已忽略 Seerr 事件 {event}"),
        })),
    };

    let (history_result, failed) = match &result {
        Ok(value) => (
            value
                .get("message")
                .and_then(Value::as_str)
                .unwrap_or("处理完成")
                .to_string(),
            false,
        ),
        Err(error) => (error.clone(), true),
    };
    if failed {
        state.summary.errors += 1;
    }
    state.items.insert(
        0,
        HistoryItem {
            title,
            event: if event.is_empty() {
                "UNKNOWN".to_string()
            } else {
                event
            },
            result: history_result,
            received_at: now_string(),
        },
    );
    state
        .items
        .truncate(context.settings.history_limit.clamp(20, MAX_HISTORY_LIMIT));
    save_state(&context.data_dir, &state)?;
    result
}

async fn process_approved(
    context: &PluginContext,
    webhook: &SeerrWebhook,
    state: &mut PluginState,
) -> Result<Value, String> {
    let media = approved_media(webhook)?;
    let details = fetch_tmdb_details(context, media.tmdb_id, &media.media_type).await;
    let title = media_title(webhook, media.tmdb_id, details.as_ref());
    let mut created = 0usize;
    let mut existing = 0usize;
    let mut subscription_ids = Vec::new();

    for season in &media.seasons {
        let key = mapping_key(&media.request_id, &media.media_type, media.tmdb_id, *season);
        if let Some(mapping) = state.mappings.get(&key).cloned() {
            let current =
                find_subscription(context, media.tmdb_id, &media.media_type, *season).await?;
            if current.is_some_and(|subscription| subscription.id == mapping.subscription_id) {
                existing += 1;
                subscription_ids.push(mapping.subscription_id);
                continue;
            }
            state.mappings.remove(&key);
            save_state(&context.data_dir, state)?;
        }

        if let Some(subscription) =
            find_subscription(context, media.tmdb_id, &media.media_type, *season).await?
        {
            existing += 1;
            subscription_ids.push(subscription.id);
            state.mappings.insert(
                key,
                new_mapping(&media, *season, subscription.id, &title, false),
            );
            save_state(&context.data_dir, state)?;
            continue;
        }

        let payload =
            subscription_payload(context, webhook, &media, *season, &title, details.as_ref());
        let subscription_id = create_subscription(context, payload).await?;
        created += 1;
        state.summary.created += 1;
        subscription_ids.push(subscription_id);
        state.mappings.insert(
            key,
            new_mapping(&media, *season, subscription_id, &title, true),
        );
        save_state(&context.data_dir, state)?;
    }

    let message = if media.media_type == "tv" {
        format!("Seerr 剧集请求已同步：新建 {created} 季，已存在 {existing} 季")
    } else {
        format!("Seerr 电影请求已同步：新建 {created}，已存在 {existing}")
    };
    Ok(json!({
        "success": true,
        "message": message,
        "created": created,
        "existing": existing,
        "subscription_ids": subscription_ids,
    }))
}

async fn process_declined(
    context: &PluginContext,
    webhook: &SeerrWebhook,
    state: &mut PluginState,
) -> Result<Value, String> {
    if !context.settings.sync_declines {
        return Ok(json!({
            "success": true,
            "skipped": true,
            "message": "已忽略 Seerr 拒绝事件：拒绝同步未启用",
        }));
    }
    let request_id = webhook_request_id(webhook)?;
    let targets = state
        .mappings
        .iter()
        .filter(|(_, mapping)| mapping.request_id == request_id && mapping.owned)
        .map(|(key, mapping)| (key.clone(), mapping.clone()))
        .collect::<Vec<_>>();
    if targets.is_empty() {
        return Ok(json!({
            "success": true,
            "skipped": true,
            "message": "Seerr 拒绝事件无可取消的插件订阅",
        }));
    }

    let mut deleted = 0usize;
    let mut completed = 0usize;
    for (key, mapping) in targets {
        let current = find_subscription(
            context,
            mapping.tmdb_id,
            &mapping.media_type,
            mapping.season,
        )
        .await?;
        let Some(current) = current else {
            state.mappings.remove(&key);
            continue;
        };
        if current.id != mapping.subscription_id {
            state.mappings.remove(&key);
            continue;
        }
        if current.state == "completed" {
            completed += 1;
            state.mappings.remove(&key);
            continue;
        }
        delete_subscription(context, mapping.subscription_id).await?;
        state.mappings.remove(&key);
        state.summary.deleted += 1;
        deleted += 1;
    }

    Ok(json!({
        "success": true,
        "message": format!("Seerr 拒绝请求已同步：取消 {deleted} 个，保留已完成 {completed} 个"),
        "deleted": deleted,
        "completed_preserved": completed,
    }))
}

fn approved_media(webhook: &SeerrWebhook) -> Result<ApprovedMedia, String> {
    let media = webhook
        .media
        .as_ref()
        .ok_or_else(|| "Seerr Webhook 缺少 media 对象".to_string())?;
    let media_type = normalize_media_type(&media.media_type)
        .ok_or_else(|| "Seerr media_type 必须是 movie 或 tv".to_string())?;
    let tmdb_id = value_i32(&media.tmdb_id)
        .filter(|value| *value > 0)
        .ok_or_else(|| "Seerr Webhook 缺少有效 TMDB ID".to_string())?;
    let request_id = webhook_request_id(webhook)?;
    let seasons = if media_type == "tv" {
        let seasons = requested_seasons(&webhook.extra);
        if seasons.is_empty() {
            return Err("Seerr 剧集 Webhook 缺少 Requested Seasons".to_string());
        }
        seasons.into_iter().map(Some).collect()
    } else {
        vec![None]
    };
    let requested_by = webhook
        .request
        .as_ref()
        .map(|request| request.requested_by_username.trim())
        .filter(|value| !value.is_empty())
        .map(str::to_string);
    Ok(ApprovedMedia {
        request_id,
        media_type: media_type.to_string(),
        tmdb_id,
        seasons,
        requested_by,
        is_4k: is_4k_request(webhook),
    })
}

fn subscription_payload(
    context: &PluginContext,
    webhook: &SeerrWebhook,
    media: &ApprovedMedia,
    season: Option<i32>,
    title: &str,
    details: Option<&TmdbDetailsEnvelope>,
) -> Value {
    let detail = details.and_then(|item| item.details.as_ref());
    let resolution = if media.is_4k {
        non_empty(&context.settings.four_k_resolution)
    } else {
        non_empty(&context.settings.standard_resolution)
    };
    let expected_episodes = season.and_then(|season_number| {
        detail?
            .seasons
            .iter()
            .find(|item| item.season_number == season_number)
            .map(|item| item.episode_count)
            .filter(|count| *count > 0)
    });
    let year = details
        .and_then(|item| item.year)
        .or_else(|| detail.and_then(details_year))
        .or_else(|| subject_year(&webhook.subject));
    let secondary_category = non_empty(&context.settings.secondary_category).or_else(|| {
        details.and_then(|item| {
            item.suggested_secondary_category
                .as_deref()
                .and_then(non_empty)
        })
    });
    let poster_path = detail
        .and_then(|item| item.poster_path.as_deref())
        .map(str::to_string)
        .or_else(|| tmdb_image_path(&webhook.image));
    json!({
        "name": title,
        "year": year,
        "media_type": media.media_type,
        "tmdb_id": media.tmdb_id,
        "season": season,
        "season_start_episode": season.map(|_| 1),
        "expected_episodes": expected_episodes,
        "poster_path": poster_path,
        "backdrop_path": detail.and_then(|item| item.backdrop_path.as_deref()),
        "vote_average": detail.and_then(|item| item.vote_average),
        "description": detail
            .and_then(|item| item.overview.as_deref())
            .or_else(|| non_empty(&webhook.message)),
        "quality": non_empty(&context.settings.quality).unwrap_or("all"),
        "resolution": resolution,
        "include_rules": non_empty(&context.settings.include_rules),
        "exclude_rules": non_empty(&context.settings.exclude_rules),
        "source_owners": media.requested_by,
        "min_size_mb": null,
        "max_size_mb": null,
        "season_offset": null,
        "episode_offset": null,
        "title_corrections": [],
        "save_path": non_empty(&context.settings.save_path),
        "secondary_category": secondary_category,
    })
}

async fn fetch_tmdb_details(
    context: &PluginContext,
    tmdb_id: i32,
    media_type: &str,
) -> Option<TmdbDetailsEnvelope> {
    let response = context
        .client
        .get(format!("{}/search/tmdb/details", context.api_url))
        .query(&[
            ("id", tmdb_id.to_string()),
            ("media_type", media_type.to_string()),
        ])
        .bearer_auth(&context.token)
        .send()
        .await
        .ok()?;
    if !response.status().is_success() {
        return None;
    }
    response.json().await.ok()
}

async fn find_subscription(
    context: &PluginContext,
    tmdb_id: i32,
    media_type: &str,
    season: Option<i32>,
) -> Result<Option<SubscriptionRef>, String> {
    let mut query = vec![
        ("tmdb_id", tmdb_id.to_string()),
        ("media_type", media_type.to_string()),
        ("limit", "1".to_string()),
    ];
    if let Some(season) = season {
        query.push(("season", season.to_string()));
    }
    let response = context
        .client
        .get(format!("{}/subscriptions", context.api_url))
        .query(&query)
        .bearer_auth(&context.token)
        .send()
        .await
        .map_err(|error| format!("查询 Mediary 订阅失败: {error}"))?;
    let status = response.status();
    let body = response.bytes().await.unwrap_or_default();
    if !status.is_success() {
        return Err(response_error("查询 Mediary 订阅失败", status, &body));
    }
    serde_json::from_slice::<Vec<SubscriptionRef>>(&body)
        .map(|items| items.into_iter().next())
        .map_err(|error| format!("Mediary 订阅列表格式无效: {error}"))
}

async fn create_subscription(context: &PluginContext, payload: Value) -> Result<i64, String> {
    let response = context
        .client
        .post(format!("{}/subscriptions", context.api_url))
        .bearer_auth(&context.token)
        .json(&payload)
        .send()
        .await
        .map_err(|error| format!("创建 Mediary 订阅失败: {error}"))?;
    let status = response.status();
    let body = response.bytes().await.unwrap_or_default();
    if !status.is_success() {
        return Err(response_error("创建 Mediary 订阅失败", status, &body));
    }
    let value = serde_json::from_slice::<Value>(&body)
        .map_err(|error| format!("Mediary 创建订阅响应格式无效: {error}"))?;
    value
        .get("id")
        .and_then(Value::as_i64)
        .ok_or_else(|| "Mediary 创建订阅响应缺少 id".to_string())
}

async fn delete_subscription(context: &PluginContext, subscription_id: i64) -> Result<(), String> {
    let response = context
        .client
        .delete(format!(
            "{}/subscriptions/{subscription_id}",
            context.api_url
        ))
        .bearer_auth(&context.token)
        .send()
        .await
        .map_err(|error| format!("取消 Mediary 订阅失败: {error}"))?;
    let status = response.status();
    if status.is_success() || status == StatusCode::NOT_FOUND {
        return Ok(());
    }
    let body = response.bytes().await.unwrap_or_default();
    Err(response_error("取消 Mediary 订阅失败", status, &body))
}

fn response_error(prefix: &str, status: StatusCode, body: &[u8]) -> String {
    let detail = serde_json::from_slice::<Value>(body)
        .ok()
        .and_then(|value| {
            value
                .get("error")
                .or_else(|| value.get("message"))
                .and_then(Value::as_str)
                .map(str::to_string)
        })
        .unwrap_or_else(|| String::from_utf8_lossy(body).trim().to_string());
    if detail.is_empty() {
        format!("{prefix}: HTTP {status}")
    } else {
        format!("{prefix}: HTTP {status}: {detail}")
    }
}

fn requested_seasons(extra: &[SeerrExtra]) -> Vec<i32> {
    let mut seasons = extra
        .iter()
        .filter(|item| {
            let name = item.name.trim().to_ascii_lowercase();
            name == "requested seasons" || name.contains("season") || name.contains("季")
        })
        .flat_map(|item| integers_from_value(&item.value))
        .filter(|season| *season > 0)
        .collect::<Vec<_>>();
    seasons.sort_unstable();
    seasons.dedup();
    seasons
}

fn integers_from_value(value: &Value) -> Vec<i32> {
    match value {
        Value::Array(values) => values.iter().filter_map(value_i32).collect(),
        Value::Number(_) => value_i32(value).into_iter().collect(),
        Value::String(value) => value
            .split(|character: char| !character.is_ascii_digit())
            .filter(|part| !part.is_empty())
            .filter_map(|part| part.parse::<i32>().ok())
            .collect(),
        _ => Vec::new(),
    }
}

fn value_i32(value: &Value) -> Option<i32> {
    value
        .as_i64()
        .and_then(|value| i32::try_from(value).ok())
        .or_else(|| value.as_str()?.trim().parse::<i32>().ok())
}

fn value_string(value: &Value) -> Option<String> {
    value
        .as_str()
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_string)
        .or_else(|| value.as_i64().map(|value| value.to_string()))
}

fn webhook_request_id(webhook: &SeerrWebhook) -> Result<String, String> {
    webhook
        .request
        .as_ref()
        .and_then(|request| value_string(&request.request_id))
        .ok_or_else(|| "Seerr Webhook 缺少 request_id".to_string())
}

fn is_4k_request(webhook: &SeerrWebhook) -> bool {
    if webhook.event.to_ascii_lowercase().contains("4k") {
        return true;
    }
    webhook.media.as_ref().is_some_and(|media| {
        matches!(media.status4k.as_str(), "PENDING" | "PROCESSING")
            && matches!(media.status.as_str(), "UNKNOWN" | "DELETED" | "")
    })
}

fn media_title(
    webhook: &SeerrWebhook,
    tmdb_id: i32,
    details: Option<&TmdbDetailsEnvelope>,
) -> String {
    details
        .and_then(|item| item.title.as_deref())
        .or_else(|| {
            details.and_then(|item| {
                item.details
                    .as_ref()
                    .and_then(|detail| detail.title.as_deref().or(detail.name.as_deref()))
            })
        })
        .and_then(non_empty)
        .map(str::to_string)
        .or_else(|| non_empty(&strip_subject_year(&webhook.subject)).map(str::to_string))
        .unwrap_or_else(|| format!("TMDB {tmdb_id}"))
}

fn webhook_title(webhook: &SeerrWebhook) -> String {
    non_empty(&webhook.subject)
        .unwrap_or("未命名 Seerr 请求")
        .to_string()
}

fn strip_subject_year(subject: &str) -> String {
    let trimmed = subject.trim();
    if trimmed.len() >= 7 {
        let suffix = trimmed.get(trimmed.len() - 7..);
        if suffix.is_some_and(|suffix| {
            suffix.starts_with(" (")
                && suffix.ends_with(')')
                && suffix[2..6]
                    .chars()
                    .all(|character| character.is_ascii_digit())
        }) && let Some(title) = trimmed.get(..trimmed.len() - 7)
        {
            return title.trim().to_string();
        }
    }
    trimmed.to_string()
}

fn tmdb_image_path(image: &str) -> Option<String> {
    let image = non_empty(image)?;
    let path = image
        .split_once("image.tmdb.org/t/p/")?
        .1
        .split_once('/')?
        .1;
    let path = path.split(['?', '#']).next()?.trim_start_matches('/');
    (!path.is_empty()).then(|| format!("/{path}"))
}

fn subject_year(subject: &str) -> Option<i32> {
    let trimmed = subject.trim();
    let suffix = trimmed.get(trimmed.len().checked_sub(6)?..)?;
    if suffix.starts_with('(') && suffix.ends_with(')') {
        suffix[1..5].parse().ok()
    } else {
        None
    }
}

fn details_year(details: &TmdbDetails) -> Option<i32> {
    details
        .release_date
        .as_deref()
        .or(details.first_air_date.as_deref())
        .and_then(|date| date.get(..4))
        .and_then(|year| year.parse().ok())
}

fn normalize_media_type(value: &str) -> Option<&'static str> {
    match value.trim().to_ascii_lowercase().as_str() {
        "movie" => Some("movie"),
        "tv" => Some("tv"),
        _ => None,
    }
}

fn mapping_key(request_id: &str, media_type: &str, tmdb_id: i32, season: Option<i32>) -> String {
    format!(
        "{request_id}:{media_type}:{tmdb_id}:{}",
        season.map_or_else(|| "movie".to_string(), |value| value.to_string())
    )
}

fn new_mapping(
    media: &ApprovedMedia,
    season: Option<i32>,
    subscription_id: i64,
    title: &str,
    owned: bool,
) -> SubscriptionMapping {
    SubscriptionMapping {
        request_id: media.request_id.clone(),
        media_type: media.media_type.clone(),
        tmdb_id: media.tmdb_id,
        season,
        subscription_id,
        title: title.to_string(),
        owned,
        created_at: now_string(),
    }
}

fn status(context: &PluginContext) -> Result<Value, String> {
    let _lock = lock_state(&context.data_dir)?;
    let state = load_state(&context.data_dir);
    Ok(json!({
        "notice": format!("已记录 {} 个 Seerr 请求映射", state.mappings.len()),
        "items": [
            {
                "label": "Webhook 路径",
                "value": format!("/api/plugins/{PLUGIN_ID}/external/{WEBHOOK_ACTION}")
            },
            {"label": "请求映射", "value": state.mappings.len()},
            {"label": "已创建订阅", "value": state.summary.created},
            {"label": "已同步取消", "value": state.summary.deleted},
            {"label": "处理失败", "value": state.summary.errors}
        ]
    }))
}

fn lock_state(data_dir: &Path) -> Result<File, String> {
    let path = data_dir.join("state.lock");
    let file = OpenOptions::new()
        .create(true)
        .read(true)
        .write(true)
        .open(&path)
        .map_err(|error| format!("打开 Seerr 插件状态锁失败: {error}"))?;
    file.lock_exclusive()
        .map_err(|error| format!("锁定 Seerr 插件状态失败: {error}"))?;
    Ok(file)
}

fn load_state(data_dir: &Path) -> PluginState {
    fs::read(data_dir.join("state.json"))
        .ok()
        .and_then(|bytes| serde_json::from_slice(&bytes).ok())
        .unwrap_or_default()
}

fn save_state(data_dir: &Path, state: &PluginState) -> Result<(), String> {
    write_json_atomic(&data_dir.join("state.json"), state)?;
    write_json_atomic(
        &data_dir.join("history.json"),
        &json!({
            "summary": state.summary,
            "items": state.items,
        }),
    )
}

fn write_json_atomic(path: &Path, value: &impl Serialize) -> Result<(), String> {
    let temporary = path.with_extension("json.tmp");
    let bytes = serde_json::to_vec_pretty(value)
        .map_err(|error| format!("序列化 Seerr 插件状态失败: {error}"))?;
    let mut file = File::create(&temporary)
        .map_err(|error| format!("创建 Seerr 插件临时文件失败: {error}"))?;
    file.write_all(&bytes)
        .and_then(|_| file.sync_all())
        .map_err(|error| format!("写入 Seerr 插件状态失败: {error}"))?;
    fs::rename(&temporary, path).map_err(|error| format!("替换 Seerr 插件状态文件失败: {error}"))
}

fn read_stdin_json() -> Result<Value, String> {
    serde_json::from_reader(std::io::stdin())
        .map_err(|error| format!("读取 Seerr Webhook JSON 失败: {error}"))
}

fn required_env(name: &str) -> Result<String, String> {
    env::var(name)
        .ok()
        .filter(|value| !value.trim().is_empty())
        .ok_or_else(|| format!("缺少 {name} 环境变量"))
}

fn non_empty(value: &str) -> Option<&str> {
    let value = value.trim();
    (!value.is_empty()).then_some(value)
}

fn now_string() -> String {
    Local::now().format("%Y-%m-%d %H:%M:%S").to_string()
}

fn default_true() -> bool {
    true
}

fn default_quality() -> String {
    "all".to_string()
}

fn default_four_k_resolution() -> String {
    "2160p".to_string()
}

fn default_history_limit() -> usize {
    200
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::{
        Json, Router,
        extract::{Query, State},
        http::StatusCode as AxumStatusCode,
        routing::get,
    };
    use std::sync::Arc;
    use tokio::sync::Mutex;

    #[derive(Clone, Default)]
    struct MockApiState {
        payloads: Arc<Mutex<Vec<Value>>>,
    }

    fn webhook(value: Value) -> SeerrWebhook {
        serde_json::from_value(value).unwrap()
    }

    async fn mock_details() -> Json<Value> {
        Json(json!({
            "title": "Example Show",
            "year": 2026,
            "suggested_secondary_category": "TV",
            "details": {
                "name": "Example Show",
                "first_air_date": "2026-01-02",
                "poster_path": "/poster.jpg",
                "backdrop_path": "/backdrop.jpg",
                "vote_average": 8.2,
                "overview": "Example overview",
                "seasons": [
                    {"season_number": 1, "episode_count": 8},
                    {"season_number": 2, "episode_count": 10}
                ]
            }
        }))
    }

    async fn mock_list(
        State(state): State<MockApiState>,
        Query(query): Query<HashMap<String, String>>,
    ) -> Json<Value> {
        let season = query
            .get("season")
            .and_then(|value| value.parse::<i32>().ok());
        let payloads = state.payloads.lock().await;
        let subscriptions = payloads
            .iter()
            .enumerate()
            .filter(|(_, payload)| {
                payload.get("season").and_then(Value::as_i64) == season.map(i64::from)
            })
            .map(|(index, _)| json!({"id": 101 + index as i64, "state": "active"}))
            .collect::<Vec<_>>();
        Json(json!(subscriptions))
    }

    async fn mock_create(
        State(state): State<MockApiState>,
        Json(payload): Json<Value>,
    ) -> (AxumStatusCode, Json<Value>) {
        let mut payloads = state.payloads.lock().await;
        payloads.push(payload);
        (
            AxumStatusCode::CREATED,
            Json(json!({"id": 100 + payloads.len() as i64})),
        )
    }

    async fn mock_context() -> (PluginContext, MockApiState, tokio::task::JoinHandle<()>) {
        let state = MockApiState::default();
        let app = Router::new()
            .route("/api/search/tmdb/details", get(mock_details))
            .route("/api/subscriptions", get(mock_list).post(mock_create))
            .with_state(state.clone());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let unique = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let data_dir = std::env::temp_dir().join(format!(
            "mediary-seerr-test-{}-{unique}",
            std::process::id()
        ));
        fs::create_dir_all(&data_dir).unwrap();
        (
            PluginContext {
                api_url: format!("http://{address}/api"),
                token: "plugin-token".to_string(),
                data_dir,
                settings: Settings::default(),
                client: Client::new(),
            },
            state,
            server,
        )
    }

    #[test]
    fn parses_default_seerr_movie_payload() {
        let payload = webhook(json!({
            "notification_type": "MEDIA_AUTO_APPROVED",
            "event": "Movie Request Automatically Approved",
            "subject": "Example Movie (2026)",
            "media": {
                "media_type": "movie",
                "tmdbId": "123",
                "status": "PENDING",
                "status4k": "UNKNOWN"
            },
            "request": {
                "request_id": "42",
                "requestedBy_username": "admin"
            },
            "extra": []
        }));
        let media = approved_media(&payload).unwrap();
        assert_eq!(media.request_id, "42");
        assert_eq!(media.tmdb_id, 123);
        assert_eq!(media.media_type, "movie");
        assert_eq!(media.seasons, vec![None]);
        assert_eq!(media.requested_by.as_deref(), Some("admin"));
        assert!(!media.is_4k);
    }

    #[test]
    fn parses_and_deduplicates_requested_seasons() {
        let payload = webhook(json!({
            "notification_type": "MEDIA_APPROVED",
            "event": "Series Request Approved",
            "subject": "Example Show (2026)",
            "media": {"media_type": "tv", "tmdbId": 456},
            "request": {"request_id": 77},
            "extra": [{"name": "Requested Seasons", "value": "3, 1, 3, 2"}]
        }));
        let media = approved_media(&payload).unwrap();
        assert_eq!(media.seasons, vec![Some(1), Some(2), Some(3)]);
    }

    #[tokio::test]
    async fn creates_each_requested_season_and_reuses_idempotency_mapping() {
        let (context, api, server) = mock_context().await;
        let payload = webhook(json!({
            "notification_type": "MEDIA_APPROVED",
            "event": "Series Request Approved",
            "subject": "Fallback Show (2026)",
            "message": "Fallback overview",
            "media": {"media_type": "tv", "tmdbId": 456},
            "request": {
                "request_id": 77,
                "requestedBy_username": "requester"
            },
            "extra": [{"name": "Requested Seasons", "value": "1, 2"}]
        }));
        let mut state = PluginState::default();

        let first = process_approved(&context, &payload, &mut state)
            .await
            .unwrap();
        assert_eq!(first["created"], 2);
        assert_eq!(state.mappings.len(), 2);
        let sent = api.payloads.lock().await;
        assert_eq!(sent.len(), 2);
        assert_eq!(sent[0]["season"], 1);
        assert_eq!(sent[0]["expected_episodes"], 8);
        assert_eq!(sent[1]["season"], 2);
        assert_eq!(sent[1]["expected_episodes"], 10);
        assert_eq!(sent[0]["source_owners"], "requester");
        drop(sent);
        let persisted = load_state(&context.data_dir);
        assert_eq!(persisted.mappings.len(), 2);
        assert!(persisted.mappings.values().all(|mapping| mapping.owned));

        let second = process_approved(&context, &payload, &mut state)
            .await
            .unwrap();
        assert_eq!(second["created"], 0);
        assert_eq!(second["existing"], 2);
        assert_eq!(api.payloads.lock().await.len(), 2);
        server.abort();
        fs::remove_dir_all(&context.data_dir).unwrap();
    }

    #[test]
    fn rejects_tv_request_without_seasons() {
        let payload = webhook(json!({
            "notification_type": "MEDIA_APPROVED",
            "media": {"media_type": "tv", "tmdbId": 456},
            "request": {"request_id": 77}
        }));
        assert!(
            approved_media(&payload)
                .unwrap_err()
                .contains("Requested Seasons")
        );
    }

    #[test]
    fn detects_4k_request_from_event_or_media_status() {
        let event_payload = webhook(json!({"event": "4K Movie Request Approved"}));
        assert!(is_4k_request(&event_payload));

        let status_payload = webhook(json!({
            "media": {"status": "UNKNOWN", "status4k": "PENDING"}
        }));
        assert!(is_4k_request(&status_payload));
    }

    #[test]
    fn strips_terminal_year_without_changing_other_titles() {
        assert_eq!(strip_subject_year("Example (2026)"), "Example");
        assert_eq!(subject_year("Example (2026)"), Some(2026));
        assert_eq!(strip_subject_year("Example 2026"), "Example 2026");
        assert_eq!(strip_subject_year("中文"), "中文");
    }

    #[test]
    fn extracts_tmdb_poster_path_from_webhook_image() {
        assert_eq!(
            tmdb_image_path("https://image.tmdb.org/t/p/w600_and_h900_bestv2/poster.jpg"),
            Some("/poster.jpg".to_string())
        );
        assert_eq!(tmdb_image_path("https://example.com/poster.jpg"), None);
    }

    #[test]
    fn manifest_declares_external_webhook_and_narrow_scopes() {
        let manifest: Value =
            serde_json::from_str(include_str!("../../seerr/plugin.json")).unwrap();
        assert_eq!(manifest["id"], PLUGIN_ID);
        assert_eq!(manifest["external_actions"], json!([WEBHOOK_ACTION]));
        assert_eq!(
            manifest["requested_scopes"],
            json!(["catalog:read", "subscriptions:read", "subscriptions:write"])
        );
        assert_eq!(manifest["auto_grant_scopes"], false);
    }
}
