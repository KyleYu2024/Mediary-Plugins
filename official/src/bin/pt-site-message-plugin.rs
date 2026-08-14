use chrono::Local;
use reqwest::Client;
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use serde_json::json;
use std::{
    collections::{HashSet, VecDeque},
    env, fs,
    path::{Path, PathBuf},
    time::Duration,
};

const MAX_HISTORY_ITEMS: usize = 2000;
const NOTIFICATION_IMAGE_URL: &str = "https://raw.githubusercontent.com/KyleYu2024/Mediary-Plugins/main/official/pt-site-message/assets/notification.png";

#[derive(Default, Deserialize)]
struct Settings {
    #[serde(default)]
    site_ids: String,
    #[serde(default)]
    title_filters: String,
}

struct PluginContext {
    api_url: String,
    token: String,
    data_dir: PathBuf,
    trigger: String,
    settings: Settings,
    client: Client,
}

#[derive(Default, Deserialize, Serialize)]
struct History {
    #[serde(default)]
    updated_at: String,
    #[serde(default)]
    summary: HistorySummary,
    #[serde(default)]
    processed_keys: VecDeque<String>,
    #[serde(default)]
    items: VecDeque<HistoryRecord>,
}

#[derive(Default, Deserialize, Serialize)]
struct HistorySummary {
    #[serde(default)]
    runs: usize,
    #[serde(default)]
    notified: usize,
    #[serde(default)]
    filtered: usize,
    #[serde(default)]
    last_result: String,
    #[serde(default)]
    last_run_at: String,
}

#[derive(Deserialize)]
struct SiteMessagesResponse {
    #[serde(default)]
    items: Vec<SiteMessageBatch>,
}

#[derive(Deserialize)]
struct SiteMessageBatch {
    site_id: i64,
    site_name: String,
    #[serde(default)]
    messages: Vec<SiteMessage>,
    error: Option<String>,
}

#[derive(Deserialize)]
struct SiteMessage {
    key: String,
    title: String,
    date: String,
    content: String,
    url: String,
}

#[derive(Deserialize, Serialize)]
struct HistoryRecord {
    key: String,
    site_name: String,
    title: String,
    status: String,
    message_date: String,
    processed_at: String,
}

#[derive(Default)]
struct RunReport {
    site_count: usize,
    new_messages: usize,
    notified: usize,
    filtered: usize,
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
    if action != "check" {
        return Err(format!("不支持的 PT 站内信动作: {action}"));
    }
    let context = PluginContext::from_env()?;
    let history_path = context.data_dir.join("history.json");
    let mut history = load_json::<History>(&history_path);
    let report = check_messages(&context, &mut history).await?;
    finish_history(&mut history, &context.trigger, &report);
    write_json(&history_path, &history)?;

    let notice = format!(
        "检查 {} 个站点，发现 {} 条新站内信，通知 {} 条，过滤 {} 条{}",
        report.site_count,
        report.new_messages,
        report.notified,
        report.filtered,
        if report.failures.is_empty() {
            String::new()
        } else {
            format!("，{} 个站点读取失败", report.failures.len())
        }
    );
    println!(
        "{}",
        json!({
            "notice": notice,
            "report": {
                "site_count": report.site_count,
                "new_messages": report.new_messages,
                "notified": report.notified,
                "filtered": report.filtered,
                "failures": report.failures,
            }
        })
    );
    Ok(())
}

impl PluginContext {
    fn from_env() -> Result<Self, String> {
        let settings = env::var("MEDIARY_PLUGIN_SETTINGS_JSON")
            .ok()
            .and_then(|value| serde_json::from_str::<Settings>(&value).ok())
            .unwrap_or_default();
        validate_site_ids(&settings.site_ids)?;
        let data_dir = env::var("MEDIARY_PLUGIN_DATA_DIR")
            .map(PathBuf::from)
            .map_err(|_| "缺少 MEDIARY_PLUGIN_DATA_DIR".to_string())?;
        fs::create_dir_all(&data_dir).map_err(|error| format!("创建插件数据目录失败: {error}"))?;
        let client = Client::builder()
            .timeout(Duration::from_secs(120))
            .build()
            .map_err(|error| format!("创建 HTTP 客户端失败: {error}"))?;
        Ok(Self {
            api_url: env::var("MEDIARY_PLUGIN_API_URL").unwrap_or_default(),
            token: env::var("MEDIARY_PLUGIN_TOKEN").unwrap_or_default(),
            data_dir,
            trigger: env::var("MEDIARY_PLUGIN_TRIGGER").unwrap_or_else(|_| "manual".to_string()),
            settings,
            client,
        })
    }
}

