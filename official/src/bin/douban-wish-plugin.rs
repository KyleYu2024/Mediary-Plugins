// SPDX-License-Identifier: GPL-3.0-only

use chrono::{DateTime, Local, Utc};
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

const DOUBAN_FEED_BASE: &str = "https://www.douban.com/feed/people";
const DOUBAN_API_BASE: &str = "https://m.douban.com/rexxar/api/v2/movie";
const USER_AGENT: &str =
    "Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) AppleWebKit/537.36 Chrome/121 Safari/537.36";
const MAX_RECORDS: usize = 2_000;
const MAX_FEED_BYTES: usize = 5 * 1024 * 1024;

#[derive(Clone, Deserialize)]
struct Settings {
    #[serde(default)]
    users: String,
    #[serde(default = "default_days")]
    days: i64,
    #[serde(default)]
    include: String,
    #[serde(default)]
    exclude: String,
    #[serde(default)]
    clear: bool,
}

struct PluginContext {
    api_url: String,
    douban_feed_base: String,
    douban_api_base: String,
    token: String,
    data_dir: PathBuf,
    client: Client,
    settings: Settings,
    include: Option<Regex>,
    exclude: Option<Regex>,
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
    #[serde(default, rename = "pubDate")]
    pub_date: String,
    #[serde(default, rename = "creator")]
    creator: String,
}

#[derive(Clone)]
struct WishItem {
    douban_user: String,
    nickname: String,
    douban_id: String,
    title: String,
    description: String,
    pub_date: Option<DateTime<Utc>>,
}

