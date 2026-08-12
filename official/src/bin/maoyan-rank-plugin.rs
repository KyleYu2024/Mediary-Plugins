use chrono::{Datelike, Duration as ChronoDuration, Local};
use reqwest::Client;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{
    collections::{HashSet, VecDeque},
    env, fs,
    path::{Path, PathBuf},
    time::Duration,
};

const MAOYAN_BASE_URL: &str = "https://piaofang.maoyan.com";
const USER_AGENT: &str =
    "Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) AppleWebKit/537.36 Chrome/121 Safari/537.36";
const MAX_HISTORY_ITEMS: usize = 5_000;

#[derive(Clone, Deserialize)]
struct Settings {
    #[serde(default)]
    clear: bool,
    #[serde(default = "default_rank_types", rename = "type", alias = "rank_types")]
    rank_types: Vec<String>,
    #[serde(default = "default_limit", rename = "num", alias = "limit_per_rank")]
    movie_num: usize,
    #[serde(default)]
    all_enabled: bool,
    #[serde(default = "default_limit")]
    all_num: usize,
    #[serde(default)]
    tx_enabled: bool,
    #[serde(default = "default_limit")]
    tx_num: usize,
    #[serde(default)]
    iqy_enabled: bool,
    #[serde(default = "default_limit")]
    iqy_num: usize,
    #[serde(default)]
    mg_enabled: bool,
    #[serde(default = "default_limit")]
    mg_num: usize,
    #[serde(default)]
    yk_enabled: bool,
    #[serde(default = "default_limit")]
    yk_num: usize,
}

#[derive(Clone)]
struct PluginContext {
    api_url: String,
    token: String,
    data_dir: PathBuf,
    client: Client,
    settings: Settings,
}

#[derive(Clone, Serialize)]
struct Candidate {
    title: String,
    media_type: String,
    source: String,
    release_info: String,
    platform: String,
    year: Option<i32>,
}

#[derive(Default, Deserialize, Serialize)]
struct History {
    #[serde(default)]
    processed: VecDeque<String>,
}

#[derive(Default, Deserialize, Serialize)]
struct SubscriptionData {
    #[serde(default)]
    updated_at: String,
    #[serde(default)]
    summary: SubscriptionSummary,
    #[serde(default)]
    items: VecDeque<SubscriptionRecord>,
}

#[derive(Default, Deserialize, Serialize)]
struct SubscriptionSummary {
    total: usize,
    movies: usize,
    series: usize,
    platforms: usize,
}

#[derive(Deserialize, Serialize)]
struct SubscriptionRecord {
    title: String,
    tmdb_id: String,
    media_type: String,
    source: String,
    platform: String,
    year: Option<i32>,
    subscribed_at: String,
}

#[derive(Serialize)]
struct RunReport {
    ran_at: String,
    fetched: usize,
    considered: usize,
    resolved: usize,
    subscribed: usize,
    skipped_history: usize,
    skipped_existing: usize,
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
    let action = env::var("MEDIARY_PLUGIN_ACTION").unwrap_or_default();
    if action != "refresh" {
        return Err(format!("不支持的猫眼榜单动作: {action}"));
    }
    let context =
        PluginContext::from_env().map_err(|error| format!("猫眼榜单插件初始化失败: {error}"))?;
    if context.settings.clear {
        let _ = fs::remove_file(context.data_dir.join("history.json"));
        let _ = fs::remove_file(context.data_dir.join("records.json"));
        let _ = reset_runtime_flag("clear");
    }
    let report = refresh(&context).await?;
    let notice = format!(
        "猫眼榜单刷新完成：获取 {}，新增订阅 {}，失败 {}。",
        report.fetched,
        report.subscribed,
        report.failures.len()
    );
    println!("{}", json!({"notice": notice, "report": report}));
    Ok(())
}

impl PluginContext {
    fn from_env() -> Result<Self, String> {
        let api_url = required_env("MEDIARY_PLUGIN_API_URL")?;
        let token = required_env("MEDIARY_PLUGIN_TOKEN")?;
        let data_dir = PathBuf::from(required_env("MEDIARY_PLUGIN_DATA_DIR")?);
        fs::create_dir_all(&data_dir).map_err(|error| error.to_string())?;
        let settings = env::var("MEDIARY_PLUGIN_SETTINGS_JSON")
            .ok()
            .filter(|value| !value.trim().is_empty())
            .map(|raw| serde_json::from_str(&raw).map_err(|error| error.to_string()))
            .transpose()?
            .unwrap_or_else(default_settings);
        let client = Client::builder()
            .user_agent(USER_AGENT)
            .timeout(Duration::from_secs(20))
            .build()
            .map_err(|error| error.to_string())?;
        Ok(Self {
            api_url: api_url.trim_end_matches('/').to_string(),
            token,
            data_dir,
            client,
            settings,
        })
    }
}