async fn check_messages(
    context: &PluginContext,
    history: &mut History,
) -> Result<RunReport, String> {
    if context.api_url.trim().is_empty() || context.token.trim().is_empty() {
        return Err("缺少 Mediary 插件 API 环境".to_string());
    }
    let mut request = context
        .client
        .get(format!("{}/plugin/site-messages", context.api_url))
        .bearer_auth(&context.token);
    let site_ids = context.settings.site_ids.trim();
    if !site_ids.is_empty() {
        request = request.query(&[("site_ids", site_ids)]);
    }
    let response = request
        .send()
        .await
        .map_err(|error| format!("读取 PT 站内信失败: {error}"))?;
    if !response.status().is_success() {
        return Err(format!("Mediary 站内信接口返回 HTTP {}", response.status()));
    }
    let payload = response
        .json::<SiteMessagesResponse>()
        .await
        .map_err(|error| format!("解析 PT 站内信响应失败: {error}"))?;
    let filters = parse_title_filters(&context.settings.title_filters);
    let mut processed = history
        .processed_keys
        .iter()
        .cloned()
        .collect::<HashSet<_>>();
    let mut report = RunReport {
        site_count: payload.items.len(),
        ..Default::default()
    };

    for batch in payload.items {
        if let Some(error) = batch.error.filter(|value| !value.trim().is_empty()) {
            report
                .failures
                .push(format!("{}：{}", batch.site_name, error));
        }
        for message in batch.messages {
            let key = format!("{}:{}", batch.site_id, message.key);
            if processed.contains(&key) {
                continue;
            }
            report.new_messages += 1;
            if title_is_filtered(&message.title, &filters) {
                report.filtered += 1;
                remember_processed(history, &mut processed, key.clone());
                push_record(history, key, &batch.site_name, &message, "已过滤");
                continue;
            }
            let title = truncate_chars(
                &format!("📨 {} 新站内信：{}", batch.site_name, message.title),
                120,
            );
            let mut lines = vec![format!("站点: {}", batch.site_name)];
            if !message.date.trim().is_empty() {
                lines.push(format!("时间: {}", message.date));
            }
            lines.push(format!("标题: {}", message.title));
            let content = truncate_chars(
                &format!(
                    "{}\n\n内容:\n{}\n\n查看站内信: {}",
                    lines.join("\n"),
                    message.content,
                    message.url
                ),
                4000,
            );
            match send_notification(context, &title, &content).await {
                Ok(()) => {
                    report.notified += 1;
                    remember_processed(history, &mut processed, key.clone());
                    push_record(history, key, &batch.site_name, &message, "已通知");
                }
                Err(error) => {
                    report
                        .failures
                        .push(format!("{}：通知失败：{}", batch.site_name, error));
                    push_record(
                        history,
                        key,
                        &batch.site_name,
                        &message,
                        "通知失败，等待重试",
                    );
                }
            }
        }
    }
    Ok(report)
}

async fn send_notification(
    context: &PluginContext,
    title: &str,
    content: &str,
) -> Result<(), String> {
    let response = context
        .client
        .post(format!("{}/plugin/notifications", context.api_url))
        .bearer_auth(&context.token)
        .json(&json!({
            "title": title,
            "content": content,
            "image_url": NOTIFICATION_IMAGE_URL,
        }))
        .send()
        .await
        .map_err(|error| format!("请求通知接口失败: {error}"))?;
    if response.status().is_success() {
        Ok(())
    } else {
        Err(format!("通知接口返回 HTTP {}", response.status()))
    }
}

fn validate_site_ids(value: &str) -> Result<(), String> {
    for item in value
        .split(',')
        .map(str::trim)
        .filter(|item| !item.is_empty())
    {
        if item.parse::<i64>().ok().is_none_or(|id| id <= 0) {
            return Err("站点 ID 必须是英文逗号分隔的正整数".to_string());
        }
    }
    Ok(())
}

fn parse_title_filters(value: &str) -> Vec<String> {
    value
        .split(['|', ',', '，'])
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_lowercase)
        .collect()
}

fn title_is_filtered(title: &str, filters: &[String]) -> bool {
    let title = title.to_lowercase();
    filters.iter().any(|filter| title.contains(filter))
}

fn truncate_chars(value: &str, limit: usize) -> String {
    if value.chars().count() <= limit {
        return value.to_string();
    }
    let mut truncated = value
        .chars()
        .take(limit.saturating_sub(1))
        .collect::<String>();
    truncated.push('…');
    truncated
}

fn remember_processed(history: &mut History, processed: &mut HashSet<String>, key: String) {
    if processed.insert(key.clone()) {
        history.processed_keys.push_front(key);
        history.processed_keys.truncate(MAX_HISTORY_ITEMS);
    }
}

fn push_record(
    history: &mut History,
    key: String,
    site_name: &str,
    message: &SiteMessage,
    status: &str,
) {
    history.items.push_front(HistoryRecord {
        key,
        site_name: site_name.to_string(),
        title: message.title.clone(),
        status: status.to_string(),
        message_date: message.date.clone(),
        processed_at: Local::now().to_rfc3339(),
    });
    history.items.truncate(MAX_HISTORY_ITEMS);
}

