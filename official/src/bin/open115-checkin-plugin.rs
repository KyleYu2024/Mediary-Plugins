use chrono::Local;
use reqwest::{Client, header::COOKIE};
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use serde_json::{Value, json};
use sha1::{Digest, Sha1};
use sha2::Sha256;
use std::{
    collections::VecDeque,
    env, fs,
    path::{Path, PathBuf},
    time::{Duration, SystemTime, UNIX_EPOCH},
};

const CHECKIN_URL: &str = "https://proapi.115.com/android/2.0/user/points_sign";
const NOTIFICATION_IMAGE_URL: &str = "https://raw.githubusercontent.com/KyleYu2024/Mediary-Plugins/main/official/115-checkin/assets/notification.jpg";
const ANDROID_APP_VERSION: &str = "37.2.6";
const USER_AGENT: &str = "115disk/37.2.6 (Android 15; mobile)";
const MAX_HISTORY_ITEMS: usize = 100;

#[derive(Default, Deserialize)]
struct Settings {
    #[serde(default)]
    cookie: String,
    #[serde(default = "default_true")]
    notify: bool,
}

struct PluginContext {
    api_url: String,
    token: String,
    data_dir: PathBuf,
    trigger: String,
    settings: Settings,
    client: Client,
}

#[derive(Debug, PartialEq, Eq)]
enum CheckinStatus {
    Success,
    AlreadyDone,
}

#[derive(Debug)]
struct CheckinOutcome {
    status: CheckinStatus,
    message: String,
    points: Option<i64>,
    continuous_day: Option<i64>,
}

#[derive(Default, Deserialize, Serialize)]
struct History {
    #[serde(default)]
    updated_at: String,
    #[serde(default)]
    summary: HistorySummary,
    #[serde(default)]
    items: VecDeque<HistoryRecord>,
}

#[derive(Default, Deserialize, Serialize)]
struct HistorySummary {
    #[serde(default)]
    runs: usize,
    #[serde(default)]
    successes: usize,
    #[serde(default)]
    last_result: String,
    #[serde(default)]
    last_run_at: String,
}

#[derive(Deserialize, Serialize)]
struct HistoryRecord {
    result: String,
    points: Option<i64>,
    continuous_day: Option<i64>,
    trigger: String,
    finished_at: String,
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
    if action != "checkin" {
        return Err(format!("不支持的 115 签到动作: {action}"));
    }

    let context = PluginContext::from_env()?;
    let history_path = context.data_dir.join("history.json");
    let mut history = load_json::<History>(&history_path);

    match perform_checkin(&context).await {
        Ok(outcome) => {
            record_run(
                &mut history,
                &outcome.message,
                outcome.points,
                outcome.continuous_day,
                &context.trigger,
                true,
            );
            write_json(&history_path, &history)?;
            if context.settings.notify
                && let Err(error) = send_notification(&context, &outcome).await
            {
                eprintln!("115 签到通知发送失败: {error}");
            }
            println!(
                "{}",
                json!({
                    "notice": outcome.message,
                    "report": {
                        "checked_in": outcome.status == CheckinStatus::Success,
                        "already_done": outcome.status == CheckinStatus::AlreadyDone,
                        "points": outcome.points,
                        "continuous_day": outcome.continuous_day,
                    }
                })
            );
            Ok(())
        }
        Err(error) => {
            let message = format!("签到失败：{error}");
            record_run(&mut history, &message, None, None, &context.trigger, false);
            write_json(&history_path, &history)?;
            if context.settings.notify
                && let Err(notification_error) = send_failure_notification(&context, &error).await
            {
                eprintln!("115 签到失败通知发送失败: {notification_error}");
            }
            Err(message)
        }
    }
}

