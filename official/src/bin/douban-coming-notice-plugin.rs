// SPDX-License-Identifier: GPL-3.0-only

use chrono::{DateTime, Local, NaiveDate, TimeZone};
use quick_xml::de::from_str;
use regex::Regex;
use reqwest::Client;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{
    collections::{HashSet, VecDeque},
    env, fs,
    path::{Path, PathBuf},
    time::Duration,
};

const USER_AGENT: &str = "Mediary-Douban-Coming-Notice/1.0";
const POSTER_BASE_URL: &str = "https://image.tmdb.org/t/p/w500";
const MAX_RECORDS: usize = 1_000;
const MAX_NOTIFY_KEYS: usize = 2_000;

#[derive(Clone, Deserialize)]
struct Settings {
    #[serde(default = "default_rsshub")]
    rsshub: String,
    #[serde(default = "default_sort_by")]
    sort_by: String,
    #[serde(default = "default_count")]
    count: usize,
    #[serde(default = "default_wish_threshold")]
    wish_count_threshold: u64,
    #[serde(default = "default_advance_days")]
    advance_days: i64,
    #[serde(default = "default_true")]
    notify_before_air: bool,
    #[serde(default = "default_notify_hours")]
    notify_hours: i64,
    #[serde(default)]
    clear: bool,
}

struct PluginContext {
    api_url: String,
    token: String,
    data_dir: PathBuf,
    client: Client,
    settings: Settings,
}

#[derive(Default, Deserialize)]
struct Rss {
    channel: RssChannel,
}

#[derive(Default, Deserialize)]
struct RssChannel {
    #[serde(default, rename = "item")]
    items: Vec<RssItem>,
}

#[derive(Clone, Default, Deserialize)]
struct RssItem {
    #[serde(default)]
    title: String,
    #[serde(default)]
    link: String,
    #[serde(default)]
    description: String,
    #[serde(default)]
    category: String,
}

#[derive(Clone)]
struct Candidate {
    title: String,
    search_title: String,
    description: String,
    douban_id: Option<String>,
    wish_count: u64,
    year: Option<i32>,
    explicit_season: Option<i32>,
}

#[derive(Deserialize)]
struct ResolvedMedia {
    tmdb_id: String,
    #[serde(default)]
    title: String,
    year: Option<i32>,
    poster_path: Option<String>,
    backdrop_path: Option<String>,
    vote_average: Option<f64>,
    description: Option<String>,
    expected_episodes: Option<i32>,
}

#[derive(Default, Deserialize)]
struct DetailsEnvelope {
    #[serde(default)]
    details: TmdbDetails,
    #[serde(default)]
    seasons: Vec<TmdbSeason>,
}

#[derive(Default, Deserialize)]
struct TmdbDetails {
    first_air_date: Option<String>,
    poster_path: Option<String>,
    backdrop_path: Option<String>,
    vote_average: Option<f64>,
    overview: Option<String>,
    number_of_episodes: Option<i32>,
    #[serde(default)]
    genres: Vec<TmdbGenre>,
    #[serde(default)]
    seasons: Vec<TmdbSeason>,
}

#[derive(Clone, Default, Deserialize)]
struct TmdbSeason {
    season_number: i32,
    episode_count: i32,
    air_date: Option<String>,
}

#[derive(Default, Deserialize)]
struct TmdbGenre {
    #[serde(default)]
    name: String,
}

#[derive(Default, Deserialize, Serialize)]
struct PluginState {
    #[serde(default)]
    clear_applied: bool,
    #[serde(default)]
    notify_keys: VecDeque<String>,
}

#[derive(Default, Deserialize, Serialize)]
struct RecordsData {
    #[serde(default)]
    updated_at: String,
    #[serde(default)]
    summary: RecordsSummary,
    #[serde(default)]
    items: VecDeque<Record>,
}

#[derive(Default, Deserialize, Serialize)]
struct RecordsSummary {
    total: usize,
    subscribed: usize,
    notified: usize,
    last_result: String,
}

