use chrono::{DateTime, Datelike, Local, NaiveDate, Timelike, Weekday};
use reqwest::Client;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{
    cmp::Ordering,
    collections::{HashMap, VecDeque},
    env, fs,
    path::{Path, PathBuf},
    time::Duration,
};

const TMDB_BACKDROP_BASE_URL: &str = "https://image.tmdb.org/t/p/w1280";
const MAX_HISTORY_ITEMS: usize = 100;
const MAX_SENT_DATES: usize = 90;
// WeCom news `description` is cut past 512 UTF-8 bytes, and textcard
// `description` is cut past 512 characters
// (https://developer.work.weixin.qq.com/document/path/90236). Paging on 512
// bytes stays inside both limits. A show is never split across pages; only a
// single show that is itself over the budget is trimmed.
const WECOM_DESCRIPTION_MAX_BYTES: usize = 512;

#[derive(Clone, Deserialize)]
struct Settings {
    #[serde(default = "default_true")]
    show_progress: bool,
    #[serde(default)]
    notify_when_empty: bool,
}

struct PluginContext {
    api_url: String,
    token: String,
    data_dir: PathBuf,
    trigger: String,
    client: Client,
    settings: Settings,
}

#[derive(Default, Deserialize)]
struct AiringResponse {
    #[serde(default)]
    items: Vec<AiringSubscription>,
    #[serde(default)]
    failures: Vec<String>,
}

#[derive(Clone, Deserialize)]
struct AiringSubscription {
    id: i64,
    name: String,
    year: Option<i32>,
    tmdb_id: i32,
    season: Option<i32>,
    #[serde(default)]
    episodes: Vec<i32>,
    expected_episodes: Option<i32>,
    poster_path: Option<String>,
    backdrop_path: Option<String>,
    air_at: Option<String>,
    #[serde(default)]
    collected_episodes: i32,
}

#[derive(Clone)]
struct ReminderItem {
    subscription: AiringSubscription,
    episode_numbers: Vec<i32>,
    local_air_at: Option<DateTime<Local>>,
}

#[derive(Default, Deserialize, Serialize)]
struct History {
    #[serde(default)]
    updated_at: String,
    #[serde(default)]
    summary: HistorySummary,
    #[serde(default)]
    items: VecDeque<HistoryRecord>,
    #[serde(default)]
    sent_dates: VecDeque<String>,
}

#[derive(Default, Deserialize, Serialize)]
struct HistorySummary {
    #[serde(default)]
    runs: usize,
    #[serde(default)]
    notifications: usize,
    #[serde(default)]
    last_count: usize,
    #[serde(default)]
    last_result: String,
}

#[derive(Serialize, Deserialize)]
struct HistoryRecord {
    date: String,
    result: String,
    count: usize,
    notification_count: usize,
    trigger: String,
    finished_at: String,
}

#[derive(Default, Deserialize)]
struct TmdbDetailsEnvelope {
    #[serde(default)]
    details: TmdbDetails,
}

#[derive(Default, Deserialize)]
struct TmdbDetails {
    next_episode_to_air: Option<TmdbEpisode>,
}

#[derive(Deserialize)]
struct TmdbEpisode {
    episode_number: Option<i32>,
    air_date: Option<String>,
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
    if action != "remind" {
        return Err(format!("不支持的订阅提醒动作: {action}"));
    }

    let context = PluginContext::from_env()?;
    let today = Local::now().date_naive();
    let today_key = today.format("%Y-%m-%d").to_string();
    let history_path = context.data_dir.join("history.json");
    let mut history = load_json::<History>(&history_path);
    let scheduled = context.trigger == "schedule";

    if scheduled && history.sent_dates.iter().any(|date| date == &today_key) {
        let result = "今日定时提醒已发送，跳过重复执行".to_string();
        record_run(&mut history, &today_key, &result, 0, 0, &context.trigger);
        write_json(&history_path, &history)?;
        println!("{}", json!({"notice": result, "count": 0, "skipped": true}));
        return Ok(());
    }

    let airing = fetch_airing_subscriptions(&context, &today_key).await?;
    let failure_count = airing.failures.len();
    let mut reminders = airing
        .items
        .into_iter()
        .map(|subscription| ReminderItem {
            local_air_at: parse_local_airtime(subscription.air_at.as_deref()),
            episode_numbers: subscription.episodes.clone(),
            subscription,
        })
        .collect::<Vec<_>>();

    enrich_episode_numbers(&context, &today_key, &mut reminders).await;
    reminders.sort_by(compare_reminders);