fn finish_history(history: &mut History, trigger: &str, report: &RunReport) {
    let finished_at = Local::now().to_rfc3339();
    history.updated_at = finished_at.clone();
    history.summary.runs += 1;
    history.summary.notified += report.notified;
    history.summary.filtered += report.filtered;
    history.summary.last_result = format!(
        "{}触发：{} 个站点，通知 {} 条，过滤 {} 条，失败 {} 项",
        if trigger == "schedule" {
            "计划"
        } else {
            "手动"
        },
        report.site_count,
        report.notified,
        report.filtered,
        report.failures.len()
    );
    history.summary.last_run_at = finished_at;
}

fn load_json<T: DeserializeOwned + Default>(path: &Path) -> T {
    fs::read_to_string(path)
        .ok()
        .and_then(|value| serde_json::from_str(&value).ok())
        .unwrap_or_default()
}

fn write_json<T: Serialize>(path: &Path, value: &T) -> Result<(), String> {
    let parent = path
        .parent()
        .ok_or_else(|| "插件数据路径无父目录".to_string())?;
    fs::create_dir_all(parent).map_err(|error| format!("创建数据目录失败: {error}"))?;
    let temporary = parent.join("history.json.tmp");
    let data =
        serde_json::to_vec_pretty(value).map_err(|error| format!("序列化历史失败: {error}"))?;
    fs::write(&temporary, data).map_err(|error| format!("写入临时历史失败: {error}"))?;
    fs::rename(&temporary, path).map_err(|error| format!("保存历史失败: {error}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::{
        Json, Router,
        extract::State,
        routing::{get, post},
    };
    use serde_json::Value;
    use std::sync::Arc;
    use tokio::sync::Mutex;

    #[derive(Clone, Default)]
    struct MockState {
        notifications: Arc<Mutex<Vec<Value>>>,
    }

    async fn mock_messages() -> Json<Value> {
        Json(json!({
            "items": [{
                "site_id": 7,
                "site_name": "测试站",
                "messages": [
                    {"key":"a","title":"邀请码申请结果","date":"2026-08-14","content":"申请已通过","url":"https://pt.test/a"},
                    {"key":"b","title":"今日促销活动","date":"2026-08-14","content":"促销正文","url":"https://pt.test/b"}
                ],
                "error": null
            }]
        }))
    }

    async fn mock_notification(State(state): State<MockState>, Json(value): Json<Value>) {
        state.notifications.lock().await.push(value);
    }

    #[tokio::test]
    async fn filters_keywords_notifies_content_and_deduplicates() {
        let state = MockState::default();
        let app = Router::new()
            .route("/api/plugin/site-messages", get(mock_messages))
            .route("/api/plugin/notifications", post(mock_notification))
            .with_state(state.clone());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let data_dir =
            std::env::temp_dir().join(format!("pt-site-message-test-{}", std::process::id()));
        let context = PluginContext {
            api_url: format!("http://{address}/api"),
            token: "test-token".to_string(),
            data_dir: data_dir.clone(),
            trigger: "manual".to_string(),
            settings: Settings {
                site_ids: "7".to_string(),
                title_filters: "促销|公告，签到".to_string(),
            },
            client: Client::new(),
        };
        let mut history = History::default();
        let first = check_messages(&context, &mut history).await.unwrap();
        assert_eq!(first.new_messages, 2);
        assert_eq!(first.notified, 1);
        assert_eq!(first.filtered, 1);
        let second = check_messages(&context, &mut history).await.unwrap();
        assert_eq!(second.new_messages, 0);
        let notifications = state.notifications.lock().await;
        assert_eq!(notifications.len(), 1);
        assert!(
            notifications[0]["content"]
                .as_str()
                .unwrap()
                .contains("申请已通过")
        );
        assert!(
            notifications[0]["title"]
                .as_str()
                .unwrap()
                .contains("邀请码申请结果")
        );
        assert_eq!(notifications[0]["image_url"], NOTIFICATION_IMAGE_URL);
        drop(notifications);
        let _ = fs::remove_dir_all(data_dir);
    }

    #[test]
    fn title_filters_support_all_requested_or_separators() {
        let filters = parse_title_filters("签到|促销, Announcement，管理组通知");
        assert_eq!(filters.len(), 4);
        assert!(title_is_filtered("New ANNOUNCEMENT", &filters));
        assert!(!title_is_filtered("邀请码申请结果", &filters));
    }

    #[test]
    fn notification_image_is_the_project_asset() {
        assert!(
            NOTIFICATION_IMAGE_URL.ends_with("/official/pt-site-message/assets/notification.png")
        );
        assert!(Path::new("pt-site-message/assets/notification.png").is_file());
    }
}