#[derive(Clone, Deserialize, Serialize)]
struct Record {
    key: String,
    title: String,
    season_label: String,
    tmdb_id: String,
    douban_id: Option<String>,
    wish_count: u64,
    air_date: Option<String>,
    status: String,
    subscribed: bool,
    notified: bool,
    updated_at: String,
}

#[derive(Serialize)]
struct RunReport {
    ran_at: String,
    fetched: usize,
    qualified: usize,
    resolved: usize,
    subscribed: usize,
    notified: usize,
    skipped_low_wish: usize,
    skipped_existing: usize,
    skipped_outside_window: usize,
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
        return Err(format!("不支持的豆瓣将映动作: {action}"));
    }
    let context = PluginContext::from_env()?;
    let state_path = context.data_dir.join("state.json");
    let records_path = context.data_dir.join("records.json");
    let mut state = load_json::<PluginState>(&state_path);
    let mut records = load_json::<RecordsData>(&records_path);

    if context.settings.clear && !state.clear_applied {
        state = PluginState {
            clear_applied: true,
            ..PluginState::default()
        };
        records = RecordsData::default();
    } else if !context.settings.clear {
        state.clear_applied = false;
    }

    let report = refresh(&context, &mut state, &mut records).await?;
    records.summary.last_result = format!(
        "获取 {}，新增订阅 {}，发送提醒 {}，失败 {}",
        report.fetched,
        report.subscribed,
        report.notified,
        report.failures.len()
    );
    update_summary(&mut records);
    trim_queue(&mut state.notify_keys, MAX_NOTIFY_KEYS);
    write_json(&state_path, &state)?;
    write_json(&records_path, &records)?;
    write_json(&context.data_dir.join("last-run.json"), &report)?;

    println!(
        "{}",
        json!({
            "notice": format!("豆瓣将映刷新完成：{}", records.summary.last_result),
            "report": report
        })
    );
    Ok(())
}

impl PluginContext {
    fn from_env() -> Result<Self, String> {
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
            .timeout(Duration::from_secs(30))
            .build()
            .map_err(|error| error.to_string())?;
        Ok(Self {
            api_url: required_env("MEDIARY_PLUGIN_API_URL")?
                .trim_end_matches('/')
                .to_string(),
            token: required_env("MEDIARY_PLUGIN_TOKEN")?,
            data_dir,
            client,
            settings,
        })
    }
}

