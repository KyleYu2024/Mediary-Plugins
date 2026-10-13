use chrono::{Datelike, Duration as ChronoDuration, FixedOffset, Local, Utc};
use regex::Regex;
use reqwest::Client;
use reqwest::header::{COOKIE, HeaderMap, HeaderValue, REFERER, SET_COOKIE};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{
    collections::{HashSet, VecDeque},
    env, fs,
    path::{Path, PathBuf},
    time::Duration,
};

#[path = "../platform_ranks.rs"]
mod platform_ranks;

const MAOYAN_BASE_URL: &str = "https://piaofang.maoyan.com";
const USER_AGENT: &str =
    "Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) AppleWebKit/537.36 Chrome/121 Safari/537.36";
const MAX_HISTORY_ITEMS: usize = 5_000;
const DEFAULT_TV_SEASON: i32 = 1;
const MAOYAN_HEAT_PAGE: &str = "https://piaofang.maoyan.com/dashboard/web-heat";

fn heat_rank_url(series_type: i32, platform: &str, date: &str) -> String {
    let mut url = format!(
        "{MAOYAN_BASE_URL}/i/api/encrypt/dashboard/webHeatData?seriesType={series_type}&showDate={date}"
    );
    // The current website omits platformType for the all-platform chart.
    if platform != "all" {
        url.push_str(&format!("&platformType={}", platform_type(platform)));
    }
    url
}

fn web_movie_rank_url(date: &str) -> String {
    format!(
        "{MAOYAN_BASE_URL}/i/api/encrypt/dashboard/webHeatNetData?showDate={date}&platformType=3&dateType=0&rankType=0"
    )
}

fn beijing_rank_date(now: chrono::DateTime<Utc>) -> String {
    now.with_timezone(&FixedOffset::east_opt(8 * 3600).unwrap())
        .format("%Y%m%d")
        .to_string()
}

// Only anonymous cookies supplied by Maoyan are used, in memory for this refresh.
async fn rank_session_headers(context: &PluginContext) -> Result<HeaderMap, String> {
    let response = context
        .client
        .get(MAOYAN_HEAT_PAGE)
        .send()
        .await
        .map_err(|e| e.to_string())?
        .error_for_status()
        .map_err(|e| e.to_string())?;
    if response.url().host_str() != Some("piaofang.maoyan.com") {
        return Err("猫眼官网页面重定向到其他域名".into());
    }
    let cookies = response
        .headers()
        .get_all(SET_COOKIE)
        .iter()
        .filter_map(|value| value.to_str().ok())
        .filter_map(|cookie| cookie.split(';').next())
        .filter(|cookie| cookie.contains('='))
        .collect::<Vec<_>>()
        .join("; ");
    let html = response.text().await.map_err(|e| e.to_string())?;
    let mut headers = parse_rank_page_headers(&html)?;
    if !cookies.is_empty() {
        headers.insert(
            COOKIE,
            HeaderValue::from_str(&cookies).map_err(|_| "猫眼匿名会话格式无效")?,
        );
    }
    Ok(headers)
}

fn parse_rank_page_headers(html: &str) -> Result<HeaderMap, String> {
    let mut headers = HeaderMap::new();
    headers.insert(REFERER, HeaderValue::from_static(MAOYAN_HEAT_PAGE));
    let tags = Regex::new(r"(?is)<meta\b[^>]*>").unwrap();
    let attributes = Regex::new(r#"(?i)\b(name|content)\s*=\s*(?:"([^"]*)"|'([^']*)')"#).unwrap();
    for tag in tags.find_iter(html) {
        let mut name = None;
        let mut content = None;
        for capture in attributes.captures_iter(tag.as_str()) {
            let value = capture.get(2).or_else(|| capture.get(3)).unwrap().as_str();
            match capture[1].to_ascii_lowercase().as_str() {
                "name" => name = Some(value),
                "content" => content = Some(value),
                _ => {}
            }
        }
        let key = match name {
            Some("csrf") => "uid",
            Some("deviceId") => "uuid",
            Some("channelId") => "channelid",
            _ => continue,
        };
        if let Some(value) = content.filter(|v| !v.is_empty()) {
            headers.insert(
                key,
                HeaderValue::from_str(value).map_err(|_| "猫眼页面请求头格式无效")?,
            );
        }
    }
    Ok(headers)
}

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
    source_rank: Option<usize>,
    source_heat: Option<f64>,
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
    #[serde(default, skip_serializing_if = "Option::is_none")]
    source_rank: Option<usize>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    source_heat: Option<f64>,
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
    warnings: Vec<String>,
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
        "猫眼榜单刷新完成：获取 {}，新增订阅 {}，失败 {}，提示 {}。",
        report.fetched,
        report.subscribed,
        report.failures.len(),
        report.warnings.len()
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
        warnings: Vec::new(),
    };
    let candidates = fetch_rankings(context, &mut report).await?;
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

