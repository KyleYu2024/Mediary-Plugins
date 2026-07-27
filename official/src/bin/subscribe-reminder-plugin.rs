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

#[derive(Clone, Deserialize)]
struct Settings {
    #[serde(default = "default_true")]
    show_progress: bool,
    #[serde(default)]
    notify_when_empty: bool,
    #[serde(default = "default_max_per_message")]
    max_per_message: usize,
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
    let batch_size = context.settings.max_per_message.clamp(1, 12);
    let total_batches = reminders.len().div_ceil(batch_size);
    for (index, batch) in reminders.chunks(batch_size).enumerate() {
        let suffix = (total_batches > 1).then(|| format!(" · {}/{}", index + 1, total_batches));
        let title = format!(
            "📺 今日追更 · {}{}",
            friendly_date(today),
            suffix.unwrap_or_default()
        );
        let content = format_batch(batch, context.settings.show_progress);
        let image_url = batch.iter().find_map(reminder_image_url);
        send_notification(context, &title, &content, image_url.as_deref()).await?;
    }
    Ok(total_batches)
}

fn format_batch(reminders: &[ReminderItem], show_progress: bool) -> String {
    let now = Local::now();
    let mut lines = vec![format!("今天有 {} 部订阅剧集更新", reminders.len())];
    for reminder in reminders {
        lines.push(String::new());
        let time = reminder
            .local_air_at
            .map(|value| format!("{:02}:{:02}", value.hour(), value.minute()))
            .unwrap_or_else(|| "今日".to_string());
        let state = reminder
            .local_air_at
            .map(|value| {
                if value <= now {
                    "已更新"
                } else {
                    "待播出"
                }
            })
            .unwrap_or("更新日");
        lines.push(format!("{time}  {state}"));
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
    }
    lines.join("\n")
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
        max_per_message: default_max_per_message(),
    }
}

fn default_true() -> bool {
    true
}

fn default_max_per_message() -> usize {
    8
}

#[cfg(test)]
mod tests {
    use super::*;

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