async fn refresh(
    context: &PluginContext,
    state: &mut PluginState,
    records: &mut RecordsData,
) -> Result<RunReport, String> {
    let mut report = RunReport {
        ran_at: Local::now().to_rfc3339(),
        fetched: 0,
        qualified: 0,
        resolved: 0,
        subscribed: 0,
        notified: 0,
        skipped_low_wish: 0,
        skipped_existing: 0,
        skipped_outside_window: 0,
        failures: Vec::new(),
    };
    let candidates = fetch_candidates(context).await?;
    report.fetched = candidates.len();
    let mut existing = fetch_existing_subscription_keys(context).await?;
    let today = Local::now().date_naive();

    for candidate in candidates {
        if candidate.wish_count < context.settings.wish_count_threshold {
            report.skipped_low_wish += 1;
            continue;
        }
        report.qualified += 1;
        let resolved = match resolve_tmdb(context, &candidate).await {
            Ok(value) => value,
            Err(error) => {
                report
                    .failures
                    .push(format!("{}: {error}", candidate.title));
                continue;
            }
        };
        let tmdb_id = match resolved.tmdb_id.parse::<i32>() {
            Ok(value) if value > 0 => value,
            _ => {
                report
                    .failures
                    .push(format!("{}: TMDB ID 无效", candidate.title));
                continue;
            }
        };
        report.resolved += 1;
        let details = match fetch_tmdb_details(context, tmdb_id).await {
            Ok(value) => value,
            Err(error) => {
                report
                    .failures
                    .push(format!("{}: {error}", candidate.title));
                continue;
            }
        };
        let season = select_season(candidate.explicit_season, &details, today);
        let air_date = season_air_date(season, &details);
        let subscription_key = format!("tv:{tmdb_id}:{season}");
        let record_key = format!(
            "{}:{season}",
            candidate.douban_id.as_deref().unwrap_or(&resolved.tmdb_id)
        );
        let mut subscribed = existing.contains(&subscription_key);
        let mut status = if subscribed {
            report.skipped_existing += 1;
            "已有订阅".to_string()
        } else {
            subscription_window_status(air_date.as_deref(), today, context.settings.advance_days)
        };

        if !subscribed && status == "可订阅" {
            match create_subscription(context, &candidate, &resolved, &details, season).await {
                Ok(()) => {
                    subscribed = true;
                    existing.insert(subscription_key);
                    status = "已新增订阅".to_string();
                    report.subscribed += 1;
                }
                Err(error) => {
                    report
                        .failures
                        .push(format!("{}: {error}", candidate.title));
                    status = "订阅失败".to_string();
                }
            }
        } else if !subscribed {
            report.skipped_outside_window += 1;
        }

        let notify_key = format!(
            "air:{record_key}:{}",
            air_date.as_deref().unwrap_or("unknown")
        );
        let mut notified = state.notify_keys.iter().any(|key| key == &notify_key);
        if context.settings.notify_before_air
            && !notified
            && within_notify_window(
                air_date.as_deref(),
                Local::now(),
                context.settings.notify_hours,
            )
        {
            match send_notification(
                context,
                &candidate,
                &resolved,
                &details,
                season,
                air_date.as_deref(),
                subscribed,
            )
            .await
            {
                Ok(()) => {
                    state.notify_keys.push_front(notify_key);
                    notified = true;
                    report.notified += 1;
                }
                Err(error) => report
                    .failures
                    .push(format!("{} 提醒失败: {error}", candidate.title)),
            }
        }

        upsert_record(
            records,
            Record {
                key: record_key,
                title: display_title(&candidate, &resolved),
                season_label: format!("第 {season} 季"),
                tmdb_id: resolved.tmdb_id.clone(),
                douban_id: candidate.douban_id.clone(),
                wish_count: candidate.wish_count,
                air_date,
                status,
                subscribed,
                notified,
                updated_at: Local::now().to_rfc3339(),
            },
        );
    }
    Ok(report)
}

async fn fetch_candidates(context: &PluginContext) -> Result<Vec<Candidate>, String> {
    let base = context.settings.rsshub.trim().trim_end_matches('/');
    if !base.starts_with("https://") && !base.starts_with("http://") {
        return Err("RSSHub 地址必须使用 http:// 或 https://".to_string());
    }
    let sort = match context
        .settings
        .sort_by
        .trim()
        .to_ascii_lowercase()
        .as_str()
    {
        "time" => "time",
        _ => "hot",
    };
    let url = format!(
        "{base}/douban/tv/coming/{sort}/{}",
        context.settings.count.clamp(1, 100)
    );
    let response = context
        .client
        .get(&url)
        .send()
        .await
        .map_err(|error| format!("RSSHub 请求失败: {error}"))?
        .error_for_status()
        .map_err(|error| format!("RSSHub 返回错误: {error}"))?;
    let body = response
        .text()
        .await
        .map_err(|error| format!("读取 RSSHub 响应失败: {error}"))?;
    parse_rss(&body)
}

fn parse_rss(raw: &str) -> Result<Vec<Candidate>, String> {
    let rss: Rss = from_str(raw).map_err(|error| format!("RSS XML 解析失败: {error}"))?;
    let mut seen = HashSet::new();
    let mut candidates = Vec::new();
    for item in rss.channel.items {
        let title = item.title.trim().to_string();
        if title.is_empty() {
            continue;
        }
        let douban_id = extract_douban_id(&item.link);
        let dedupe_key = douban_id.clone().unwrap_or_else(|| title.clone());
        if !seen.insert(dedupe_key) {
            continue;
        }
        let explicit_season = extract_season(&title);
        candidates.push(Candidate {
            search_title: strip_season(&title),
            wish_count: extract_wish_count(&item.description),
            year: extract_year(&item.category).or_else(|| extract_year(&item.description)),
            description: item.description.trim().to_string(),
            douban_id,
            title,
            explicit_season,
        });
    }
    Ok(candidates)
}