async fn refresh(context: &PluginContext) -> Result<RunReport, String> {
    let mut report = RunReport {
        ran_at: Local::now().to_rfc3339(),
        fetched: 0,
        considered: 0,
        resolved: 0,
        subscribed: 0,
        skipped_history: 0,
        skipped_existing: 0,
        failures: Vec::new(),
    };
    let candidates = fetch_rankings(context).await?;
    report.fetched = candidates.len();
    let mut existing_subscriptions = fetch_existing_subscription_keys(context).await?;
    let mut history = load_json::<History>(&context.data_dir.join("history.json"));
    let mut subscription_data =
        load_json::<SubscriptionData>(&context.data_dir.join("records.json"));
    let mut processed = history.processed.iter().cloned().collect::<HashSet<_>>();

    for candidate in candidates {
        let key = candidate_key(&candidate);
        if processed.contains(&key) {
            report.skipped_history += 1;
            continue;
        }
        report.considered += 1;
        let resolved = match resolve_tmdb(context, &candidate).await {
            Ok(resolved) => {
                report.resolved += 1;
                resolved
            }
            Err(error) => {
                report
                    .failures
                    .push(format!("{}: {error}", candidate.title));
                continue;
            }
        };
        let identity = resolved_subscription_key(&candidate, &resolved);
        if existing_subscriptions.contains(&identity) {
            history.processed.push_back(key.clone());
            processed.insert(key);
            report.skipped_existing += 1;
            continue;
        }
        match create_subscription(context, &candidate, &resolved).await {
            Ok(()) => {
                history.processed.push_back(key.clone());
                processed.insert(key);
                existing_subscriptions.insert(identity);
                subscription_data
                    .items
                    .push_front(subscription_record(&candidate, &resolved));
                report.subscribed += 1;
            }
            Err(error) => report
                .failures
                .push(format!("{}: {error}", candidate.title)),
        }
    }

    while history.processed.len() > MAX_HISTORY_ITEMS {
        history.processed.pop_front();
    }
    while subscription_data.items.len() > MAX_HISTORY_ITEMS {
        subscription_data.items.pop_back();
    }
    update_subscription_summary(&mut subscription_data);
    write_json(context.data_dir.join("history.json"), &history)?;
    write_json(context.data_dir.join("records.json"), &subscription_data)?;
    write_json(context.data_dir.join("last-run.json"), &report)?;
    Ok(report)
}

async fn fetch_rankings(context: &PluginContext) -> Result<Vec<Candidate>, String> {
    let mut output = Vec::new();
    let types = normalized_values(&context.settings.rank_types, default_rank_types());
    let limit = context.settings.movie_num.max(1);

    if types.iter().any(|kind| kind == "movie") {
        let payload = get_json(context, &format!("{MAOYAN_BASE_URL}/dashboard-ajax/movie")).await?;
        for item in json_array(&payload, &["movieList", "list"])
            .into_iter()
            .take(limit)
        {
            let info = item.get("movieInfo").unwrap_or(&item);
            if let Some(title) = json_text(info, "movieName") {
                let release_info = json_text(info, "releaseInfo").unwrap_or_default();
                output.push(candidate(
                    title,
                    "movie",
                    "movie",
                    release_info,
                    String::new(),
                ));
            }
        }
    }

    if types.iter().any(|kind| kind == "web-movie") {
        let date = Local::now().format("%Y-%m-%d");
        let url = format!(
            "{MAOYAN_BASE_URL}/dashboard/webMaoYanHotData?seriesType=0&platform=20&date={date}&networkHot=3"
        );
        let payload = get_json(context, &url).await?;
        for item in json_array(&payload, &["data", "list"])
            .into_iter()
            .take(limit)
        {
            if let Some(title) = json_text(&item, "name") {
                output.push(candidate(
                    title,
                    "movie",
                    "web-movie",
                    String::new(),
                    json_text(&item, "platformDesc").unwrap_or_default(),
                ));
            }
        }
    }

    for (rank_type, series_type) in [("web-heat", 0), ("web-tv", 1), ("zongyi", 2)] {
        if !types.iter().any(|kind| kind == rank_type) {
            continue;
        }
        for (platform, platform_limit) in context.settings.platform_limits() {
            let platform_type = platform_type(platform);
            let url = format!(
                "{MAOYAN_BASE_URL}/dashboard/webHeatData?seriesType={series_type}&platformType={platform_type}&showDate=2"
            );
            let payload = get_json(context, &url).await?;
            for item in json_array(&payload, &["dataList", "list"])
                .into_iter()
                .take(platform_limit.max(1))
            {
                let info = item.get("seriesInfo").unwrap_or(&item);
                if let Some(title) = json_text(info, "name") {
                    output.push(candidate(
                        title,
                        "tv",
                        rank_type,
                        json_text(info, "releaseInfo").unwrap_or_default(),
                        json_text(info, "platformDesc").unwrap_or_else(|| platform.to_string()),
                    ));
                }
            }
        }
    }

    let mut deduped = HashSet::new();
    output.retain(|item| deduped.insert(format!("{}:{}", item.media_type, item.title)));
    Ok(output)
}