#[derive(Deserialize)]
struct DoubanSubject {
    id: String,
    title: String,
    #[serde(default)]
    r#type: String,
    #[serde(default)]
    subtype: String,
    year: Value,
    #[serde(default)]
    intro: String,
    #[serde(default)]
    cover_url: String,
    #[serde(default)]
    episodes_count: i32,
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

#[derive(Default, Deserialize)]
struct DetailsEnvelope {
    #[serde(default)]
    details: TmdbDetails,
    #[serde(default)]
    seasons: Vec<TmdbSeason>,
}

#[derive(Default, Deserialize)]
struct TmdbDetails {
    #[serde(default)]
    seasons: Vec<TmdbSeason>,
}

#[derive(Clone, Default, Deserialize)]
struct TmdbSeason {
    season_number: i32,
    episode_count: i32,
}

#[derive(Default, Deserialize, Serialize)]
struct PluginState {
    #[serde(default)]
    clear_applied: bool,
    #[serde(default)]
    processed: VecDeque<String>,
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
    existing: usize,
    last_result: String,
}

#[derive(Clone, Deserialize, Serialize)]
struct Record {
    douban_id: String,
    douban_user: String,
    title: String,
    tmdb_id: String,
    media_type: String,
    season_label: String,
    status: String,
    updated_at: String,
}

#[derive(Serialize)]
struct RunReport {
    ran_at: String,
    users: usize,
    fetched: usize,
    wishes: usize,
    resolved: usize,
    subscribed: usize,
    skipped_old: usize,
    skipped_history: usize,
    skipped_filter: usize,
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
        return Err(format!("不支持的豆瓣想看动作: {action}"));
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
        "获取 {}，想看 {}，新增订阅 {}，失败 {}",
        report.fetched,
        report.wishes,
        report.subscribed,
        report.failures.len()
    );
    update_summary(&mut records);
    trim_queue(&mut state.processed, MAX_RECORDS);
    write_json(&state_path, &state)?;
    write_json(&records_path, &records)?;
    write_json(&context.data_dir.join("last-run.json"), &report)?;
    println!(
        "{}",
        json!({
            "notice": format!("豆瓣想看同步完成：{}", records.summary.last_result),
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
        let include = compile_optional_regex(&settings.include, "包含规则")?;
        let exclude = compile_optional_regex(&settings.exclude, "排除规则")?;
        let client = Client::builder()
            .user_agent(USER_AGENT)
            .timeout(Duration::from_secs(30))
            .build()
            .map_err(|error| error.to_string())?;
        Ok(Self {
            api_url: required_env("MEDIARY_PLUGIN_API_URL")?
                .trim_end_matches('/')
                .to_string(),
            douban_feed_base: DOUBAN_FEED_BASE.to_string(),
            douban_api_base: DOUBAN_API_BASE.to_string(),
            token: required_env("MEDIARY_PLUGIN_TOKEN")?,
            data_dir,
            client,
            settings,
            include,
            exclude,
        })
    }
}

async fn refresh(
    context: &PluginContext,
    state: &mut PluginState,
    records: &mut RecordsData,
) -> Result<RunReport, String> {
    let users = parse_users(&context.settings.users)?;
    if users.is_empty() {
        return Err("请先填写豆瓣用户 ID".to_string());
    }
    let mut report = RunReport {
        ran_at: Local::now().to_rfc3339(),
        users: users.len(),
        fetched: 0,
        wishes: 0,
        resolved: 0,
        subscribed: 0,
        skipped_old: 0,
        skipped_history: 0,
        skipped_filter: 0,
        skipped_existing: 0,
        failures: Vec::new(),
    };
    let mut existing = fetch_existing_subscription_keys(context).await?;
    let mut processed = state.processed.iter().cloned().collect::<HashSet<_>>();

    for user in users {
        let items = match fetch_wishes(context, &user).await {
            Ok((fetched, items)) => {
                report.fetched += fetched;
                items
            }
            Err(error) => {
                report.failures.push(format!("用户 {user}: {error}"));
                continue;
            }
        };
        report.wishes += items.len();
        for item in items {
            if is_older_than(&item, context.settings.days) {
                report.skipped_old += 1;
                continue;
            }
            if processed.contains(&item.douban_id) {
                report.skipped_history += 1;
                continue;
            }
            let searchable = format!("{} {}", item.title, strip_html(&item.description));
            if context
                .include
                .as_ref()
                .is_some_and(|rule| !rule.is_match(&searchable))
                || context
                    .exclude
                    .as_ref()
                    .is_some_and(|rule| rule.is_match(&searchable))
            {
                report.skipped_filter += 1;
                continue;
            }
            match process_wish(context, &item, &mut existing).await {
                Ok((record, created)) => {
                    report.resolved += 1;
                    if created {
                        report.subscribed += 1;
                    } else {
                        report.skipped_existing += 1;
                    }
                    processed.insert(item.douban_id.clone());
                    state.processed.push_front(item.douban_id.clone());
                    upsert_record(records, record);
                }
                Err(error) => report
                    .failures
                    .push(format!("{} ({}): {error}", item.title, item.douban_id)),
            }
        }
    }
    Ok(report)
}

async fn fetch_wishes(
    context: &PluginContext,
    user: &str,
) -> Result<(usize, Vec<WishItem>), String> {
    let url = format!("{}/{user}/interests", context.douban_feed_base);
    let response = context
        .client
        .get(&url)
        .send()
        .await
        .map_err(|error| format!("请求 RSS 失败: {error}"))?;
    let status = response.status();
    let bytes = response
        .bytes()
        .await
        .map_err(|error| format!("读取 RSS 失败: {error}"))?;
    if !status.is_success() {
        return Err(format!("豆瓣 RSS HTTP {status}"));
    }
    if bytes.len() > MAX_FEED_BYTES {
        return Err("豆瓣 RSS 超过 5 MB 限制".to_string());
    }
    let raw = std::str::from_utf8(&bytes).map_err(|_| "豆瓣 RSS 不是 UTF-8".to_string())?;
    let rss = from_str::<Rss>(raw).map_err(|error| format!("解析豆瓣 RSS 失败: {error}"))?;
    let fetched = rss.channel.items.len();
    let items = rss
        .channel
        .items
        .into_iter()
        .filter_map(|item| wish_from_rss(user, item))
        .collect();
    Ok((fetched, items))
}

fn wish_from_rss(user: &str, item: RssItem) -> Option<WishItem> {
    let title = item.title.trim().strip_prefix("想看")?.trim();
    if title.is_empty() {
        return None;
    }
    Some(WishItem {
        douban_user: user.to_string(),
        nickname: item.creator.trim().to_string(),
        douban_id: extract_douban_id(&item.link)?,
        title: title.to_string(),
        description: item.description,
        pub_date: DateTime::parse_from_rfc2822(item.pub_date.trim())
            .ok()
            .map(|value| value.with_timezone(&Utc)),
    })
}

async fn process_wish(
    context: &PluginContext,
    item: &WishItem,
    existing: &mut HashSet<String>,
) -> Result<(Record, bool), String> {
    let subject = fetch_douban_subject(context, &item.douban_id).await?;
    if subject.id != item.douban_id {
        return Err("豆瓣条目 ID 不一致".to_string());
    }
    let media_type = douban_media_type(&subject)?;
    let season = (media_type == "tv").then(|| {
        extract_season(&subject.title)
            .or_else(|| extract_season(&item.title))
            .unwrap_or(1)
    });
    let search_title = if media_type == "tv" {
        strip_season(&subject.title)
    } else {
        subject.title.trim().to_string()
    };
    let year = value_as_i32(&subject.year);
    let resolved = resolve_tmdb(
        context,
        &search_title,
        tmdb_search_year(year, media_type, season),
        media_type,
    )
    .await?;
    if !resolved.media_type.trim().is_empty() && resolved.media_type != media_type {
        return Err(format!(
            "TMDB 返回类型 {}，与豆瓣类型 {media_type} 不一致",
            resolved.media_type
        ));
    }
    let season_episode_count = if let Some(season) = season {
        let tmdb_id = resolved
            .tmdb_id
            .parse::<i32>()
            .ok()
            .filter(|value| *value > 0)
            .ok_or_else(|| "TMDB ID 无效".to_string())?;
        let details = fetch_tmdb_details(context, tmdb_id).await?;
        Some(
            find_tmdb_season(&details, season)
                .ok_or_else(|| format!("TMDB 未确认存在第 {season} 季"))?
                .episode_count,
        )
        .filter(|value| *value > 0)
    } else {
        None
    };
    let key = subscription_key(media_type, &resolved.tmdb_id, season);
    let created = if existing.contains(&key) {
        false
    } else {
        create_subscription(
            context,
            &subject,
            &resolved,
            media_type,
            season,
            season_episode_count,
        )
        .await?;
        existing.insert(key);
        true
    };
    let display = if resolved.title.trim().is_empty() {
        subject.title.clone()
    } else {
        resolved.title.clone()
    };
    Ok((
        Record {
            douban_id: item.douban_id.clone(),
            douban_user: if item.nickname.is_empty() {
                item.douban_user.clone()
            } else {
                format!("{} ({})", item.nickname, item.douban_user)
            },
            title: display,
            tmdb_id: resolved.tmdb_id,
            media_type: if media_type == "movie" {
                "电影".to_string()
            } else {
                "剧集".to_string()
            },
            season_label: season
                .map(|value| format!("第 {value} 季"))
                .unwrap_or_else(|| "-".to_string()),
            status: if created {
                "已新增订阅"
            } else {
                "已有订阅"
            }
            .to_string(),
            updated_at: Local::now().to_rfc3339(),
        },
        created,
    ))
}

async fn fetch_douban_subject(
    context: &PluginContext,
    douban_id: &str,
) -> Result<DoubanSubject, String> {
    let response = context
        .client
        .get(format!("{}/{douban_id}", context.douban_api_base))
        .header("Referer", "https://m.douban.com/")
        .send()
        .await
        .map_err(|error| format!("请求豆瓣条目失败: {error}"))?;
    let status = response.status();
    let payload = response.text().await.unwrap_or_default();
    if !status.is_success() {
        return Err(format!(
            "豆瓣条目 HTTP {status}: {}",
            truncate(&payload, 120)
        ));
    }
    serde_json::from_str(&payload).map_err(|error| format!("豆瓣条目格式无效: {error}"))
}

async fn resolve_tmdb(
    context: &PluginContext,
    title: &str,
    year: Option<i32>,
    media_type: &str,
) -> Result<ResolvedMedia, String> {
    let payload = plugin_api_post(
        context,
        "/tmdb/resolve",
        json!({"title": title, "year": year, "media_type": media_type}),
    )
    .await?;
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
            let media_type = item.get("media_type")?.as_str()?;
            let tmdb_id = value_as_id(item.get("tmdb_id")?)?;
            let season = item.get("season").and_then(value_as_i32);
            Some(subscription_key(media_type, &tmdb_id, season))
        })
        .collect())
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