async fn resolve_tmdb(
    context: &PluginContext,
    candidate: &Candidate,
) -> Result<ResolvedMedia, String> {
    let search_year = (candidate.explicit_season.unwrap_or(1) == 1)
        .then_some(candidate.year)
        .flatten();
    let body = json!({
        "title": candidate.search_title,
        "year": search_year,
        "media_type": "tv"
    });
    let payload = plugin_api_post(context, "/tmdb/resolve", body).await?;
    if payload.get("status").and_then(Value::as_str) == Some("failed") {
        return Err(payload
            .get("message")
            .and_then(Value::as_str)
            .unwrap_or("TMDB 未匹配")
            .to_string());
    }
    serde_json::from_value(payload.get("data").cloned().unwrap_or(payload))
        .map_err(|error| format!("TMDB 匹配响应格式无效: {error}"))
}

async fn fetch_tmdb_details(
    context: &PluginContext,
    tmdb_id: i32,
) -> Result<DetailsEnvelope, String> {
    let payload = plugin_api_get(
        context,
        &format!("/search/tmdb/details?id={tmdb_id}&media_type=tv"),
    )
    .await?;
    serde_json::from_value(payload).map_err(|error| format!("TMDB 详情响应格式无效: {error}"))
}

async fn fetch_existing_subscription_keys(
    context: &PluginContext,
) -> Result<HashSet<String>, String> {
    let payload = plugin_api_get(context, "/subscriptions").await?;
    let items = payload
        .as_array()
        .ok_or_else(|| "Mediary 订阅列表响应格式无效".to_string())?;
    Ok(items
        .iter()
        .filter_map(|item| {
            if item.get("media_type")?.as_str()? != "tv" {
                return None;
            }
            let tmdb_id = value_as_i32(item.get("tmdb_id")?)?;
            let season = item.get("season").and_then(value_as_i32).unwrap_or(1);
            Some(format!("tv:{tmdb_id}:{season}"))
        })
        .collect())
}

async fn create_subscription(
    context: &PluginContext,
    candidate: &Candidate,
    resolved: &ResolvedMedia,
    details: &DetailsEnvelope,
    season: i32,
) -> Result<(), String> {
    let season_info = all_seasons(details)
        .into_iter()
        .find(|value| value.season_number == season);
    let expected_episodes = season_info
        .filter(|value| value.episode_count > 0)
        .map(|value| value.episode_count)
        .or(resolved.expected_episodes)
        .or(details.details.number_of_episodes);
    plugin_api_post(
        context,
        "/subscriptions",
        json!({
            "tmdb_id": resolved.tmdb_id,
            "name": display_title(candidate, resolved),
            "year": resolved.year.or(candidate.year),
            "season": season,
            "media_type": "tv",
            "poster_path": resolved.poster_path.as_ref().or(details.details.poster_path.as_ref()),
            "backdrop_path": resolved.backdrop_path.as_ref().or(details.details.backdrop_path.as_ref()),
            "vote_average": resolved.vote_average.or(details.details.vote_average),
            "description": resolved.description.as_ref().or(details.details.overview.as_ref()).unwrap_or(&candidate.description),
            "expected_episodes": expected_episodes
        }),
    )
    .await
    .map(|_| ())
}