async fn get_json(context: &PluginContext, url: &str) -> Result<Value, String> {
    context
        .client
        .get(url)
        .send()
        .await
        .map_err(|error| error.to_string())?
        .error_for_status()
        .map_err(|error| error.to_string())?
        .json()
        .await
        .map_err(|error| error.to_string())
}

#[derive(Deserialize)]
struct ResolvedMedia {
    tmdb_id: String,
    #[serde(default)]
    title: String,
    #[serde(default, rename = "type")]
    media_type: String,
    year: Option<i32>,
    poster_path: Option<String>,
    backdrop_path: Option<String>,
    vote_average: Option<f64>,
    description: Option<String>,
    expected_episodes: Option<i32>,
}

async fn resolve_tmdb(
    context: &PluginContext,
    candidate: &Candidate,
) -> Result<ResolvedMedia, String> {
    let body = json!({
        "title": candidate.title,
        "year": candidate.year,
        "media_type": candidate.media_type,
    });
    let response = plugin_api(context, "/tmdb/resolve", body).await?;
    if response.get("status").and_then(Value::as_str) == Some("failed") {
        return Err(response
            .get("message")
            .and_then(Value::as_str)
            .unwrap_or("TMDB 未匹配")
            .to_string());
    }
    serde_json::from_value(response.get("data").cloned().unwrap_or(response))
        .map_err(|error| error.to_string())
}

async fn create_subscription(
    context: &PluginContext,
    candidate: &Candidate,
    resolved: &ResolvedMedia,
) -> Result<(), String> {
    let media_type = if resolved.media_type.trim().is_empty() {
        candidate.media_type.as_str()
    } else {
        resolved.media_type.as_str()
    };
    let body = json!({
        "tmdb_id": resolved.tmdb_id,
        "name": if resolved.title.trim().is_empty() { &candidate.title } else { &resolved.title },
        "year": resolved.year.or(candidate.year),
        "season": (media_type == "tv").then_some(1),
        "media_type": media_type,
        "poster_path": resolved.poster_path,
        "backdrop_path": resolved.backdrop_path,
        "vote_average": resolved.vote_average,
        "description": resolved.description,
        "expected_episodes": resolved.expected_episodes,
    });
    plugin_api(context, "/subscriptions", body)
        .await
        .map(|_| ())
}

async fn fetch_existing_subscription_keys(
    context: &PluginContext,
) -> Result<HashSet<String>, String> {
    let payload = plugin_api_get(context, "/subscriptions").await?;
    let subscriptions = payload
        .as_array()
        .ok_or_else(|| "Mediary 订阅列表响应格式无效".to_string())?;
    Ok(subscriptions
        .iter()
        .filter_map(existing_subscription_key)
        .collect())
}

fn existing_subscription_key(subscription: &Value) -> Option<String> {
    let media_type = subscription.get("media_type")?.as_str()?;
    let tmdb_id = value_as_id(subscription.get("tmdb_id")?)?;
    let season = subscription
        .get("season")
        .and_then(Value::as_i64)
        .and_then(|value| i32::try_from(value).ok());
    Some(subscription_key(media_type, &tmdb_id, season))
}

fn resolved_subscription_key(candidate: &Candidate, resolved: &ResolvedMedia) -> String {
    let media_type = if resolved.media_type.trim().is_empty() {
        candidate.media_type.as_str()
    } else {
        resolved.media_type.as_str()
    };
    subscription_key(
        media_type,
        resolved.tmdb_id.trim(),
        (media_type == "tv").then_some(1),
    )
}

fn subscription_key(media_type: &str, tmdb_id: &str, season: Option<i32>) -> String {
    let media_type = media_type.trim().to_ascii_lowercase();
    let season = if media_type == "tv" {
        season.unwrap_or(1)
    } else {
        0
    };
    format!("{media_type}:{}:{season}", tmdb_id.trim())
}