    let notification_count = if reminders.is_empty() {
        if context.settings.notify_when_empty && failure_count == 0 {
            send_notification(
                &context,
                &format!("📺 今日追更 · {}", friendly_date(today)),
                "今天没有订阅剧集更新，可以安心补完片单。",
                None,
            )
            .await?;
            1
        } else {
            0
        }
    } else {
        send_reminder_batches(&context, today, &reminders).await?
    };

    if scheduled {
        history.sent_dates.push_front(today_key.clone());
        while history.sent_dates.len() > MAX_SENT_DATES {
            history.sent_dates.pop_back();
        }
    }

    let mut result = if reminders.is_empty() {
        if notification_count == 0 {
            if failure_count == 0 {
                "今天没有剧集更新，未发送通知".to_string()
            } else {
                "未确认到今日更新，部分订阅排期查询失败".to_string()
            }
        } else {
            "今天没有剧集更新，已发送空提醒".to_string()
        }
    } else {
        format!(
            "已提醒 {} 部今日更新剧集，共 {} 条通知",
            reminders.len(),
            notification_count
        )
    };
    if failure_count > 0 {
        result.push_str(&format!("，{} 部排期查询失败", failure_count));
    }
    record_run(
        &mut history,
        &today_key,
        &result,
        reminders.len(),
        notification_count,
        &context.trigger,
    );
    write_json(&history_path, &history)?;
    println!(
        "{}",
        json!({
            "notice": result,
            "count": reminders.len(),
            "notifications": notification_count
        })
    );
    Ok(())
}

impl PluginContext {
    fn from_env() -> Result<Self, String> {
        let settings = env::var("MEDIARY_PLUGIN_SETTINGS_JSON")
            .ok()
            .filter(|value| !value.trim().is_empty())
            .map(|raw| serde_json::from_str(&raw).map_err(|error| error.to_string()))
            .transpose()?
            .unwrap_or_else(default_settings);
        let data_dir = PathBuf::from(required_env("MEDIARY_PLUGIN_DATA_DIR")?);
        fs::create_dir_all(&data_dir).map_err(|error| error.to_string())?;
        let client = Client::builder()
            .timeout(Duration::from_secs(30))
            .user_agent("Mediary/subscribe-reminder")
            .build()
            .map_err(|error| error.to_string())?;
        Ok(Self {
            api_url: required_env("MEDIARY_PLUGIN_API_URL")?
                .trim_end_matches('/')
                .to_string(),
            token: required_env("MEDIARY_PLUGIN_TOKEN")?,
            trigger: env::var("MEDIARY_PLUGIN_TRIGGER").unwrap_or_else(|_| "manual".to_string()),
            data_dir,
            client,
            settings,
        })
    }
}

async fn fetch_airing_subscriptions(
    context: &PluginContext,
    today: &str,
) -> Result<AiringResponse, String> {
    let payload = plugin_api_get(
        context,
        &format!("/plugin/subscriptions/airing?date={today}"),
    )
    .await?;
    serde_json::from_value(payload).map_err(|error| format!("剧集排期响应格式无效: {error}"))
}

async fn enrich_episode_numbers(
    context: &PluginContext,
    today: &str,
    reminders: &mut [ReminderItem],
) {
    let mut cache = HashMap::<i32, Vec<i32>>::new();
    for reminder in reminders {
        if !reminder.episode_numbers.is_empty() {
            continue;
        }
        let tmdb_id = reminder.subscription.tmdb_id;
        if let Some(cached) = cache.get(&tmdb_id) {
            reminder.episode_numbers = cached.clone();
            continue;
        }
        let episode_numbers = fetch_episode_number(context, tmdb_id, today)
            .await
            .into_iter()
            .collect::<Vec<_>>();
        cache.insert(tmdb_id, episode_numbers.clone());
        reminder.episode_numbers = episode_numbers;
    }
}

async fn fetch_episode_number(context: &PluginContext, tmdb_id: i32, today: &str) -> Option<i32> {
    let path = format!("/search/tmdb/details?id={tmdb_id}&media_type=tv");
    let payload = plugin_api_get(context, &path).await.ok()?;
    let envelope = serde_json::from_value::<TmdbDetailsEnvelope>(payload).ok()?;
    let episode = envelope.details.next_episode_to_air?;
    (episode.air_date.as_deref() == Some(today))
        .then_some(episode.episode_number)
        .flatten()
}