async fn fetch_rankings(
    context: &PluginContext,
    report: &mut RunReport,
) -> Result<Vec<Candidate>, String> {
    let mut output = Vec::new();
    let types = normalized_values(&context.settings.rank_types, default_rank_types());
    let limit = context.settings.movie_num.max(1);
    let date = beijing_rank_date(Utc::now());
    let needs_heat = types.iter().any(|kind| kind == "web-movie")
        || (!context.settings.platform_limits().is_empty()
            && types
                .iter()
                .any(|kind| matches!(kind.as_str(), "web-heat" | "web-tv" | "zongyi")));
    let session_headers = if needs_heat {
        match rank_session_headers(context).await {
            Ok(headers) => headers,
            Err(error) => {
                report
                    .warnings
                    .push(format!("读取猫眼匿名网页会话失败，继续请求榜单: {error}"));
                HeaderMap::new()
            }
        }
    } else {
        HeaderMap::new()
    };

    if types.iter().any(|kind| kind == "movie") {
        let payload =
            match get_json(context, &format!("{MAOYAN_BASE_URL}/dashboard-ajax/movie")).await {
                Ok(payload) => payload,
                Err(error) => {
                    report.failures.push(format!("电影票房榜单: {error}"));
                    Value::Null
                }
            };
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
        let url = web_movie_rank_url(&date);
        let payload = match get_heat_json(context, &url, &session_headers, report).await {
            Ok(payload) => payload,
            Err(error) => {
                report.failures.push(format!("网络电影榜单: {error}"));
                Value::Null
            }
        };
        for item in json_array(&payload, &["dataList", "list"])
            .into_iter()
            .take(limit)
        {
            if let Some(title) = json_text(&item, "movieName").or_else(|| json_text(&item, "name"))
            {
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
            let url = heat_rank_url(series_type, platform, &date);
            let (payload, source) = match get_heat_json(context, &url, &session_headers, report)
                .await
            {
                Ok(payload) => (payload, rank_type),
                Err(error) if rank_type == "web-heat" && platform == "all" => {
                    match fetch_combined_web_heat(context).await {
                        Ok(payload) => {
                            report.warnings.push(
                                format!("新电视剧热度接口不可用（{error}），已改用猫眼公开页面的电视剧＋网剧全网综合榜"),
                            );
                            (payload, "web-heat-combined")
                        }
                        Err(fallback_error) => {
                            report.failures.push(format!(
                                "{} ({platform}): {error}; 备用综合榜: {fallback_error}",
                                rank_source_label(rank_type)
                            ));
                            continue;
                        }
                    }
                }
                Err(error) => {
                    if rank_type == "web-heat" && platform_ranks::source(platform).is_some() {
                        match fetch_platform_candidates(context, platform, platform_limit).await {
                            Ok(candidates) => {
                                let (_, label, _) = platform_ranks::source(platform).unwrap();
                                report.warnings.push(format!("猫眼 {platform} 电视剧榜单不可用（{error}），改用{label}，读取 {} 条；平台榜包含电视剧和网剧", candidates.len()));
                                output.extend(candidates);
                                continue;
                            }
                            Err(fallback_error) => {
                                report.failures.push(format!(
                                    "{} ({platform}): {error}; 平台公开榜单: {fallback_error}",
                                    rank_source_label(rank_type)
                                ));
                                continue;
                            }
                        }
                    }
                    report.failures.push(format!(
                        "{} ({platform}): {error}",
                        rank_source_label(rank_type)
                    ));
                    continue;
                }
            };
            for item in json_array(&payload, &["dataList", "list"])
                .into_iter()
                .take(platform_limit.max(1))
            {
                let info = item.get("seriesInfo").unwrap_or(&item);
                if let Some(title) = json_text(info, "name") {
                    output.push(candidate(
                        title,
                        "tv",
                        source,
                        json_text(info, "releaseInfo").unwrap_or_default(),
                        json_text(info, "platformDesc").unwrap_or_else(|| platform.to_string()),
                    ));
                }
            }
        }
    }

    let mut deduped = HashSet::new();
    output.retain(|item| deduped.insert(format!("{}:{}", item.media_type, item.title)));
    if output.is_empty() && !report.failures.is_empty() {
        write_json(context.data_dir.join("last-run.json"), report)?;
        return Err(format!("猫眼榜单获取失败: {}", report.failures.join("; ")));
    }
    Ok(output)
}

async fn fetch_platform_candidates(
    context: &PluginContext,
    platform: &str,
    limit: usize,
) -> Result<Vec<Candidate>, String> {
    let (source, _, name) = platform_ranks::source(platform).ok_or("该平台未接入公开榜单")?;
    let items = platform_ranks::fetch(&context.client, platform).await?;
    Ok(items
        .into_iter()
        .filter(|item| item.rank <= limit.max(1))
        .map(|item| {
            let mut candidate =
                candidate(item.title, "tv", source, String::new(), name.to_string());
            candidate.year = item.year;
            candidate.source_rank = Some(item.rank);
            candidate.source_heat = item.heat;
            candidate
        })
        .collect())
}

async fn fetch_combined_web_heat(context: &PluginContext) -> Result<Value, String> {
    let html = context
        .client
        .get(format!("{MAOYAN_BASE_URL}/web-heat"))
        .send()
        .await
        .map_err(|error| error.to_string())?
        .error_for_status()
        .map_err(|error| error.to_string())?
        .text()
        .await
        .map_err(|error| error.to_string())?;
    parse_combined_web_heat(&html)
}

fn parse_combined_web_heat(html: &str) -> Result<Value, String> {
    let marker = Regex::new(r"\bvar\s+AppData\s*=\s*").unwrap();
    let assignment = marker.find(html).ok_or("猫眼页面缺少 AppData 榜单数据")?;
    // Read one JSON value, ignoring the surrounding JavaScript and unrelated page config.
    let data = serde_json::Deserializer::from_str(&html[assignment.end()..])
        .into_iter::<Value>()
        .next()
        .ok_or("猫眼页面榜单数据为空")?
        .map_err(|error| format!("猫眼页面榜单数据无效: {error}"))?;
    let list = data
        .pointer("/pageData/webHeatData")
        .and_then(Value::as_array)
        .ok_or("猫眼页面榜单结构已变化")?;
    if list.is_empty()
        || !list.iter().any(|item| {
            item.pointer("/seriesInfo/name")
                .and_then(Value::as_str)
                .is_some()
        })
    {
        return Err("猫眼页面没有有效的综合榜单条目".to_string());
    }
    Ok(json!({"dataList": {"list": list}}))
}

async fn get_json(context: &PluginContext, url: &str) -> Result<Value, String> {
    get_json_with_headers(context, url, &HeaderMap::new()).await
}

async fn get_json_with_headers(
    context: &PluginContext,
    url: &str,
    headers: &HeaderMap,
) -> Result<Value, String> {
    let payload = request_json(context, url, headers).await?;
    let list_path = if url.contains("/dashboard-ajax/movie") {
        "/movieList/list"
    } else {
        "/dataList/list"
    };
    validate_rank_payload(&payload, list_path)?;
    Ok(payload)
}

async fn request_json(
    context: &PluginContext,
    url: &str,
    headers: &HeaderMap,
) -> Result<Value, String> {
    context
        .client
        .get(url)
        .headers(headers.clone())
        .send()
        .await
        .map_err(|error| error.to_string())?
        .error_for_status()
        .map_err(|error| error.to_string())?
        .json()
        .await
        .map_err(|error| error.to_string())
}

async fn get_heat_json(
    context: &PluginContext,
    url: &str,
    headers: &HeaderMap,
    report: &mut RunReport,
) -> Result<Value, String> {
    let payload = request_json(context, url, headers).await?;
    // Web movies may publish yesterday's daily chart while today's chart is pending.
    if url.contains("/webHeatNetData")
        && (payload.get("status") == Some(&Value::Bool(false))
            || json_array(&payload, &["dataList", "list"]).is_empty())
    {
        if let Some(latest) = latest_web_movie_date(&payload) {
            let mut retry = reqwest::Url::parse(url).map_err(|e| e.to_string())?;
            let pairs: Vec<(String, String)> = retry
                .query_pairs()
                .map(|(k, v)| {
                    let value = if k == "showDate" {
                        latest.clone()
                    } else {
                        v.into_owned()
                    };
                    (k.into_owned(), value)
                })
                .collect();
            let requested = retry
                .query_pairs()
                .find(|(k, _)| k == "showDate")
                .map(|(_, v)| v.into_owned());
            // Never substitute a future chart or loop through dates looking for data.
            if requested
                .as_deref()
                .is_some_and(|date| latest.as_str() < date)
            {
                retry.query_pairs_mut().clear().extend_pairs(pairs);
                report.warnings.push(format!(
                    "网络电影当日榜单尚未发布，尝试最新可用日期 {latest}"
                ));
                return get_json_with_headers(context, retry.as_str(), headers).await;
            }
        }
    }
    validate_rank_payload(&payload, "/dataList/list")?;
    Ok(payload)
}

fn latest_web_movie_date(payload: &Value) -> Option<String> {
    let raw = payload.pointer("/calendar/selectMaxDate")?.as_str()?;
    let date = chrono::NaiveDate::parse_from_str(raw, "%Y-%m-%d")
        .or_else(|_| chrono::NaiveDate::parse_from_str(raw, "%Y%m%d"))
        .ok()?;
    Some(date.format("%Y%m%d").to_string())
}

fn validate_rank_payload(payload: &Value, list_path: &str) -> Result<(), String> {
    if payload.get("status") == Some(&Value::Bool(false)) {
        return Err("猫眼接口返回失败状态，可能需要网页脚本校验".into());
    }
    if payload
        .pointer(list_path)
        .and_then(Value::as_array)
        .is_none()
    {
        return Err("猫眼榜单响应缺少有效列表，接口可能不可用或结构已变化".to_string());
    }
    Ok(())
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
    let target_season = candidate_tv_season(candidate);
    let search_title = if target_season.is_some() {
        strip_season(&candidate.title)
    } else {
        candidate.title.clone()
    };
    let search_year = if target_season.is_some_and(|season| season > DEFAULT_TV_SEASON) {
        None
    } else {
        candidate.year
    };
    let body = json!({
        "title": search_title,
        "year": search_year,
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
    let mut resolved: ResolvedMedia =
        serde_json::from_value(response.get("data").cloned().unwrap_or(response))
            .map_err(|error| error.to_string())?;
    enrich_tv_episode_count(context, candidate, &mut resolved).await?;
    Ok(resolved)
}

async fn enrich_tv_episode_count(
    context: &PluginContext,
    candidate: &Candidate,
    resolved: &mut ResolvedMedia,
) -> Result<(), String> {
    let Some(target_season) = target_tv_season(candidate, resolved) else {
        resolved.expected_episodes = None;
        return Ok(());
    };

    let tmdb_id = resolved
        .tmdb_id
        .trim()
        .parse::<i32>()
        .ok()
        .filter(|value| *value > 0)
        .ok_or_else(|| format!("TMDB ID 无效: {}", resolved.tmdb_id))?;
    let details = plugin_api_get(
        context,
        &format!("/search/tmdb/details?id={tmdb_id}&media_type=tv"),
    )
    .await
    .map_err(|error| format!("获取 TMDB 第 {target_season} 季详情失败: {error}"))?;
    let episode_count = tmdb_season_episode_count(&details, target_season).ok_or_else(|| {
        format!("TMDB 详情未提供第 {target_season} 季的有效总集数，已跳过创建订阅")
    })?;
    resolved.expected_episodes = Some(episode_count);
    Ok(())
}

fn tmdb_season_episode_count(details: &Value, season_number: i32) -> Option<i32> {
    details
        .get("seasons")
        .and_then(Value::as_array)
        .or_else(|| {
            details
                .get("details")
                .and_then(|value| value.get("seasons"))
                .and_then(Value::as_array)
        })?
        .iter()
        .find_map(|season| {
            let number = json_positive_i32(season.get("season_number"))?;
            (number == season_number)
                .then(|| json_positive_i32(season.get("episode_count")))
                .flatten()
        })
}

fn json_positive_i32(value: Option<&Value>) -> Option<i32> {
    value
        .and_then(Value::as_i64)
        .and_then(|value| i32::try_from(value).ok())
        .filter(|value| *value > 0)
}

fn resolved_media_type<'a>(candidate: &'a Candidate, resolved: &'a ResolvedMedia) -> &'a str {
    if resolved.media_type.trim().is_empty() {
        candidate.media_type.as_str()
    } else {
        resolved.media_type.as_str()
    }
}

fn candidate_tv_season(candidate: &Candidate) -> Option<i32> {
    (candidate.media_type == "tv").then(|| extract_season(&candidate.title).unwrap_or(1))
}

fn target_tv_season(candidate: &Candidate, resolved: &ResolvedMedia) -> Option<i32> {
    (resolved_media_type(candidate, resolved) == "tv").then(|| {
        extract_season(&candidate.title)
            .or_else(|| extract_season(&resolved.title))
            .unwrap_or(DEFAULT_TV_SEASON)
    })
}

fn extract_season(title: &str) -> Option<i32> {
    for pattern in [
        r"(?i)(?:第\s*)?(\d{1,2})\s*季",
        r"(?i)\bS\s*(\d{1,2})\b",
        r"(?i)\bSeason\s*(\d{1,2})\b",
    ] {
        if let Some(value) = Regex::new(pattern)
            .unwrap()
            .captures(title)
            .and_then(|captures| captures.get(1))
            .and_then(|value| value.as_str().parse::<i32>().ok())
            .filter(|value| *value > 0)
        {
            return Some(value);
        }
    }
    Regex::new(r"第([一二三四五六七八九十]{1,3})季")
        .unwrap()
        .captures(title)
        .and_then(|captures| captures.get(1))
        .and_then(|value| chinese_number(value.as_str()))
        .or_else(|| extract_trailing_season(title))
}

fn strip_season(title: &str) -> String {
    let mut value = title.to_string();
    for pattern in [
        r"(?i)\s*(?:第\s*)?\d{1,2}\s*季\s*$",
        r"\s*第[一二三四五六七八九十]{1,3}季\s*$",
        r"(?i)\s+S\s*\d{1,2}\s*$",
        r"(?i)\s+Season\s*\d{1,2}\s*$",
    ] {
        value = Regex::new(pattern).unwrap().replace(&value, "").to_string();
    }
    if let Some(captures) = Regex::new(r"(?:^|[^0-9])(\d{1,2})\s*$")
        .unwrap()
        .captures(&value)
        && let Some(number) = captures.get(1)
    {
        value.truncate(number.start());
    }
    let value = value.trim();
    if value.is_empty() {
        title.trim()
    } else {
        value
    }
    .to_string()
}

fn extract_trailing_season(title: &str) -> Option<i32> {
    Regex::new(r"(?:^|[^0-9])(\d{1,2})\s*$")
        .unwrap()
        .captures(title.trim())
        .and_then(|captures| captures.get(1))
        .and_then(|value| value.as_str().parse::<i32>().ok())
        .filter(|value| *value > 0)
}

fn chinese_number(value: &str) -> Option<i32> {
    let digit = |character| match character {
        '一' => Some(1),
        '二' => Some(2),
        '三' => Some(3),
        '四' => Some(4),
        '五' => Some(5),
        '六' => Some(6),
        '七' => Some(7),
        '八' => Some(8),
        '九' => Some(9),
        _ => None,
    };
    if value == "十" {
        return Some(10);
    }
    if let Some((head, tail)) = value.split_once('十') {
        let tens = head.chars().next().and_then(digit).unwrap_or(1);
        let ones = tail.chars().next().and_then(digit).unwrap_or(0);
        return Some(tens * 10 + ones);
    }
    value.chars().next().and_then(digit)
}

async fn create_subscription(
    context: &PluginContext,
    candidate: &Candidate,
    resolved: &ResolvedMedia,
) -> Result<(), String> {
    let media_type = resolved_media_type(candidate, resolved);
    let season = target_tv_season(candidate, resolved);
    let body = json!({
        "tmdb_id": resolved.tmdb_id,
        "name": if resolved.title.trim().is_empty() { &candidate.title } else { &resolved.title },
        "year": resolved.year.or(candidate.year),
        "season": season,
        "season_start_episode": (media_type == "tv").then_some(1),
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
    let media_type = resolved_media_type(candidate, resolved);
    subscription_key(
        media_type,
        resolved.tmdb_id.trim(),
        target_tv_season(candidate, resolved),
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
        source_rank: None,
        source_heat: None,
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
        source_rank: candidate.source_rank,
        source_heat: candidate.source_heat,
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
        "web-heat-combined" => "电视剧＋网剧全网综合榜",
        "web-tv" => "网剧热度榜单",
        "zongyi" => "综艺榜单",
        "web-movie" => "网络电影榜单",
        "tencent-tv-search" => "腾讯电视剧热搜榜",
        "iqiyi-tv-hot" => "爱奇艺电视剧热播榜",
        "mango-tv-popular" => "芒果电视剧热门榜",
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

    #[tokio::test]
    #[ignore = "reads live platform charts without creating subscriptions"]
    async fn live_platform_fallbacks_supply_tv_candidates() {
        let mut settings = default_settings();
        settings.rank_types = vec!["web-heat".into()];
        settings.tx_enabled = true;
        settings.tx_num = 3;
        settings.iqy_enabled = true;
        settings.iqy_num = 3;
        settings.mg_enabled = true;
        settings.mg_num = 3;
        let context = PluginContext {
            api_url: String::new(),
            token: String::new(),
            data_dir: env::temp_dir(),
            client: Client::builder()
                .user_agent(USER_AGENT)
                .timeout(Duration::from_secs(20))
                .build()
                .unwrap(),
            settings,
        };
        let mut report = RunReport {
            ran_at: String::new(),
            fetched: 0,
            considered: 0,
            resolved: 0,
            subscribed: 0,
            skipped_history: 0,
            skipped_existing: 0,
            failures: Vec::new(),
            warnings: Vec::new(),
        };
        let candidates = fetch_rankings(&context, &mut report).await.unwrap();
        eprintln!(
            "平台只读取数验证: {}",
            json!({"candidates": candidates,"warnings":report.warnings,"failures":report.failures})
        );
        assert!(report.failures.is_empty(), "{:?}", report.failures);
        for source in ["tencent-tv-search", "iqiyi-tv-hot", "mango-tv-popular"] {
            assert!(
                candidates.iter().any(|c| c.source == source),
                "缺少 {source}"
            );
        }
        assert!(
            candidates
                .iter()
                .all(|c| c.media_type == "tv" && c.source_rank.is_some_and(|r| r <= 3))
        );
    }

    #[test]
    fn old_subscription_records_keep_loading_after_rank_metadata_is_added() {
        let record: SubscriptionRecord = serde_json::from_value(json!({"title":"旧订阅","tmdb_id":"123","media_type":"剧集/综艺","source":"电视剧热度榜单","platform":"猫眼","year":2026,"subscribed_at":"2026-10-13T09:00:00+08:00"})).unwrap();
        assert!(record.source_rank.is_none());
        assert!(record.source_heat.is_none());
    }

    #[test]
    fn accepts_empty_rankings_but_rejects_error_responses() {
        assert!(validate_rank_payload(&json!({"dataList":{"list":[]}}), "/dataList/list").is_ok());
        for payload in [json!({"status":false}), json!({"dataList":{"list":null}})] {
            assert!(validate_rank_payload(&payload, "/dataList/list").is_err());
        }
    }

    #[test]
    fn reads_page_json_without_tracing_html_or_script_delimiters() {
        let html = r#"<script>var AppData = {"pageData":{"webHeatData":[{"seriesInfo":{"name":"剧名 </script> ; 特别篇","platformDesc":"腾讯视频独播"}}]}}; unrelated();</script>"#;
        let payload = parse_combined_web_heat(html).unwrap();
        assert_eq!(
            payload["dataList"]["list"][0]["seriesInfo"]["name"],
            "剧名 </script> ; 特别篇"
        );
    }

    #[test]
    fn rejects_challenge_pages_and_changed_or_empty_page_data() {
        for html in [
            "<html>403 Forbidden</html>",
            "var AppData = {broken};",
            r#"var AppData = {"pageData":{"webHeatData":[]}};"#,
            r#"var AppData = {"pageData":{"webHeatData":[{}]}};"#,
            r#"var AppData = {"pageData":{"other":[]}};"#,
        ] {
            assert!(parse_combined_web_heat(html).is_err(), "{html}");
        }
    }

    #[tokio::test]
    #[ignore = "requires live Maoyan access"]
    async fn live_rankings_continue_after_blocked_sources() {
        let mut settings = default_settings();
        settings.all_enabled = true;
        settings.all_num = 3;
        settings.movie_num = 3;
        let context = PluginContext {
            api_url: String::new(),
            token: String::new(),
            data_dir: env::temp_dir(),
            client: Client::builder()
                .user_agent(USER_AGENT)
                .timeout(Duration::from_secs(20))
                .build()
                .unwrap(),
            settings,
        };
        let mut report = RunReport {
            ran_at: String::new(),
            fetched: 0,
            considered: 0,
            resolved: 0,
            subscribed: 0,
            skipped_history: 0,
            skipped_existing: 0,
            failures: Vec::new(),
            warnings: Vec::new(),
        };
        let candidates = fetch_rankings(&context, &mut report).await.unwrap();
        eprintln!(
            "猫眼只读取数验证: {}",
            json!({
                "candidates": candidates.iter().map(|item| json!({"title":item.title,"source":item.source})).collect::<Vec<_>>(),
                "warnings": report.warnings,
                "failures": report.failures,
            })
        );
        assert!(candidates.iter().any(|item| item.source == "movie"));
        assert!(
            candidates
                .iter()
                .any(|item| item.source == "web-heat" || item.source == "web-heat-combined")
        );
        assert!(candidates.iter().all(|item| !item.title.is_empty()));
        // The default selection includes web movies and separate TV/variety feeds;
        // their failures must not discard the working movie and combined feeds.
        assert!(candidates.len() >= 6);
    }

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

    #[test]
    fn extracts_target_season_and_strips_it_from_tmdb_search_title() {
        for (title, season, search_title) in [
            ("庆余年2", 2, "庆余年"),
            ("谜探路德维希 第二季", 2, "谜探路德维希"),
            ("Slow Horses Season 6", 6, "Slow Horses"),
            ("流人 S06", 6, "流人"),
        ] {
            assert_eq!(extract_season(title), Some(season));
            assert_eq!(strip_season(title), search_title);
        }
        assert_eq!(extract_season("新剧"), None);
        assert_eq!(extract_season("惩罚者 2017"), None);
    }

    #[test]
    fn subscription_identity_uses_season_from_maoyan_title() {
        let candidate = candidate(
            "庆余年2".to_string(),
            "tv",
            "web-tv",
            String::new(),
            String::new(),
        );
        let resolved = ResolvedMedia {
            tmdb_id: "242143".to_string(),
            title: "庆余年".to_string(),
            media_type: "tv".to_string(),
            year: None,
            poster_path: None,
            backdrop_path: None,
            vote_average: None,
            description: None,
            expected_episodes: Some(36),
        };

        assert_eq!(
            resolved_subscription_key(&candidate, &resolved),
            "tv:242143:2"
        );
    }

    #[test]
    fn reads_episode_count_from_requested_tmdb_season() {
        let details = json!({
            "number_of_episodes": 30,
            "seasons": [
                {"season_number": 0, "episode_count": 2},
                {"season_number": 1, "episode_count": 12},
                {"season_number": 2, "episode_count": 16}
            ]
        });

        assert_eq!(tmdb_season_episode_count(&details, 1), Some(12));
        assert_eq!(tmdb_season_episode_count(&details, 2), Some(16));
    }

    #[test]
    fn rejects_missing_or_zero_tmdb_season_episode_count() {
        let missing = json!({
            "details": {
                "number_of_episodes": 30,
                "seasons": [{"season_number": 2, "episode_count": 16}]
            }
        });
        let zero = json!({
            "seasons": [{"season_number": 1, "episode_count": 0}]
        });

        assert_eq!(tmdb_season_episode_count(&missing, 1), None);
        assert_eq!(tmdb_season_episode_count(&zero, 1), None);
    }
    #[test]
    fn heat_urls_follow_current_site_and_platform_parameters() {
        let all = heat_rank_url(0, "all", "20261013");
        assert_eq!(
            all,
            "https://piaofang.maoyan.com/i/api/encrypt/dashboard/webHeatData?seriesType=0&showDate=20261013"
        );
        for (platform, code) in [
            ("tencent", "3"),
            ("iqiyi", "2"),
            ("mango", "7"),
            ("youku", "1"),
        ] {
            assert!(
                heat_rank_url(1, platform, "20261013").ends_with(&format!("&platformType={code}"))
            );
        }
        assert!(
            web_movie_rank_url("20261013")
                .contains("/webHeatNetData?showDate=20261013&platformType=3&dateType=0&rankType=0")
        );
    }

    #[test]
    fn rank_date_uses_beijing_even_when_host_is_utc() {
        let now = chrono::DateTime::parse_from_rfc3339("2026-10-12T16:01:00Z")
            .unwrap()
            .with_timezone(&Utc);
        assert_eq!(beijing_rank_date(now), "20261013");
    }

    #[test]
    fn page_headers_handle_reordered_and_missing_meta_attributes() {
        let headers = parse_rank_page_headers(r#"<meta content='user-id' name='csrf'><META name="deviceId" content="device-id"><meta name='channelId' content=''><meta name='other' content='ignored'>"#).unwrap();
        assert_eq!(headers["uid"], "user-id");
        assert_eq!(headers["uuid"], "device-id");
        assert_eq!(headers[REFERER], MAOYAN_HEAT_PAGE);
        assert!(!headers.contains_key("channelid"));
        assert!(!headers.contains_key("other"));
        assert!(parse_rank_page_headers("<meta name='csrf' content='bad\nvalue'>").is_err());
    }

    #[test]
    fn pending_web_movie_dates_are_validated() {
        assert_eq!(
            latest_web_movie_date(&json!({"calendar":{"selectMaxDate":"2026-10-12"}})),
            Some("20261012".into())
        );
        assert!(
            latest_web_movie_date(&json!({"calendar":{"selectMaxDate":"2026-02-30"}})).is_none()
        );
        assert!(
            validate_rank_payload(
                &json!({"status":false,"dataList":{"list":[]}}),
                "/dataList/list"
            )
            .is_err()
        );
    }

    #[tokio::test]
    async fn pending_web_movie_chart_retries_announced_date_before_validating_list() {
        use std::io::{Read, Write};
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        listener.set_nonblocking(true).unwrap();
        let server = std::thread::spawn(move || {
            let mut requests = Vec::new();
            let replies = [
                json!({"status":false,"calendar":{"selectMaxDate":"2026-10-12"}}),
                json!({"status":true,"dataList":{"list":[{"movieName":"测试电影"}]}}),
            ];
            let deadline = std::time::Instant::now() + Duration::from_secs(5);
            while requests.len() < replies.len() && std::time::Instant::now() < deadline {
                let Ok((mut stream, _)) = listener.accept() else {
                    std::thread::sleep(Duration::from_millis(20));
                    continue;
                };
                stream
                    .set_read_timeout(Some(Duration::from_secs(2)))
                    .unwrap();
                let mut bytes = Vec::new();
                let mut buffer = [0; 1024];
                while !bytes.windows(4).any(|part| part == b"\r\n\r\n") {
                    let read = stream.read(&mut buffer).unwrap();
                    if read == 0 {
                        break;
                    }
                    bytes.extend_from_slice(&buffer[..read]);
                }
                let body = replies[requests.len()].to_string();
                requests.push(String::from_utf8(bytes).unwrap());
                write!(stream, "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}", body.len(), body).unwrap();
            }
            requests
        });
        let context = PluginContext {
            api_url: String::new(),
            token: String::new(),
            data_dir: env::temp_dir(),
            client: Client::builder()
                .no_proxy()
                .timeout(Duration::from_secs(2))
                .build()
                .unwrap(),
            settings: default_settings(),
        };
        let mut report = RunReport {
            ran_at: String::new(),
            fetched: 0,
            considered: 0,
            resolved: 0,
            subscribed: 0,
            skipped_history: 0,
            skipped_existing: 0,
            failures: Vec::new(),
            warnings: Vec::new(),
        };
        let result = get_heat_json(&context,
            &format!("http://{address}/i/api/encrypt/dashboard/webHeatNetData?showDate=20261013&platformType=3&dateType=0&rankType=0"),
            &HeaderMap::new(), &mut report).await;
        let requests = server.join().unwrap();
        assert_eq!(
            result.unwrap()["dataList"]["list"][0]["movieName"],
            "测试电影"
        );
        assert_eq!(requests.len(), 2);
        assert!(requests[1].contains("showDate=20261012&platformType=3&dateType=0&rankType=0"));
        assert_eq!(report.warnings.len(), 1);
    }
}