async fn send_notification(
    context: &PluginContext,
    candidate: &Candidate,
    resolved: &ResolvedMedia,
    details: &DetailsEnvelope,
    season: i32,
    air_date: Option<&str>,
    subscribed: bool,
) -> Result<(), String> {
    let genres = details
        .details
        .genres
        .iter()
        .map(|genre| genre.name.trim())
        .filter(|name| !name.is_empty())
        .collect::<Vec<_>>()
        .join(" / ");
    let douban_link = candidate
        .douban_id
        .as_deref()
        .map(|id| format!("https://movie.douban.com/subject/{id}/"))
        .unwrap_or_else(|| "-".to_string());
    let content = [
        format!("名称：{}", display_title(candidate, resolved)),
        format!("季度：第 {season} 季"),
        format!("开播日期：{}", air_date.unwrap_or("-")),
        format!("想看人数：{}", candidate.wish_count),
        format!("订阅状态：{}", if subscribed { "已订阅" } else { "未订阅" }),
        format!("类型：{}", if genres.is_empty() { "剧集" } else { &genres }),
        format!("TMDB ID：{}", resolved.tmdb_id),
        format!("豆瓣链接：{douban_link}"),
    ]
    .join("\n");
    let poster = resolved
        .poster_path
        .as_deref()
        .or(details.details.poster_path.as_deref())
        .map(image_url);
    plugin_api_post(
        context,
        "/plugin/notifications",
        json!({
            "title": "豆瓣将映 · 开播提醒",
            "content": content,
            "image_url": poster
        }),
    )
    .await
    .map(|_| ())
}

async fn plugin_api_get(context: &PluginContext, path: &str) -> Result<Value, String> {
    let response = context
        .client
        .get(format!("{}{}", context.api_url, path))
        .bearer_auth(&context.token)
        .send()
        .await
        .map_err(|error| error.to_string())?;
    parse_api_response(response).await
}

async fn plugin_api_post(
    context: &PluginContext,
    path: &str,
    body: Value,
) -> Result<Value, String> {
    let response = context
        .client
        .post(format!("{}{}", context.api_url, path))
        .bearer_auth(&context.token)
        .json(&body)
        .send()
        .await
        .map_err(|error| error.to_string())?;
    parse_api_response(response).await
}

async fn parse_api_response(response: reqwest::Response) -> Result<Value, String> {
    let status = response.status();
    let payload = response.text().await.unwrap_or_default();
    if !status.is_success() {
        return Err(format!("Mediary API {status}: {}", truncate(&payload, 200)));
    }
    serde_json::from_str(&payload).map_err(|error| error.to_string())
}

fn select_season(explicit: Option<i32>, details: &DetailsEnvelope, today: NaiveDate) -> i32 {
    if let Some(season) = explicit.filter(|value| *value > 0) {
        return season;
    }
    all_seasons(details)
        .into_iter()
        .filter(|season| season.season_number > 0)
        .filter_map(|season| {
            parse_date(season.air_date.as_deref()?)
                .filter(|date| *date >= today)
                .map(|date| (date, season.season_number))
        })
        .min_by_key(|(date, _)| *date)
        .map(|(_, season)| season)
        .unwrap_or(1)
}

fn season_air_date(season: i32, details: &DetailsEnvelope) -> Option<String> {
    all_seasons(details)
        .into_iter()
        .find(|value| value.season_number == season)
        .and_then(|value| value.air_date.clone())
        .filter(|value| parse_date(value).is_some())
        .or_else(|| {
            (season == 1)
                .then(|| details.details.first_air_date.clone())
                .flatten()
                .filter(|value| parse_date(value).is_some())
        })
}

fn all_seasons(details: &DetailsEnvelope) -> Vec<&TmdbSeason> {
    if details.seasons.is_empty() {
        details.details.seasons.iter().collect()
    } else {
        details.seasons.iter().collect()
    }
}

fn subscription_window_status(
    air_date: Option<&str>,
    today: NaiveDate,
    advance_days: i64,
) -> String {
    let Some(date) = air_date.and_then(parse_date) else {
        return "未获取开播日期".to_string();
    };
    let days = (date - today).num_days();
    if days < 0 {
        "已开播".to_string()
    } else if days <= advance_days.max(0) {
        "可订阅".to_string()
    } else {
        format!("距开播 {days} 天")
    }
}

fn within_notify_window(air_date: Option<&str>, now: DateTime<Local>, notify_hours: i64) -> bool {
    let Some(date) = air_date.and_then(parse_date) else {
        return false;
    };
    let Some(air_at) = Local
        .from_local_datetime(&date.and_hms_opt(0, 0, 0).unwrap())
        .earliest()
    else {
        return false;
    };
    let hours = (air_at - now).num_minutes() as f64 / 60.0;
    hours >= 0.0 && hours <= notify_hours.max(1) as f64
}