async fn send_reminder_batches(
    context: &PluginContext,
    today: NaiveDate,
    reminders: &[ReminderItem],
) -> Result<usize, String> {
    let now = Local::now();
    let show_progress = context.settings.show_progress;
    let pages = reminder_page_ranges(reminders, show_progress, now);
    let total_pages = pages.len();
    for (index, (start, end)) in pages.into_iter().enumerate() {
        let batch = &reminders[start..end];
        let title = reminder_title(today, index, total_pages);
        let content =
            limit_wecom_description(format_page(batch, reminders.len(), show_progress, now));
        let image_url = batch.iter().find_map(reminder_image_url);
        send_notification(context, &title, &content, image_url.as_deref()).await?;
    }
    Ok(total_pages)
}

fn reminder_title(today: NaiveDate, page_index: usize, page_count: usize) -> String {
    let suffix = (page_count > 1).then(|| format!(" · {}/{}", page_index + 1, page_count));
    format!(
        "📺 今日追更 · {}{}",
        friendly_date(today),
        suffix.unwrap_or_default()
    )
}

/// Pack whole shows until the next one would pass the WeCom byte budget.
/// A single show is never divided; `limit_wecom_description` trims that rare
/// oversized entry when the page is sent.
fn reminder_page_ranges(
    reminders: &[ReminderItem],
    show_progress: bool,
    now: DateTime<Local>,
) -> Vec<(usize, usize)> {
    let total = reminders.len();
    let mut pages = Vec::new();
    let mut start = 0;
    while start < total {
        let mut end = start + 1;
        while end < total {
            let content = format_page(&reminders[start..=end], total, show_progress, now);
            if content.len() > WECOM_DESCRIPTION_MAX_BYTES {
                break;
            }
            end += 1;
        }
        pages.push((start, end));
        start = end;
    }
    pages
}

fn format_page(
    reminders: &[ReminderItem],
    total: usize,
    show_progress: bool,
    now: DateTime<Local>,
) -> String {
    let mut lines = Vec::with_capacity(reminders.len() * 2 + 1);
    lines.push(format!("今天有 {total} 部订阅剧集更新"));
    for reminder in reminders {
        lines.push(String::new());
        lines.push(format_reminder_line(reminder, show_progress, now));
    }
    lines.join("\n")
}

fn format_reminder_line(
    reminder: &ReminderItem,
    show_progress: bool,
    now: DateTime<Local>,
) -> String {
    let mut lines = Vec::with_capacity(3);
    if let Some(value) = reminder.local_air_at {
        let state = if value <= now {
            "已更新"
        } else {
            "待播出"
        };
        lines.push(format!(
            "{:02}:{:02}  {state}",
            value.hour(),
            value.minute()
        ));
    }
    lines.push(format!(
        "{}{} · {}",
        reminder.subscription.name,
        reminder
            .subscription
            .year
            .map(|year| format!(" ({year})"))
            .unwrap_or_default(),
        season_episode_label(reminder)
    ));
    if show_progress
        && let Some(expected) = reminder
            .subscription
            .expected_episodes
            .filter(|value| *value > 0)
    {
        lines.push(format!(
            "订阅进度  {}/{}",
            reminder.subscription.collected_episodes.max(0),
            expected
        ));
    }
    lines.join("\n")
}

fn limit_wecom_description(content: String) -> String {
    if content.len() <= WECOM_DESCRIPTION_MAX_BYTES {
        return content;
    }
    let ellipsis = "…";
    let mut end = WECOM_DESCRIPTION_MAX_BYTES.saturating_sub(ellipsis.len());
    while end > 0 && !content.is_char_boundary(end) {
        end -= 1;
    }
    let mut trimmed = content[..end].trim_end().to_string();
    trimmed.push_str(ellipsis);
    trimmed
}

fn season_episode_label(reminder: &ReminderItem) -> String {
    let mut episodes = reminder
        .episode_numbers
        .iter()
        .copied()
        .filter(|value| *value > 0)
        .collect::<Vec<_>>();
    episodes.sort_unstable();
    episodes.dedup();
    let episode_label =
        match episodes.as_slice() {
            [] => None,
            [episode] => Some(format!("E{episode:02}")),
            episodes if episodes.windows(2).all(|window| window[1] == window[0] + 1) => Some(
                format!("E{:02}-{:02}", episodes[0], episodes[episodes.len() - 1]),
            ),
            episodes => Some(
                episodes
                    .iter()
                    .map(|episode| format!("E{episode:02}"))
                    .collect::<Vec<_>>()
                    .join(","),
            ),
        };
    match (reminder.subscription.season, episode_label) {
        (Some(season), Some(episodes)) => format!("S{season:02}{episodes}"),
        (Some(season), None) => format!("S{season:02} · 新集"),
        (None, Some(episodes)) => episodes,
        (None, None) => "新集".to_string(),
    }
}