fn value_as_id(value: &Value) -> Option<String> {
    value
        .as_str()
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(ToString::to_string)
        .or_else(|| value.as_i64().map(|value| value.to_string()))
}

async fn plugin_api_get(context: &PluginContext, path: &str) -> Result<Value, String> {
    let response = context
        .client
        .get(format!("{}{}", context.api_url, path))
        .bearer_auth(&context.token)
        .send()
        .await
        .map_err(|error| error.to_string())?;
    parse_plugin_api_response(response).await
}

async fn plugin_api(context: &PluginContext, path: &str, body: Value) -> Result<Value, String> {
    let response = context
        .client
        .post(format!("{}{}", context.api_url, path))
        .bearer_auth(&context.token)
        .json(&body)
        .send()
        .await
        .map_err(|error| error.to_string())?;
    parse_plugin_api_response(response).await
}

async fn parse_plugin_api_response(response: reqwest::Response) -> Result<Value, String> {
    let status = response.status();
    let payload = response.text().await.unwrap_or_default();
    if !status.is_success() {
        return Err(format!("Mediary API {status}: {}", truncate(&payload, 200)));
    }
    serde_json::from_str(&payload).map_err(|error| error.to_string())
}

fn candidate(
    title: String,
    media_type: &str,
    source: &str,
    release_info: String,
    platform: String,
) -> Candidate {
    Candidate {
        title,
        media_type: media_type.to_string(),
        source: source.to_string(),
        year: year_from_release_info(&release_info),
        release_info,
        platform,
    }
}

fn json_array(value: &Value, path: &[&str]) -> Vec<Value> {
    let mut current = value;
    for key in path {
        let Some(next) = current.get(key) else {
            return Vec::new();
        };
        current = next;
    }
    current.as_array().cloned().unwrap_or_default()
}

fn json_text(value: &Value, key: &str) -> Option<String> {
    value
        .get(key)
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(ToString::to_string)
}

fn platform_type(platform: &str) -> &'static str {
    match platform {
        "tencent" => "3",
        "iqiyi" => "2",
        "youku" => "1",
        "mango" => "7",
        _ => "",
    }
}

fn year_from_release_info(release_info: &str) -> Option<i32> {
    let digits = release_info
        .chars()
        .filter(|character| character.is_ascii_digit())
        .collect::<String>();
    let days = digits.parse::<i64>().ok()?;
    Some((Local::now() - ChronoDuration::days(days)).year())
}

fn candidate_key(candidate: &Candidate) -> String {
    format!(
        "{}:{}:{}:{}",
        candidate.media_type,
        candidate.source,
        candidate.title,
        candidate.year.unwrap_or_default()
    )
}

fn load_json<T: for<'de> Deserialize<'de> + Default>(path: &Path) -> T {
    fs::read_to_string(path)
        .ok()
        .and_then(|raw| serde_json::from_str(&raw).ok())
        .unwrap_or_default()
}

fn subscription_record(candidate: &Candidate, resolved: &ResolvedMedia) -> SubscriptionRecord {
    SubscriptionRecord {
        title: if resolved.title.trim().is_empty() {
            candidate.title.clone()
        } else {
            resolved.title.clone()
        },
        tmdb_id: resolved.tmdb_id.clone(),
        media_type: if resolved.media_type == "movie" || candidate.media_type == "movie" {
            "电影".to_string()
        } else {
            "剧集/综艺".to_string()
        },
        source: rank_source_label(&candidate.source).to_string(),
        platform: if candidate.platform.trim().is_empty() {
            "猫眼".to_string()
        } else {
            candidate.platform.clone()
        },
        year: resolved.year.or(candidate.year),
        subscribed_at: Local::now().to_rfc3339(),
    }
}

fn update_subscription_summary(data: &mut SubscriptionData) {
    data.summary.total = data.items.len();
    data.summary.movies = data
        .items
        .iter()
        .filter(|item| item.media_type == "电影")
        .count();
    data.summary.series = data.summary.total.saturating_sub(data.summary.movies);
    data.summary.platforms = data
        .items
        .iter()
        .map(|item| item.platform.as_str())
        .collect::<HashSet<_>>()
        .len();
    data.updated_at = Local::now().to_rfc3339();
}

fn rank_source_label(source: &str) -> &'static str {
    match source {
        "movie" => "电影票房榜单",
        "web-heat" => "电视剧热度榜单",
        "web-tv" => "网剧热度榜单",
        "zongyi" => "综艺榜单",
        "web-movie" => "网络电影榜单",
        _ => "猫眼榜单",
    }
}