fn extract_douban_id(link: &str) -> Option<String> {
    Regex::new(r"/subject/(\d+)(?:/|$)")
        .unwrap()
        .captures(link)
        .and_then(|captures| captures.get(1))
        .map(|value| value.as_str().to_string())
}

fn extract_wish_count(text: &str) -> u64 {
    let cleaned = text.replace([',', '，'], "");
    for pattern in [r"(?:想看人数|想看)\s*[:：]?\s*(\d+)", r"(\d+)\s*人想看"] {
        if let Some(value) = Regex::new(pattern)
            .unwrap()
            .captures(&cleaned)
            .and_then(|captures| captures.get(1))
            .and_then(|value| value.as_str().parse().ok())
        {
            return value;
        }
    }
    0
}

fn extract_year(text: &str) -> Option<i32> {
    Regex::new(r"(?:^|\D)((?:19|20)\d{2})(?:\D|$)")
        .unwrap()
        .captures(text)
        .and_then(|captures| captures.get(1))
        .and_then(|value| value.as_str().parse().ok())
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
    let value = value.trim();
    if value.is_empty() {
        title.trim().to_string()
    } else {
        value.to_string()
    }
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

fn display_title(candidate: &Candidate, resolved: &ResolvedMedia) -> String {
    if resolved.title.trim().is_empty() {
        candidate.search_title.clone()
    } else {
        resolved.title.clone()
    }
}

fn image_url(path: &str) -> String {
    if path.starts_with("http://") || path.starts_with("https://") {
        path.to_string()
    } else {
        format!("{POSTER_BASE_URL}/{}", path.trim_start_matches('/'))
    }
}

fn parse_date(value: &str) -> Option<NaiveDate> {
    NaiveDate::parse_from_str(value.trim(), "%Y-%m-%d").ok()
}

fn value_as_i32(value: &Value) -> Option<i32> {
    value
        .as_i64()
        .and_then(|value| i32::try_from(value).ok())
        .or_else(|| value.as_str()?.parse().ok())
}

fn upsert_record(records: &mut RecordsData, record: Record) {
    if let Some(index) = records.items.iter().position(|item| item.key == record.key) {
        records.items.remove(index);
    }
    records.items.push_front(record);
    trim_queue(&mut records.items, MAX_RECORDS);
}

fn update_summary(records: &mut RecordsData) {
    records.summary.total = records.items.len();
    records.summary.subscribed = records.items.iter().filter(|item| item.subscribed).count();
    records.summary.notified = records.items.iter().filter(|item| item.notified).count();
    records.updated_at = Local::now().to_rfc3339();
}

fn trim_queue<T>(queue: &mut VecDeque<T>, max: usize) {
    while queue.len() > max {
        queue.pop_back();
    }
}

fn load_json<T: for<'de> Deserialize<'de> + Default>(path: &Path) -> T {
    fs::read_to_string(path)
        .ok()
        .and_then(|raw| serde_json::from_str(&raw).ok())
        .unwrap_or_default()
}

fn write_json<T: Serialize>(path: &Path, value: &T) -> Result<(), String> {
    let encoded = serde_json::to_string_pretty(value).map_err(|error| error.to_string())?;
    let temporary = path.with_extension("json.tmp");
    fs::write(&temporary, format!("{encoded}\n")).map_err(|error| error.to_string())?;
    fs::rename(temporary, path).map_err(|error| error.to_string())
}

fn required_env(name: &str) -> Result<String, String> {
    env::var(name).map_err(|_| format!("缺少运行时环境变量 {name}"))
}

fn default_settings() -> Settings {
    Settings {
        rsshub: default_rsshub(),
        sort_by: default_sort_by(),
        count: default_count(),
        wish_count_threshold: default_wish_threshold(),
        advance_days: default_advance_days(),
        notify_before_air: true,
        notify_hours: default_notify_hours(),
        clear: false,
    }
}

fn default_rsshub() -> String {
    "https://rsshub.ddsrem.com".to_string()
}

fn default_sort_by() -> String {
    "hot".to_string()
}

fn default_count() -> usize {
    10
}

fn default_wish_threshold() -> u64 {
    5_000
}

fn default_advance_days() -> i64 {
    7
}

fn default_notify_hours() -> i64 {
    24
}

fn default_true() -> bool {
    true
}

fn truncate(value: &str, max_chars: usize) -> String {
    value.chars().take(max_chars).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::{
        Json, Router,
        extract::State,
        http::StatusCode,
        response::IntoResponse,
        routing::{get, post},
    };
    use std::sync::Arc;
    use tokio::sync::Mutex;

    const RSS_FIXTURE: &str = r#"<?xml version="1.0" encoding="UTF-8"?>
<rss version="2.0"><channel><title>豆瓣剧集-即将播出</title>
<item><title>谜探路德维希 第二季</title><description>想看人数：15,908，简介</description><link>https://movie.douban.com/subject/37100781/</link><category>2026 / 英国 / 悬疑</category></item>
<item><title>新剧</title><description>998 人想看</description><link>https://movie.douban.com/subject/123/</link><category>2027 / 中国大陆</category></item>
</channel></rss>"#;

    #[derive(Clone, Default)]
    struct MockApiState {
        subscriptions: Arc<Mutex<Vec<Value>>>,
        notifications: Arc<Mutex<Vec<Value>>>,
    }

    async fn mock_rss() -> impl IntoResponse {
        let tomorrow = (Local::now().date_naive() + chrono::Days::new(1))
            .format("%Y-%m-%d")
            .to_string();
        (
            StatusCode::OK,
            format!(
                r#"<?xml version="1.0"?><rss version="2.0"><channel>
<item><title>测试剧 第二季</title><description>想看人数：6000，测试简介</description><link>https://movie.douban.com/subject/456/</link><category>{tomorrow} / 中国大陆 / 剧情</category></item>
</channel></rss>"#
            ),
        )
    }

    async fn mock_resolve() -> Json<Value> {
        Json(json!({
            "status": "success",
            "data": {
                "tmdb_id": "123",
                "title": "测试剧",
                "type": "tv",
                "year": 2025,
                "poster_path": "/poster.jpg",
                "description": "TMDB 简介"
            }
        }))
    }

    async fn mock_details() -> Json<Value> {
        let tomorrow = (Local::now().date_naive() + chrono::Days::new(1))
            .format("%Y-%m-%d")
            .to_string();
        Json(json!({
            "details": {
                "name": "测试剧",
                "poster_path": "/poster.jpg",
                "genres": [{"name": "剧情"}],
                "seasons": [{"season_number": 2, "episode_count": 8, "air_date": tomorrow}]
            },
            "seasons": [{"season_number": 2, "episode_count": 8, "air_date": tomorrow}]
        }))
    }

    async fn mock_list() -> Json<Value> {
        Json(json!([]))
    }

    async fn mock_create(
        State(state): State<MockApiState>,
        Json(payload): Json<Value>,
    ) -> (StatusCode, Json<Value>) {
        state.subscriptions.lock().await.push(payload);
        (StatusCode::CREATED, Json(json!({"id": 1})))
    }

    async fn mock_notify(
        State(state): State<MockApiState>,
        Json(payload): Json<Value>,
    ) -> Json<Value> {
        state.notifications.lock().await.push(payload);
        Json(json!({"success": true}))
    }

    async fn mock_context() -> (PluginContext, MockApiState, tokio::task::JoinHandle<()>) {
        let state = MockApiState::default();
        let app = Router::new()
            .route("/rss/douban/tv/coming/hot/10", get(mock_rss))
            .route("/api/tmdb/resolve", post(mock_resolve))
            .route("/api/search/tmdb/details", get(mock_details))
            .route("/api/subscriptions", get(mock_list).post(mock_create))
            .route("/api/plugin/notifications", post(mock_notify))
            .with_state(state.clone());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let unique = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let data_dir = env::temp_dir().join(format!(
            "mediary-douban-coming-test-{}-{unique}",
            std::process::id()
        ));
        fs::create_dir_all(&data_dir).unwrap();
        (
            PluginContext {
                api_url: format!("http://{address}/api"),
                token: "plugin-token".to_string(),
                data_dir,
                client: Client::new(),
                settings: Settings {
                    rsshub: format!("http://{address}/rss"),
                    notify_hours: 48,
                    ..default_settings()
                },
            },
            state,
            server,
        )
    }

    #[test]
    fn parses_real_rss_shape_and_metadata() {
        let items = parse_rss(RSS_FIXTURE).unwrap();
        assert_eq!(items.len(), 2);
        assert_eq!(items[0].search_title, "谜探路德维希");
        assert_eq!(items[0].explicit_season, Some(2));
        assert_eq!(items[0].wish_count, 15_908);
        assert_eq!(items[0].douban_id.as_deref(), Some("37100781"));
        assert_eq!(items[0].year, Some(2026));
    }

    #[test]
    fn recognizes_supported_season_notation() {
        assert_eq!(extract_season("流人 第六季"), Some(6));
        assert_eq!(extract_season("流人 S06"), Some(6));
        assert_eq!(extract_season("Slow Horses Season 6"), Some(6));
        assert_eq!(extract_season("新剧"), None);
    }

    #[test]
    fn selects_nearest_upcoming_season_without_title_hint() {
        let details = DetailsEnvelope {
            seasons: vec![
                TmdbSeason {
                    season_number: 1,
                    air_date: Some("2025-01-01".to_string()),
                    ..TmdbSeason::default()
                },
                TmdbSeason {
                    season_number: 2,
                    air_date: Some("2026-09-01".to_string()),
                    ..TmdbSeason::default()
                },
            ],
            ..DetailsEnvelope::default()
        };
        assert_eq!(
            select_season(
                None,
                &details,
                NaiveDate::from_ymd_opt(2026, 8, 12).unwrap()
            ),
            2
        );
    }

    #[test]
    fn enforces_subscription_and_notification_windows() {
        let today = NaiveDate::from_ymd_opt(2026, 8, 12).unwrap();
        assert_eq!(
            subscription_window_status(Some("2026-08-19"), today, 7),
            "可订阅"
        );
        assert_eq!(
            subscription_window_status(Some("2026-08-20"), today, 7),
            "距开播 8 天"
        );
        let now = Local.with_ymd_and_hms(2026, 8, 12, 12, 0, 0).unwrap();
        assert!(within_notify_window(Some("2026-08-13"), now, 24));
        assert!(!within_notify_window(Some("2026-08-14"), now, 24));
    }

    #[test]
    fn manifest_has_expected_identity_and_scopes() {
        let manifest: Value =
            serde_json::from_str(include_str!("../../douban-coming-notice/plugin.json")).unwrap();
        assert_eq!(manifest["id"], "douban-coming-notice");
        assert_eq!(manifest["name"], "豆瓣将映");
        assert_eq!(
            manifest["requested_scopes"],
            json!([
                "catalog:read",
                "subscriptions:read",
                "subscriptions:write",
                "notifications:send"
            ])
        );
    }

    #[tokio::test]
    async fn refresh_creates_subscription_notifies_and_persists_record() {
        let (context, api, server) = mock_context().await;
        let mut state = PluginState::default();
        let mut records = RecordsData::default();
        let report = refresh(&context, &mut state, &mut records).await.unwrap();

        assert_eq!(report.fetched, 1);
        assert_eq!(report.subscribed, 1);
        assert_eq!(report.notified, 1);
        let subscriptions = api.subscriptions.lock().await;
        assert_eq!(subscriptions.len(), 1);
        assert_eq!(subscriptions[0]["tmdb_id"], "123");
        assert_eq!(subscriptions[0]["season"], 2);
        assert_eq!(subscriptions[0]["expected_episodes"], 8);
        drop(subscriptions);
        assert_eq!(api.notifications.lock().await.len(), 1);
        assert_eq!(records.items.len(), 1);
        assert!(records.items[0].subscribed);
        assert!(records.items[0].notified);

        server.abort();
        fs::remove_dir_all(&context.data_dir).unwrap();
    }
}