async fn create_subscription(
    context: &PluginContext,
    subject: &DoubanSubject,
    resolved: &ResolvedMedia,
    media_type: &str,
    season: Option<i32>,
    season_episode_count: Option<i32>,
) -> Result<(), String> {
    let tmdb_id = resolved
        .tmdb_id
        .parse::<i32>()
        .ok()
        .filter(|value| *value > 0)
        .ok_or_else(|| "TMDB ID 无效".to_string())?;
    let poster_path = resolved
        .poster_path
        .as_ref()
        .filter(|value| !value.trim().is_empty())
        .cloned()
        .or_else(|| (!subject.cover_url.trim().is_empty()).then(|| subject.cover_url.clone()));
    let description = resolved
        .description
        .as_ref()
        .filter(|value| !value.trim().is_empty())
        .cloned()
        .or_else(|| (!subject.intro.trim().is_empty()).then(|| subject.intro.clone()));
    let expected_episodes = season_episode_count
        .or(resolved.expected_episodes)
        .or((subject.episodes_count > 0).then_some(subject.episodes_count));
    plugin_api_post(
        context,
        "/subscriptions",
        json!({
            "tmdb_id": tmdb_id,
            "name": if resolved.title.trim().is_empty() { &subject.title } else { &resolved.title },
            "year": resolved.year.or_else(|| value_as_i32(&subject.year)),
            "season": season,
            "media_type": media_type,
            "poster_path": poster_path,
            "backdrop_path": resolved.backdrop_path,
            "vote_average": resolved.vote_average,
            "description": description,
            "expected_episodes": expected_episodes,
            "quality": "all"
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

fn parse_users(raw: &str) -> Result<Vec<String>, String> {
    let valid = Regex::new(r"^[A-Za-z0-9._-]+$").unwrap();
    let mut users = Vec::new();
    for user in raw
        .split(|character: char| character == ',' || character.is_whitespace())
        .map(str::trim)
        .filter(|value| !value.is_empty())
    {
        if !valid.is_match(user) {
            return Err(format!("豆瓣用户 ID 格式无效: {user}"));
        }
        if !users.iter().any(|value| value == user) {
            users.push(user.to_string());
        }
    }
    Ok(users)
}

fn douban_media_type(subject: &DoubanSubject) -> Result<&'static str, String> {
    match subject.r#type.trim().to_ascii_lowercase().as_str() {
        "movie" => Ok("movie"),
        "tv" => Ok("tv"),
        _ if subject.subtype.eq_ignore_ascii_case("movie") => Ok("movie"),
        _ if subject.subtype.eq_ignore_ascii_case("tv") => Ok("tv"),
        _ => Err("豆瓣条目不是可识别的电影或剧集".to_string()),
    }
}

fn extract_douban_id(link: &str) -> Option<String> {
    Regex::new(r"movie\.douban\.com/subject/(\d+)(?:/|$)")
        .unwrap()
        .captures(link.trim())
        .and_then(|captures| captures.get(1))
        .map(|value| value.as_str().to_string())
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

fn is_older_than(item: &WishItem, days: i64) -> bool {
    item.pub_date
        .is_some_and(|date| Utc::now().signed_duration_since(date).num_days() > days.max(1))
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

fn find_tmdb_season(details: &DetailsEnvelope, season: i32) -> Option<&TmdbSeason> {
    let seasons = if details.seasons.is_empty() {
        &details.details.seasons
    } else {
        &details.seasons
    };
    seasons.iter().find(|item| item.season_number == season)
}

fn tmdb_search_year(year: Option<i32>, media_type: &str, season: Option<i32>) -> Option<i32> {
    if media_type == "tv" && season.unwrap_or(1) > 1 {
        None
    } else {
        year
    }
}

fn compile_optional_regex(raw: &str, label: &str) -> Result<Option<Regex>, String> {
    let value = raw.trim();
    if value.is_empty() {
        Ok(None)
    } else {
        Regex::new(&format!("(?i){value}"))
            .map(Some)
            .map_err(|error| format!("{label}无效: {error}"))
    }
}

fn strip_html(value: &str) -> String {
    Regex::new(r"(?s)<[^>]*>")
        .unwrap()
        .replace_all(value, " ")
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

fn value_as_i32(value: &Value) -> Option<i32> {
    value
        .as_i64()
        .and_then(|value| i32::try_from(value).ok())
        .or_else(|| value.as_str()?.trim().parse().ok())
}

fn value_as_id(value: &Value) -> Option<String> {
    value
        .as_str()
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(ToString::to_string)
        .or_else(|| value.as_i64().map(|value| value.to_string()))
}

fn upsert_record(records: &mut RecordsData, record: Record) {
    if let Some(index) = records
        .items
        .iter()
        .position(|item| item.douban_id == record.douban_id)
    {
        records.items.remove(index);
    }
    records.items.push_front(record);
    trim_queue(&mut records.items, MAX_RECORDS);
}

fn update_summary(records: &mut RecordsData) {
    records.summary.total = records.items.len();
    records.summary.subscribed = records
        .items
        .iter()
        .filter(|item| item.status == "已新增订阅")
        .count();
    records.summary.existing = records
        .items
        .iter()
        .filter(|item| item.status == "已有订阅")
        .count();
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
        users: String::new(),
        days: default_days(),
        include: String::new(),
        exclude: String::new(),
        clear: false,
    }
}

fn default_days() -> i64 {
    7
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
        routing::{get, post},
    };
    use std::sync::Arc;
    use tokio::sync::Mutex;

    const SAMPLE_RSS: &str = r#"<?xml version="1.0" encoding="UTF-8"?>
<rss version="2.0" xmlns:dc="http://purl.org/dc/elements/1.1/"><channel>
<item><title>想看惩罚者2</title><link>https://movie.douban.com/subject/26710394/</link><description><![CDATA[<p>剧集</p>]]></description><dc:creator>Wei</dc:creator><pubDate>Mon, 10 Aug 2026 16:56:13 GMT</pubDate></item>
<item><title>看过祭屋出租</title><link>https://movie.douban.com/subject/36244313/</link><description></description><dc:creator>Wei</dc:creator><pubDate>Sun, 09 Aug 2026 08:28:38 GMT</pubDate></item>
</channel></rss>"#;

    #[test]
    fn parses_only_wish_items_from_real_douban_shape() {
        let rss: Rss = from_str(SAMPLE_RSS).unwrap();
        assert_eq!(rss.channel.items.len(), 2);
        let wish = wish_from_rss("nameless123", rss.channel.items[0].clone()).unwrap();
        assert_eq!(wish.title, "惩罚者2");
        assert_eq!(wish.douban_id, "26710394");
        assert_eq!(wish.nickname, "Wei");
        assert!(wish_from_rss("nameless123", rss.channel.items[1].clone()).is_none());
    }

    #[test]
    fn parses_users_and_rejects_url_injection() {
        assert_eq!(
            parse_users("alice, 123\nbob alice").unwrap(),
            vec!["alice", "123", "bob"]
        );
        assert!(parse_users("../admin").is_err());
        assert!(parse_users("https://douban.com/people/a").is_err());
    }

    #[test]
    fn extracts_tv_seasons_and_subscription_identity() {
        assert_eq!(extract_season("惩罚者 第一季"), Some(1));
        assert_eq!(extract_season("谜探路德维希 S02"), Some(2));
        assert_eq!(extract_season("庆余年2"), Some(2));
        assert_eq!(extract_season("庆余年 2"), Some(2));
        assert_eq!(extract_season("惩罚者 2017"), None);
        assert_eq!(strip_season("谜探路德维希 第二季"), "谜探路德维希");
        assert_eq!(strip_season("庆余年2"), "庆余年");
        assert_eq!(strip_season("庆余年 2"), "庆余年");
        assert_eq!(strip_season("惩罚者 2017"), "惩罚者 2017");
        assert_eq!(subscription_key("tv", "123", Some(2)), "tv:123:2");
        assert_eq!(subscription_key("movie", "123", None), "movie:123:0");
        assert_eq!(tmdb_search_year(Some(2026), "tv", Some(2)), None);
        assert_eq!(tmdb_search_year(Some(2026), "tv", Some(1)), Some(2026));
        assert_eq!(tmdb_search_year(Some(2026), "movie", None), Some(2026));
        let details: DetailsEnvelope = serde_json::from_value(json!({
            "seasons": [
                {"season_number": 1, "episode_count": 13},
                {"season_number": 2, "episode_count": 10}
            ]
        }))
        .unwrap();
        assert_eq!(find_tmdb_season(&details, 2).unwrap().episode_count, 10);
        assert!(find_tmdb_season(&details, 3).is_none());
    }

    #[test]
    fn validates_manifest_contract() {
        let manifest: Value =
            serde_json::from_str(include_str!("../../douban-wish/plugin.json")).unwrap();
        assert_eq!(manifest["id"], "douban-wish");
        assert_eq!(manifest["version"], "1.0.0");
        assert_eq!(
            manifest["requested_scopes"],
            json!(["catalog:read", "subscriptions:read", "subscriptions:write"])
        );
    }

    #[derive(Clone, Default)]
    struct MockApi {
        subscriptions: Arc<Mutex<Vec<Value>>>,
    }

    async fn mock_list(State(state): State<MockApi>) -> Json<Value> {
        Json(Value::Array(state.subscriptions.lock().await.clone()))
    }

    async fn mock_resolve() -> Json<Value> {
        Json(json!({
            "status": "success",
            "data": {
                "tmdb_id": "123",
                "title": "惩罚者",
                "type": "tv",
                "year": 2017,
                "poster_path": "/poster.jpg",
                "expected_episodes": 13
            }
        }))
    }

    async fn mock_details() -> Json<Value> {
        Json(json!({
            "details": {
                "seasons": [
                    {"season_number": 1, "episode_count": 13},
                    {"season_number": 2, "episode_count": 10}
                ]
            },
            "seasons": [
                {"season_number": 1, "episode_count": 13},
                {"season_number": 2, "episode_count": 10}
            ]
        }))
    }

    async fn mock_create(State(state): State<MockApi>, Json(payload): Json<Value>) -> Json<Value> {
        state.subscriptions.lock().await.push(payload);
        Json(json!({"id": 1}))
    }

    async fn mock_feed() -> &'static str {
        SAMPLE_RSS
    }

    async fn mock_subject() -> Json<Value> {
        Json(json!({
            "id": "26710394",
            "title": "惩罚者2",
            "type": "tv",
            "subtype": "tv",
            "year": "2017",
            "intro": "一部剧集",
            "cover_url": "https://example.com/poster.jpg",
            "episodes_count": 13
        }))
    }

    async fn test_context(base_url: String) -> PluginContext {
        let data_dir =
            env::temp_dir().join(format!("mediary-douban-wish-test-{}", std::process::id()));
        PluginContext {
            api_url: format!("{base_url}/api"),
            douban_feed_base: format!("{base_url}/feed/people"),
            douban_api_base: format!("{base_url}/douban/movie"),
            token: "test-token".to_string(),
            data_dir,
            client: Client::builder().build().unwrap(),
            settings: Settings {
                users: "nameless123".to_string(),
                days: 3650,
                ..default_settings()
            },
            include: None,
            exclude: None,
        }
    }

    #[tokio::test]
    async fn refreshes_feed_and_creates_tv_subscription_idempotently() {
        let api = MockApi::default();
        let app = Router::new()
            .route("/feed/people/nameless123/interests", get(mock_feed))
            .route("/douban/movie/26710394", get(mock_subject))
            .route("/api/tmdb/resolve", post(mock_resolve))
            .route("/api/search/tmdb/details", get(mock_details))
            .route("/api/subscriptions", get(mock_list).post(mock_create))
            .with_state(api.clone());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let context = test_context(format!("http://{address}")).await;
        let mut state = PluginState::default();
        let mut records = RecordsData::default();

        let first = refresh(&context, &mut state, &mut records).await.unwrap();
        let second = refresh(&context, &mut state, &mut records).await.unwrap();
        assert_eq!(first.fetched, 2);
        assert_eq!(first.wishes, 1);
        assert_eq!(first.subscribed, 1);
        assert_eq!(second.subscribed, 0);
        assert_eq!(second.skipped_history, 1);
        assert_eq!(records.items.len(), 1);

        let subscriptions = api.subscriptions.lock().await;
        assert_eq!(subscriptions.len(), 1);
        assert_eq!(subscriptions[0]["tmdb_id"], 123);
        assert_eq!(subscriptions[0]["media_type"], "tv");
        assert_eq!(subscriptions[0]["season"], 2);
        assert_eq!(subscriptions[0]["expected_episodes"], 10);
        drop(subscriptions);

        let existing = fetch_existing_subscription_keys(&context).await.unwrap();
        assert!(existing.contains("tv:123:2"));
    }
}