impl PluginContext {
    fn from_env() -> Result<Self, String> {
        let settings = env::var("MEDIARY_PLUGIN_SETTINGS_JSON")
            .ok()
            .and_then(|value| serde_json::from_str::<Settings>(&value).ok())
            .unwrap_or_default();
        if settings.cookie.trim().is_empty() {
            return Err("请先在插件设置中填写 115 Cookie".to_string());
        }

        let data_dir = env::var("MEDIARY_PLUGIN_DATA_DIR")
            .map(PathBuf::from)
            .map_err(|_| "缺少 MEDIARY_PLUGIN_DATA_DIR".to_string())?;
        fs::create_dir_all(&data_dir).map_err(|error| format!("创建插件数据目录失败: {error}"))?;
        let client = Client::builder()
            .timeout(Duration::from_secs(20))
            .user_agent(USER_AGENT)
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

async fn perform_checkin(context: &PluginContext) -> Result<CheckinOutcome, String> {
    let cookie = context.settings.cookie.trim();
    let user_id = user_id_from_cookie(cookie)?;
    let token_time = unix_timestamp()?.to_string();
    let token = points_sign_token(&user_id, &token_time);
    let device_id = device_id(&user_id);
    let response = context
        .client
        .post(CHECKIN_URL)
        .form(&[
            ("user_id", user_id.as_str()),
            ("app_ver", ANDROID_APP_VERSION),
            ("token_time", token_time.as_str()),
            ("token", token.as_str()),
            ("device_name", "Mediary Android"),
            ("device_id", device_id.as_str()),
        ])
        .header(COOKIE, cookie)
        .send()
        .await
        .map_err(|error| format!("请求 115 签到接口失败: {error}"))?;
    let status = response.status();
    let body = response
        .text()
        .await
        .map_err(|error| format!("读取 115 签到响应失败: {error}"))?;
    if !status.is_success() {
        return Err(format!("115 签到接口返回 HTTP {status}"));
    }
    let payload = serde_json::from_str::<Value>(&body)
        .map_err(|_| "115 签到接口返回了无法解析的响应".to_string())?;
    parse_checkin_response(&payload)
}

fn parse_checkin_response(payload: &Value) -> Result<CheckinOutcome, String> {
    let state = value_bool(payload.get("state"));
    let code = payload
        .get("code")
        .and_then(value_i64)
        .or_else(|| payload.get("error_code").and_then(value_i64))
        .or_else(|| payload.get("errno").and_then(value_i64));
    let error = payload
        .get("error")
        .and_then(Value::as_str)
        .filter(|message| !message.trim().is_empty())
        .or_else(|| payload.get("message").and_then(Value::as_str))
        .unwrap_or_default()
        .trim();
    let first_require_sign = payload
        .pointer("/data/first_require_sign")
        .and_then(value_i64);
    let points = payload.pointer("/data/points_num").and_then(value_i64);
    let continuous_day = payload.pointer("/data/continuous_day").and_then(value_i64);

    if state == Some(true) {
        let status = if first_require_sign == Some(0) {
            CheckinStatus::AlreadyDone
        } else {
            CheckinStatus::Success
        };
        let message = checkin_message(&status, points, continuous_day);
        return Ok(CheckinOutcome {
            status,
            message,
            points,
            continuous_day,
        });
    }

    if ["已签到", "已经签到", "今日已签"]
        .iter()
        .any(|marker| error.contains(marker))
    {
        return Ok(CheckinOutcome {
            status: CheckinStatus::AlreadyDone,
            message: "115 今日已签到".to_string(),
            points: None,
            continuous_day: None,
        });
    }

    if code == Some(99)
        || ["重新登录", "登录超时", "请先登录"]
            .iter()
            .any(|marker| error.contains(marker))
    {
        return Err("115 Cookie 已失效，请更新插件配置".to_string());
    }
    if error.contains("手机短信验证") {
        return Err("115 要求手机短信验证，请先在 115 安卓客户端完成验证后重试".to_string());
    }
    if error.contains("token") || error.contains("签名") {
        return Err("115 签到 token 算法已失效，请更新插件".to_string());
    }

    let message = if error.is_empty() {
        "115 签到接口未返回成功结果".to_string()
    } else {
        sanitize_error(error)
    };
    Err(message)
}

fn user_id_from_cookie(cookie: &str) -> Result<String, String> {
    let uid = cookie
        .split(';')
        .filter_map(|part| part.trim().split_once('='))
        .find(|(key, _)| key.trim().eq_ignore_ascii_case("UID"))
        .map(|(_, value)| value.trim())
        .unwrap_or_default();
    let user_id = uid
        .chars()
        .take_while(char::is_ascii_digit)
        .collect::<String>();
    if user_id.is_empty() {
        Err("115 Cookie 中缺少有效的 UID".to_string())
    } else {
        Ok(user_id)
    }
}

fn unix_timestamp() -> Result<u64, String> {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_secs())
        .map_err(|_| "本机时间早于 Unix 时间戳起点".to_string())
}

fn points_sign_token(user_id: &str, token_time: &str) -> String {
    let input = format!("{user_id}-Points_Sign@#115-{token_time}");
    format!("{:x}", Sha1::digest(input.as_bytes()))
}

fn device_id(user_id: &str) -> String {
    let digest = format!(
        "{:x}",
        Sha256::digest(format!("mediary-115-{user_id}").as_bytes())
    );
    digest[..32].to_string()
}

fn checkin_message(
    status: &CheckinStatus,
    points: Option<i64>,
    continuous_day: Option<i64>,
) -> String {
    let mut message = match status {
        CheckinStatus::Success => "115 签到成功".to_string(),
        CheckinStatus::AlreadyDone => "115 今日已签到".to_string(),
    };
    if let Some(points) = points
        && *status == CheckinStatus::Success
    {
        message.push_str(&format!("，本次获得 {points} 积分"));
    }
    if let Some(days) = continuous_day {
        message.push_str(&format!("，已连续签到 {days} 天"));
    }
    message
}

fn sanitize_error(error: &str) -> String {
    error
        .chars()
        .filter(|character| !character.is_control())
        .take(300)
        .collect()
}

async fn send_notification(
    context: &PluginContext,
    outcome: &CheckinOutcome,
) -> Result<(), String> {
    send_plugin_notification(
        context,
        &json!({
            "title": match outcome.status {
                CheckinStatus::Success => "115 签到成功",
                CheckinStatus::AlreadyDone => "115 签到结果",
            },
            "content": outcome.message,
            "image_url": NOTIFICATION_IMAGE_URL,
        }),
    )
    .await
}

async fn send_failure_notification(context: &PluginContext, error: &str) -> Result<(), String> {
    send_plugin_notification(
        context,
        &json!({
            "title": "115 签到失败",
            "content": error,
            "image_url": NOTIFICATION_IMAGE_URL,
        }),
    )
    .await
}

async fn send_plugin_notification(
    context: &PluginContext,
    notification: &Value,
) -> Result<(), String> {
    if context.api_url.trim().is_empty() || context.token.trim().is_empty() {
        return Err("缺少 Mediary 插件 API 环境".to_string());
    }
    let response = context
        .client
        .post(format!("{}/plugin/notifications", context.api_url))
        .bearer_auth(&context.token)
        .json(notification)
        .send()
        .await
        .map_err(|error| format!("请求 Mediary 通知接口失败: {error}"))?;
    if response.status().is_success() {
        Ok(())
    } else {
        Err(format!("Mediary 通知接口返回 HTTP {}", response.status()))
    }
}

fn record_run(
    history: &mut History,
    result: &str,
    points: Option<i64>,
    continuous_day: Option<i64>,
    trigger: &str,
    success: bool,
) {
    let finished_at = Local::now().to_rfc3339();
    history.updated_at = finished_at.clone();
    history.summary.runs += 1;
    if success {
        history.summary.successes += 1;
    }
    history.summary.last_result = result.to_string();
    history.summary.last_run_at = finished_at.clone();
    history.items.push_front(HistoryRecord {
        result: result.to_string(),
        points,
        continuous_day,
        trigger: trigger.to_string(),
        finished_at,
    });
    history.items.truncate(MAX_HISTORY_ITEMS);
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
        .ok_or_else(|| "签到记录路径无效".to_string())?;
    fs::create_dir_all(parent).map_err(|error| format!("创建签到记录目录失败: {error}"))?;
    let temporary = path.with_extension("json.tmp");
    let payload =
        serde_json::to_vec_pretty(value).map_err(|error| format!("序列化签到记录失败: {error}"))?;
    fs::write(&temporary, payload).map_err(|error| format!("写入签到记录失败: {error}"))?;
    fs::rename(&temporary, path).map_err(|error| format!("保存签到记录失败: {error}"))
}

fn value_bool(value: Option<&Value>) -> Option<bool> {
    match value {
        Some(Value::Bool(value)) => Some(*value),
        Some(Value::Number(value)) => value.as_i64().map(|value| value != 0),
        Some(Value::String(value)) => match value.trim().to_ascii_lowercase().as_str() {
            "1" | "true" | "ok" => Some(true),
            "0" | "false" => Some(false),
            _ => None,
        },
        _ => None,
    }
}

fn value_i64(value: &Value) -> Option<i64> {
    match value {
        Value::Number(number) => number.as_i64(),
        Value::String(text) => text.trim().parse().ok(),
        _ => None,
    }
}

fn default_true() -> bool {
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extracts_user_id_from_common_cookie_shapes() {
        assert_eq!(
            user_id_from_cookie("CID=abc; UID=123456_A1_1700000000; SEID=secret").unwrap(),
            "123456"
        );
        assert_eq!(
            user_id_from_cookie("uid=987654; cid=abc").unwrap(),
            "987654"
        );
        assert!(user_id_from_cookie("CID=abc; SEID=secret").is_err());
    }

    #[test]
    fn parses_success_response() {
        let outcome = parse_checkin_response(&json!({
            "state": true,
            "code": 0,
            "data": {
                "first_require_sign": 1,
                "points_num": 2,
                "continuous_day": 3
            }
        }))
        .unwrap();
        assert_eq!(outcome.status, CheckinStatus::Success);
        assert_eq!(outcome.points, Some(2));
        assert_eq!(outcome.continuous_day, Some(3));
        assert!(outcome.message.contains("2 积分"));
    }

    #[test]
    fn treats_duplicate_as_successful_result() {
        let outcome = parse_checkin_response(&json!({
            "state": true,
            "code": 0,
            "data": {
                "first_require_sign": 0,
                "points_num": "2",
                "continuous_day": 3
            }
        }))
        .unwrap();
        assert_eq!(outcome.status, CheckinStatus::AlreadyDone);
        assert!(outcome.message.starts_with("115 今日已签到"));
        assert!(outcome.message.contains("连续签到 3 天"));
    }

    #[test]
    fn reports_expired_cookie_without_response_details() {
        let error = parse_checkin_response(&json!({
            "state": false,
            "error": "请重新登录",
            "errno": 99,
            "request": "/android/2.0/user/points_sign?token=secret"
        }))
        .err()
        .unwrap();
        assert!(error.contains("Cookie 已失效"));
        assert!(!error.contains("secret"));
    }

    #[test]
    fn token_matches_android_client_algorithm() {
        assert_eq!(
            points_sign_token("123", "1700000000"),
            "0d6ec633ee8d5d4040be5edbdfa043d709496f36"
        );
    }

    #[test]
    fn reports_mobile_verification_requirement() {
        let error = parse_checkin_response(&json!({
            "state": false,
            "error": "请进行手机短信验证"
        }))
        .unwrap_err();
        assert!(error.contains("安卓客户端"));
    }
}