fn write_json<T: Serialize>(path: PathBuf, value: &T) -> Result<(), String> {
    let encoded = serde_json::to_string_pretty(value).map_err(|error| error.to_string())?;
    let temporary = path.with_extension("json.tmp");
    fs::write(&temporary, format!("{encoded}\n")).map_err(|error| error.to_string())?;
    fs::rename(temporary, path).map_err(|error| error.to_string())
}

fn reset_runtime_flag(key: &str) -> Result<(), String> {
    let config_path = PathBuf::from(required_env("MEDIARY_PLUGIN_CONFIG_PATH")?);
    let raw = fs::read_to_string(&config_path).map_err(|error| error.to_string())?;
    let mut config: Value = serde_json::from_str(&raw).map_err(|error| error.to_string())?;
    if let Some(settings) = config.get_mut("settings").and_then(Value::as_object_mut) {
        settings.insert(key.to_string(), Value::Bool(false));
    }
    let encoded = serde_json::to_string_pretty(&config).map_err(|error| error.to_string())?;
    let temporary = config_path.with_extension("json.tmp");
    fs::write(&temporary, format!("{encoded}\n")).map_err(|error| error.to_string())?;
    fs::rename(temporary, config_path).map_err(|error| error.to_string())
}

impl Settings {
    fn platform_limits(&self) -> Vec<(&'static str, usize)> {
        let mut values = Vec::new();
        if self.all_enabled {
            values.push(("all", self.all_num));
        }
        if self.tx_enabled {
            values.push(("tencent", self.tx_num));
        }
        if self.iqy_enabled {
            values.push(("iqiyi", self.iqy_num));
        }
        if self.mg_enabled {
            values.push(("mango", self.mg_num));
        }
        if self.yk_enabled {
            values.push(("youku", self.yk_num));
        }
        values
    }
}

fn required_env(name: &str) -> Result<String, String> {
    env::var(name).map_err(|_| format!("缺少运行时环境变量 {name}"))
}

fn normalized_values(values: &[String], defaults: Vec<String>) -> Vec<String> {
    let values = values
        .iter()
        .map(|value| value.trim().to_ascii_lowercase())
        .filter(|value| !value.is_empty())
        .collect::<Vec<_>>();
    if values.is_empty() { defaults } else { values }
}

fn default_settings() -> Settings {
    Settings {
        clear: false,
        rank_types: default_rank_types(),
        movie_num: default_limit(),
        all_enabled: false,
        all_num: default_limit(),
        tx_enabled: false,
        tx_num: default_limit(),
        iqy_enabled: false,
        iqy_num: default_limit(),
        mg_enabled: false,
        mg_num: default_limit(),
        yk_enabled: false,
        yk_num: default_limit(),
    }
}

fn default_rank_types() -> Vec<String> {
    vec![
        "movie".to_string(),
        "web-heat".to_string(),
        "web-tv".to_string(),
        "zongyi".to_string(),
        "web-movie".to_string(),
    ]
}

fn default_limit() -> usize {
    10
}

fn truncate(value: &str, max_chars: usize) -> String {
    value.chars().take(max_chars).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn derives_year_from_maoyan_release_age() {
        assert_eq!(year_from_release_info("上映2天"), Some(Local::now().year()));
        assert_eq!(year_from_release_info(""), None);
    }

    #[test]
    fn platform_configuration_keeps_only_enabled_sources() {
        let mut settings = default_settings();
        settings.all_enabled = true;
        settings.all_num = 3;
        settings.tx_enabled = true;
        settings.tx_num = 2;
        assert_eq!(settings.platform_limits(), vec![("all", 3), ("tencent", 2)]);
    }

    #[test]
    fn subscription_identity_matches_existing_movies() {
        let existing = json!({
            "media_type": "movie",
            "tmdb_id": 1305665,
            "season": null
        });
        assert_eq!(
            existing_subscription_key(&existing),
            Some("movie:1305665:0".to_string())
        );
    }

    #[test]
    fn subscription_identity_matches_tv_seasons() {
        let existing = json!({
            "media_type": "tv",
            "tmdb_id": 273114,
            "season": 1
        });
        let candidate = candidate(
            "悬案".to_string(),
            "tv",
            "web-tv",
            String::new(),
            String::new(),
        );
        let resolved = ResolvedMedia {
            tmdb_id: "273114".to_string(),
            title: "悬案".to_string(),
            media_type: "tv".to_string(),
            year: None,
            poster_path: None,
            backdrop_path: None,
            vote_average: None,
            description: None,
            expected_episodes: None,
        };
        assert_eq!(
            existing_subscription_key(&existing),
            Some(resolved_subscription_key(&candidate, &resolved))
        );
    }
}