fn reminder_image_url(reminder: &ReminderItem) -> Option<String> {
    reminder
        .subscription
        .backdrop_path
        .as_deref()
        .or(reminder.subscription.poster_path.as_deref())
        .and_then(tmdb_image_url)
}

fn tmdb_image_url(path: &str) -> Option<String> {
    let path = path.trim();
    if path.starts_with("https://") {
        Some(path.to_string())
    } else if path.starts_with('/') {
        Some(format!("{TMDB_BACKDROP_BASE_URL}{path}"))
    } else {
        None
    }
}

fn compare_reminders(left: &ReminderItem, right: &ReminderItem) -> Ordering {
    match (left.local_air_at, right.local_air_at) {
        (Some(left_time), Some(right_time)) => left_time
            .cmp(&right_time)
            .then_with(|| left_name(&left.subscription, &right.subscription)),
        (Some(_), None) => Ordering::Less,
        (None, Some(_)) => Ordering::Greater,
        (None, None) => left_name(&left.subscription, &right.subscription),
    }
}

fn left_name(left: &AiringSubscription, right: &AiringSubscription) -> Ordering {
    left.name
        .to_lowercase()
        .cmp(&right.name.to_lowercase())
        .then_with(|| left.id.cmp(&right.id))
}

fn parse_local_airtime(value: Option<&str>) -> Option<DateTime<Local>> {
    value
        .and_then(|value| DateTime::parse_from_rfc3339(value).ok())
        .map(|value| value.with_timezone(&Local))
}

fn friendly_date(date: NaiveDate) -> String {
    format!(
        "{}月{}日 {}",
        date.month(),
        date.day(),
        weekday_label(date.weekday())
    )
}

fn weekday_label(weekday: Weekday) -> &'static str {
    match weekday {
        Weekday::Mon => "周一",
        Weekday::Tue => "周二",
        Weekday::Wed => "周三",
        Weekday::Thu => "周四",
        Weekday::Fri => "周五",
        Weekday::Sat => "周六",
        Weekday::Sun => "周日",
    }
}

async fn send_notification(
    context: &PluginContext,
    title: &str,
    content: &str,
    image_url: Option<&str>,
) -> Result<(), String> {
    plugin_api_post(
        context,
        "/plugin/notifications",
        json!({
            "title": title,
            "content": content,
            "image_url": image_url
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
    parse_plugin_api_response(response).await
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
    parse_plugin_api_response(response).await
}

async fn parse_plugin_api_response(response: reqwest::Response) -> Result<Value, String> {
    let status = response.status();
    let payload = response.text().await.unwrap_or_default();
    if !status.is_success() {
        return Err(format!(
            "Mediary API {status}: {}",
            payload.chars().take(200).collect::<String>()
        ));
    }
    serde_json::from_str(&payload).map_err(|error| error.to_string())
}

fn record_run(
    history: &mut History,
    date: &str,
    result: &str,
    count: usize,
    notification_count: usize,
    trigger: &str,
) {
    let finished_at = Local::now().to_rfc3339();
    history.summary.runs += 1;
    history.summary.notifications += notification_count;
    history.summary.last_count = count;
    history.summary.last_result = result.to_string();
    history.updated_at = finished_at.clone();
    history.items.push_front(HistoryRecord {
        date: date.to_string(),
        result: result.to_string(),
        count,
        notification_count,
        trigger: if trigger == "schedule" {
            "定时".to_string()
        } else {
            "手动".to_string()
        },
        finished_at,
    });
    while history.items.len() > MAX_HISTORY_ITEMS {
        history.items.pop_back();
    }
}

fn load_json<T: for<'de> Deserialize<'de> + Default>(path: &Path) -> T {
    fs::read_to_string(path)
        .ok()
        .and_then(|raw| serde_json::from_str(&raw).ok())
        .unwrap_or_default()
}

fn write_json(path: &Path, value: &History) -> Result<(), String> {
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
        show_progress: true,
        notify_when_empty: false,
    }
}

fn default_true() -> bool {
    true
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::{Local, NaiveDate, TimeZone};

    fn subscription() -> AiringSubscription {
        AiringSubscription {
            id: 1,
            name: "测试剧集".to_string(),
            year: Some(2026),
            tmdb_id: 42,
            season: Some(1),
            episodes: vec![9],
            expected_episodes: Some(12),
            poster_path: None,
            backdrop_path: Some("/backdrop.jpg".to_string()),
            air_at: Some("2026-07-27T12:00:00Z".to_string()),
            collected_episodes: 8,
        }
    }

    #[test]
    fn formats_single_ranges_and_fallback_episode_labels() {
        let subscription = subscription();
        let mut reminder = ReminderItem {
            subscription,
            episode_numbers: vec![9],
            local_air_at: None,
        };
        assert_eq!(season_episode_label(&reminder), "S01E09");
        reminder.episode_numbers = vec![9, 10];
        assert_eq!(season_episode_label(&reminder), "S01E09-10");
        reminder.episode_numbers.clear();
        assert_eq!(season_episode_label(&reminder), "S01 · 新集");
    }

    #[test]
    fn formats_tmdb_backdrop_urls() {
        assert_eq!(
            tmdb_image_url("/backdrop.jpg").as_deref(),
            Some("https://image.tmdb.org/t/p/w1280/backdrop.jpg")
        );
        assert_eq!(
            tmdb_image_url("https://images.example/show.jpg").as_deref(),
            Some("https://images.example/show.jpg")
        );
        assert_eq!(tmdb_image_url("relative.jpg"), None);
    }

    fn preview_now() -> DateTime<Local> {
        Local.with_ymd_and_hms(2026, 11, 10, 12, 0, 0).unwrap()
    }

    fn preview_day() -> NaiveDate {
        NaiveDate::from_ymd_opt(2026, 11, 10).unwrap()
    }

    fn named_reminder(id: i64, name: &str, with_progress: bool) -> ReminderItem {
        let mut item = subscription();
        item.id = id;
        item.name = name.to_string();
        item.year = Some(2026);
        item.season = Some(1);
        item.episodes = vec![id as i32 + 1];
        item.expected_episodes = with_progress.then_some(12);
        item.collected_episodes = 3;
        ReminderItem {
            subscription: item,
            episode_numbers: vec![id as i32 + 1],
            local_air_at: None,
        }
    }

    /// Long Chinese titles with year, season/episode and progress. Ids are unique
    /// so a formatted block cannot appear inside another show.
    fn long_chinese_reminders(count: usize) -> Vec<ReminderItem> {
        (0..count)
            .map(|index| {
                let id = index as i64;
                let mut item = subscription();
                item.id = id + 1;
                item.name = format!("来自深海的星河回响与未完成的长夜剧目{id:02}");
                item.year = Some(2016 + (id % 10) as i32);
                item.season = Some(1 + (id % 3) as i32);
                let episode = (id as i32) + 1;
                item.episodes = vec![episode];
                item.expected_episodes = Some(24);
                item.collected_episodes = (id % 20) as i32;
                item.air_at = None;
                ReminderItem {
                    subscription: item,
                    episode_numbers: vec![episode],
                    local_air_at: Some(
                        Local
                            .with_ymd_and_hms(
                                2026,
                                11,
                                10,
                                8 + (id as u32 % 12),
                                (id as u32 * 7) % 60,
                                0,
                            )
                            .unwrap(),
                    ),
                }
            })
            .collect()
    }

    /// Real shows first, then other common titles of similar length. Counts past
    /// the catalog repeat from the start.
    const PREVIEW_SHOWS: &'static [(&'static str, i32, i32, &'static [i32])] = &[
        ("凡人修仙传", 2020, 1, &[199]),
        ("百日谎言", 2026, 1, &[9]),
        ("光阴之外", 2025, 1, &[47]),
        ("尚公主", 2026, 1, &[30, 31]),
        ("折月亮", 2026, 1, &[7, 8]),
        ("择日飞升", 2026, 1, &[19]),
        ("斗破苍穹", 2017, 5, &[218]),
        ("走马上任", 2026, 1, &[14, 15]),
        ("庆余年", 2019, 2, &[21]),
        ("仙逆", 2023, 1, &[64]),
        ("遮天", 2023, 1, &[52]),
        ("完美世界", 2021, 1, &[208]),
        ("斗罗大陆", 2018, 1, &[221]),
        ("吞噬星空", 2020, 1, &[133]),
        ("一念永恒", 2020, 3, &[96]),
        ("苍兰诀", 2022, 1, &[36]),
        ("莲花楼", 2023, 1, &[40]),
        ("狂飙", 2023, 1, &[39]),
        ("繁花", 2024, 1, &[30]),
        ("三体", 2023, 1, &[30]),
    ];

    /// `show_progress` is off. Every 4th show has an air time; the rest are one line.
    fn realistic_reminders(count: usize) -> Vec<ReminderItem> {
        (0..count)
            .map(|index| {
                let (name, year, season, episodes) = PREVIEW_SHOWS[index % PREVIEW_SHOWS.len()];
                let mut item = subscription();
                item.id = (index as i64) + 1;
                item.name = name.to_string();
                item.year = Some(year);
                item.season = Some(season);
                item.episodes = episodes.to_vec();
                item.expected_episodes = None;
                item.collected_episodes = 0;
                item.air_at = None;
                let local_air_at = (index % 4 == 0).then(|| {
                    let slot = (index / 4) as u32;
                    let minute = 15 + (slot % 4) * 10;
                    let hour = if slot % 2 == 0 { 9 } else { 20 };
                    Local
                        .with_ymd_and_hms(2026, 11, 10, hour, minute, 0)
                        .unwrap()
                });
                ReminderItem {
                    subscription: item,
                    episode_numbers: episodes.to_vec(),
                    local_air_at,
                }
            })
            .collect()
    }

    fn rendered_pages(
        reminders: &[ReminderItem],
        show_progress: bool,
        now: DateTime<Local>,
    ) -> Vec<(String, String, usize)> {
        let today = preview_day();
        let ranges = reminder_page_ranges(reminders, show_progress, now);
        let page_count = ranges.len();
        ranges
            .into_iter()
            .enumerate()
            .map(|(index, (start, end))| {
                let title = reminder_title(today, index, page_count);
                let content = limit_wecom_description(format_page(
                    &reminders[start..end],
                    reminders.len(),
                    show_progress,
                    now,
                ));
                (title, content, end - start)
            })
            .collect()
    }

    fn assert_byte_pages(reminders: &[ReminderItem]) {
        let now = preview_now();
        let total = reminders.len();
        let ranges = reminder_page_ranges(reminders, true, now);
        assert!(ranges.len() > 1, "{total} shows must paginate");
        let mut cursor = 0;
        let mut seen = std::collections::HashSet::new();
        for (page_index, (start, end)) in ranges.iter().copied().enumerate() {
            assert_eq!(start, cursor, "page {page_index} leaves a gap or overlap");
            assert!(end > start);
            for reminder in &reminders[start..end] {
                let alone = format_page(std::slice::from_ref(reminder), total, true, now);
                assert!(
                    alone.len() <= WECOM_DESCRIPTION_MAX_BYTES,
                    "fixture show must fit on its own, got {}",
                    alone.len()
                );
            }
            let raw = format_page(&reminders[start..end], total, true, now);
            let content = limit_wecom_description(raw.clone());
            assert!(
                content.len() <= WECOM_DESCRIPTION_MAX_BYTES,
                "page {} is {} bytes",
                page_index + 1,
                content.len()
            );
            assert_eq!(content, raw, "in-budget page must not be truncated");
            let header = format!("今天有 {total} 部订阅剧集更新");
            assert_eq!(content.lines().next(), Some(header.as_str()));
            let mut search_from = 0;
            for reminder in &reminders[start..end] {
                let block = format_reminder_line(reminder, true, now);
                let found = content[search_from..]
                    .find(&block)
                    .unwrap_or_else(|| panic!("missing block on page {}: {block}", page_index + 1));
                search_from += found + block.len();
                assert!(seen.insert(block.clone()), "duplicate block: {block}");
            }
            if end < total {
                let overflow = format_page(&reminders[start..=end], total, true, now);
                assert!(
                    overflow.len() > WECOM_DESCRIPTION_MAX_BYTES,
                    "page {} could fit another show ({} bytes)",
                    page_index + 1,
                    overflow.len()
                );
            }
            cursor = end;
        }
        assert_eq!(cursor, total);
        assert_eq!(seen.len(), total);
    }

    #[test]
    fn restores_spaced_multiline_layout_from_0_1_2() {
        let now = preview_now();
        let mut aired = subscription();
        aired.name = "晨间剧".to_string();
        aired.collected_episodes = 1;
        aired.expected_episodes = Some(8);
        let reminders = [
            ReminderItem {
                subscription: subscription(),
                episode_numbers: vec![9],
                local_air_at: Some(Local.with_ymd_and_hms(2026, 11, 10, 18, 0, 0).unwrap()),
            },
            ReminderItem {
                subscription: aired,
                episode_numbers: vec![3],
                local_air_at: Some(Local.with_ymd_and_hms(2026, 11, 10, 9, 30, 0).unwrap()),
            },
        ];
        let content = format_page(&reminders, reminders.len(), true, now);
        assert_eq!(
            content,
            "今天有 2 部订阅剧集更新\n\
             \n\
             18:00  待播出\n\
             测试剧集 (2026) · S01E09\n\
             订阅进度  8/12\n\
             \n\
             09:30  已更新\n\
             晨间剧 (2026) · S01E03\n\
             订阅进度  1/8"
        );
    }

    #[test]
    fn omits_redundant_date_label_without_exact_airtime() {
        let reminder = ReminderItem {
            subscription: subscription(),
            episode_numbers: vec![9, 10],
            local_air_at: None,
        };
        let content = format_page(&[reminder], 1, true, Local::now());
        assert!(!content.contains("今日"));
        assert!(!content.contains("更新日"));
        assert_eq!(
            content,
            "今天有 1 部订阅剧集更新\n\
             \n\
             测试剧集 (2026) · S01E09-10\n\
             订阅进度  8/12"
        );
    }

    #[test]
    fn hides_progress_when_disabled() {
        let reminder = ReminderItem {
            subscription: subscription(),
            episode_numbers: vec![9],
            local_air_at: None,
        };
        let content = format_page(std::slice::from_ref(&reminder), 1, false, Local::now());
        assert_eq!(
            content,
            "今天有 1 部订阅剧集更新\n\n测试剧集 (2026) · S01E09"
        );
    }

    #[test]
    fn ignores_legacy_max_per_message_field() {
        let settings: Settings = serde_json::from_str(
            r#"{"show_progress":false,"notify_when_empty":true,"max_per_message":6}"#,
        )
        .unwrap();
        assert!(!settings.show_progress);
        assert!(settings.notify_when_empty);

        let defaults: Settings = serde_json::from_str(r#"{"max_per_message":"12"}"#).unwrap();
        assert!(defaults.show_progress);
        assert!(!defaults.notify_when_empty);
    }

    #[test]
    fn pages_ten_long_chinese_titles_within_512_bytes() {
        assert_byte_pages(&long_chinese_reminders(10));
    }

    #[test]
    fn pages_twenty_long_chinese_titles_within_512_bytes() {
        assert_byte_pages(&long_chinese_reminders(20));
    }

    #[test]
    fn pages_thirty_long_chinese_titles_within_512_bytes() {
        assert_byte_pages(&long_chinese_reminders(30));
    }

    #[test]
    fn trims_a_single_oversized_show_to_the_wecom_byte_cap() {
        let now = Local::now();
        let reminders = vec![named_reminder(1, &"剧".repeat(200), true)];
        assert_eq!(reminder_page_ranges(&reminders, true, now), vec![(0, 1)]);
        let raw = format_page(&reminders, 1, true, now);
        assert!(raw.len() > WECOM_DESCRIPTION_MAX_BYTES);
        let limited = limit_wecom_description(raw);
        assert!(limited.len() <= WECOM_DESCRIPTION_MAX_BYTES);
        assert!(limited.ends_with('…'));
    }

    fn html_escape(value: &str) -> String {
        value
            .replace('&', "&amp;")
            .replace('<', "&lt;")
            .replace('>', "&gt;")
    }

    #[test]
    fn realistic_preview_hides_progress_and_times_about_a_quarter() {
        let now = preview_now();
        let reminders = realistic_reminders(30);
        assert_eq!(
            format_reminder_line(&reminders[1], false, now),
            "百日谎言 (2026) · S01E09"
        );
        assert_eq!(
            format_reminder_line(&reminders[3], false, now),
            "尚公主 (2026) · S01E30-31"
        );
        assert_eq!(
            format_reminder_line(&reminders[6], false, now),
            "斗破苍穹 (2017) · S05E218"
        );
        assert_eq!(
            format_reminder_line(&reminders[7], false, now),
            "走马上任 (2026) · S01E14-15"
        );
        let timed = reminders
            .iter()
            .filter(|reminder| reminder.local_air_at.is_some())
            .count();
        assert_eq!(timed, 8);
        for count in [10usize, 20, 30] {
            let reminders = realistic_reminders(count);
            let pages = rendered_pages(&reminders, false, now);
            let mut covered = 0;
            for (title, content, items) in &pages {
                assert!(content.len() <= WECOM_DESCRIPTION_MAX_BYTES, "{title}");
                assert!(!content.contains("订阅进度"));
                assert!(content.starts_with(&format!("今天有 {count} 部订阅剧集更新")));
                covered += items;
            }
            assert_eq!(covered, count);
            if pages.len() == 1 {
                assert!(!title_has_page_suffix(&pages[0].0));
            }
        }
        let first = format_reminder_line(&reminders[0], false, now);
        assert!(first.starts_with("09:15  已更新\n凡人修仙传 (2020) · S01E199"));
        let fifth = format_reminder_line(&reminders[4], false, now);
        assert!(fifth.starts_with("20:25  待播出\n折月亮 (2026) · S01E07-08"));
    }

    fn title_has_page_suffix(title: &str) -> bool {
        title.contains(" · 1/")
    }

    /// Writes the 10/20/30 previews from the real formatters. Ignored in CI.
    #[test]
    #[ignore = "writes /opt/cursor/artifacts/wecom-preview.md and wecom-preview.html"]
    fn export_wecom_preview() {
        let now = preview_now();
        let mut markdown = String::from(
            "# 订阅提醒分页预览\n\n\
             由 `reminder_page_ranges`、`format_page`、`limit_wecom_description` 和 `reminder_title` 生成。\
             `show_progress=false`。约 1/4 条目有播出时间（12:00 前为已更新，之后为待播出），其余只有剧名一行。\
             片名从给定剧目起循环，并补了相近长度的国产剧/动画名。\
             日期 2026-11-10。字节数为 description 的 UTF-8 长度。\n\n",
        );
        let mut groups = String::new();
        for count in [10usize, 20, 30] {
            let reminders = realistic_reminders(count);
            let pages = rendered_pages(&reminders, false, now);
            markdown.push_str(&format!("## {count} 条\n\n"));
            groups.push_str(&format!(
                "<section id=\"group-{count}\"><h2>{count} 条</h2>\n"
            ));
            for (index, (title, content, items)) in pages.iter().enumerate() {
                markdown.push_str(&format!(
                    "### 第 {} 页\n\n标题：`{title}`\n\n条数：{items}\n\n字节数：{}\n\n```text\n{content}\n```\n\n",
                    index + 1,
                    content.len()
                ));
                groups.push_str(&format!(
                    "<article class=\"card\"><h3>{}</h3><p>{}</p></article>\n<p class=\"meta\">{} 字节</p>\n",
                    html_escape(title),
                    html_escape(content),
                    content.len()
                ));
            }
            groups.push_str("</section>\n");
        }
        let html = format!(
            "<!DOCTYPE html>\n<html lang=\"zh-CN\">\n<head>\n<meta charset=\"utf-8\">\n\
             <title>订阅提醒企业微信分页预览</title>\n<style>\n\
             body {{ margin: 0; background: #ededed; color: #111;\
             font-family: \"WenQuanYi Micro Hei\", \"PingFang SC\", \"Noto Sans CJK SC\", \"Microsoft YaHei\", sans-serif; }}\n\
             section {{ padding: 20px 16px 8px; }}\n\
             h2 {{ margin: 0 0 12px; font-size: 15px; font-weight: 600; }}\n\
             .card {{ width: 360px; box-sizing: border-box; background: #fff; border-radius: 8px;\
             padding: 14px 16px 16px; margin: 0 0 6px;\
             box-shadow: 0 1px 3px rgba(0, 0, 0, 0.08); }}\n\
             .card h3 {{ margin: 0 0 8px; font-size: 17px; line-height: 1.4; font-weight: 700; color: #111; }}\n\
             .card p {{ margin: 0; font-size: 14px; line-height: 1.55; color: #888; white-space: pre-wrap; word-break: break-word; }}\n\
             .meta {{ margin: 0 0 16px; font-size: 12px; color: #999; }}\n\
             </style>\n</head>\n<body>\n{groups}</body>\n</html>\n"
        );
        let dir = Path::new("/opt/cursor/artifacts");
        fs::create_dir_all(dir).unwrap();
        fs::write(dir.join("wecom-preview.md"), markdown).unwrap();
        fs::write(dir.join("wecom-preview.html"), html).unwrap();
    }

    #[test]
    fn titles_carry_a_page_suffix_only_when_split() {
        let today = NaiveDate::from_ymd_opt(2026, 11, 10).unwrap();
        assert_eq!(reminder_title(today, 0, 1), "📺 今日追更 · 11月10日 周二");
        assert_eq!(
            reminder_title(today, 0, 2),
            "📺 今日追更 · 11月10日 周二 · 1/2"
        );
        assert_eq!(
            reminder_title(today, 1, 2),
            "📺 今日追更 · 11月10日 周二 · 2/2"
        );
    }

    #[test]
    fn limits_history_records() {
        let mut history = History::default();
        for index in 0..105 {
            record_run(
                &mut history,
                "2026-07-27",
                &format!("第 {index} 次"),
                1,
                1,
                "manual",
            );
        }
        assert_eq!(history.items.len(), MAX_HISTORY_ITEMS);
        assert_eq!(history.summary.runs, 105);
        assert_eq!(history.summary.notifications, 105);
    }
}
