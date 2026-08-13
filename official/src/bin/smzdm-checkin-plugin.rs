use chrono::Local;
use md5::{Digest, Md5};
use reqwest::{Client, StatusCode, header::COOKIE};
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use serde_json::{Value, json};
use std::{
    collections::{BTreeMap, VecDeque},
    env, fs,
    path::{Path, PathBuf},
    time::{Duration, SystemTime, UNIX_EPOCH},
};

const CHECKIN_URL: &str = "https://user-api.smzdm.com/checkin";
const TOKEN_URL: &str = "https://user-api.smzdm.com/robot/token";
const IOS_VERSION: &str = "11.1.35";
const IOS_USER_AGENT: &str =
    "smzdm 11.1.35 rv:167 (iPhone 6s; iOS 15.8.3; zh_CN)/iphone_smzdmapp/11.1.35";
const IOS_SIGN_KEY: &str = "zok5JtAq3$QixaA%mncn*jGWlEpSL3E1";
const ANDROID_VERSION: &str = "10.4.1";
const ANDROID_USER_AGENT: &str = "smzdm_android_V10.4.1 rv:841 (22021211RC;Android12;zh)smzdmapp";
const ANDROID_SIGN_KEY: &str = "apr1$AwP!wRRT$gJ/q.X24poeBInlUJC";
const ANDROID_SK: &str = "ierkM0OZZbsuBKLoAgQ6OJneLMXBQXmzX+LXkNTuKch8Ui2jGlahuFyWIzBiDq/L";
const MAX_HISTORY_ITEMS: usize = 200;
const NOTIFICATION_IMAGE_URL: &str = "https://raw.githubusercontent.com/KyleYu2024/Mediary-Plugins/main/official/smzdm-checkin/assets/notification.png";

#[derive(Deserialize)]
struct Settings {
    #[serde(default = "default_true")]
    use_cookiecloud: bool,
    #[serde(default)]
    cookies: String,
    #[serde(default = "default_true")]
    notify: bool,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            use_cookiecloud: true,
            cookies: String::new(),
            notify: true,
        }
    }
}

struct CookieCredential {
    cookie: String,
    source: &'static str,
}

#[derive(Deserialize)]
struct CookieCloudResponse {
    #[serde(default)]
    cookie: String,
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
    protocol: &'static str,
}

#[derive(Debug, PartialEq, Eq)]
enum FailureKind {
    Auth,
    Verification,
    Protocol,
    Temporary,
    Other,
}

#[derive(Debug)]
struct CheckinFailure {
    kind: FailureKind,
    message: String,
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
    account_attempts: usize,
    #[serde(default)]
    successes: usize,
    #[serde(default)]
    last_result: String,
    #[serde(default)]
    last_run_at: String,
}

#[derive(Deserialize, Serialize)]
struct HistoryRecord {
    account: String,
    result: String,
    points: Option<i64>,
    continuous_day: Option<i64>,
    protocol: String,
    #[serde(default)]
    source: String,
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
        return Err(format!("不支持的什么值得买签到动作: {action}"));
    }

    let context = PluginContext::from_env()?;
    let credential = match resolve_cookie(&context).await {
        Ok(credential) => credential,
        Err(error) => {
            if context.settings.notify
                && let Err(notify_error) = send_notification(
                    &context,
                    "什么值得买签到失败",
                    &format!("签到失败：{error}"),
                )
                .await
            {
                eprintln!("什么值得买签到通知发送失败: {notify_error}");
            }
            return Err(error);
        }
    };
    let history_path = context.data_dir.join("history.json");
    let mut history = load_json::<History>(&history_path);
    match perform_checkin(&context, &credential.cookie).await {
        Ok(outcome) => {
            let result = outcome.message.clone();
            record_account(
                &mut history,
                "当前账号",
                &result,
                outcome.points,
                outcome.continuous_day,
                outcome.protocol,
                credential.source,
                &context.trigger,
                true,
            );
            finish_history_run(&mut history, &result);
            write_json(&history_path, &history)?;
            if context.settings.notify
                && let Err(error) = send_notification(
                    &context,
                    notification_title(&outcome.status),
                    &outcome.message,
                )
                .await
            {
                eprintln!("什么值得买签到通知发送失败: {error}");
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
            let result = format!("签到失败：{}", error.message);
            record_account(
                &mut history,
                "当前账号",
                &result,
                None,
                None,
                "-",
                credential.source,
                &context.trigger,
                false,
            );
            finish_history_run(&mut history, &result);
            write_json(&history_path, &history)?;
            if context.settings.notify
                && let Err(notification_error) =
                    send_notification(&context, "什么值得买签到失败", &result).await
            {
                eprintln!("什么值得买签到通知发送失败: {notification_error}");
            }
            Err(result)
        }
    }
}

impl PluginContext {
    fn from_env() -> Result<Self, String> {
        let settings = env::var("MEDIARY_PLUGIN_SETTINGS_JSON")
            .ok()
            .and_then(|value| serde_json::from_str::<Settings>(&value).ok())
            .unwrap_or_default();
        if !settings.use_cookiecloud && settings.cookies.trim().is_empty() {
            return Err("请启用 CookieCloud，或填写手工 Cookie".to_string());
        }

        let data_dir = env::var("MEDIARY_PLUGIN_DATA_DIR")
            .map(PathBuf::from)
            .map_err(|_| "缺少 MEDIARY_PLUGIN_DATA_DIR".to_string())?;
        fs::create_dir_all(&data_dir).map_err(|error| format!("创建插件数据目录失败: {error}"))?;
        let client = Client::builder()
            .timeout(Duration::from_secs(20))
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

async fn resolve_cookie(context: &PluginContext) -> Result<CookieCredential, String> {
    let manual = if context.settings.cookies.trim().is_empty() {
        None
    } else {
        Some(CookieCredential {
            cookie: parse_manual_cookie(&context.settings.cookies)?.to_string(),
            source: "手工配置",
        })
    };

    if !context.settings.use_cookiecloud {
        return manual.ok_or_else(|| "请先在插件设置中填写什么值得买 Cookie".to_string());
    }

    match fetch_cookiecloud_cookie(context).await {
        Ok(cookie) => Ok(CookieCredential {
            cookie,
            source: "CookieCloud",
        }),
        Err(error) if manual.is_some() => {
            eprintln!("CookieCloud Cookie 不可用，已使用手工配置兜底: {error}");
            Ok(manual.expect("已确认存在手工 Cookie"))
        }
        Err(error) => Err(format!("CookieCloud Cookie 不可用：{error}")),
    }
}

async fn fetch_cookiecloud_cookie(context: &PluginContext) -> Result<String, String> {
    if context.api_url.trim().is_empty() || context.token.trim().is_empty() {
        return Err("缺少 Mediary 插件 API 环境".to_string());
    }
    let response = context
        .client
        .get(format!(
            "{}/plugin/cookiecloud?domain=user-api.smzdm.com",
            context.api_url.trim_end_matches('/')
        ))
        .bearer_auth(&context.token)
        .send()
        .await
        .map_err(|error| {
            format!(
                "请求 Mediary CookieCloud 接口失败: {}",
                sanitize_error(&error.to_string())
            )
        })?;
    let status = response.status();
    if !status.is_success() {
        return Err(format!("Mediary CookieCloud 接口返回 HTTP {status}"));
    }
    let payload = response
        .json::<CookieCloudResponse>()
        .await
        .map_err(|_| "Mediary CookieCloud 接口返回了无法解析的响应".to_string())?;
    let cookie = payload.cookie.trim().to_string();
    if cookie.is_empty() {
        Err("CookieCloud 中没有可供什么值得买签到接口使用的 Cookie".to_string())
    } else {
        Ok(cookie)
    }
}

async fn perform_checkin(
    context: &PluginContext,
    cookie: &str,
) -> Result<CheckinOutcome, CheckinFailure> {
    match android_checkin(context, cookie).await {
        Ok(outcome) => Ok(outcome),
        Err(error) if error.kind == FailureKind::Protocol => ios_checkin(context, cookie).await,
        Err(error) => Err(error),
    }
}

async fn ios_checkin(
    context: &PluginContext,
    cookie: &str,
) -> Result<CheckinOutcome, CheckinFailure> {
    let mut form = BTreeMap::from([
        ("basic_v", "0".to_string()),
        ("f", "iphone".to_string()),
        ("time", unix_millis()?.to_string()),
        ("v", IOS_VERSION.to_string()),
        ("weixin", "1".to_string()),
        ("zhuanzai_ab", "b".to_string()),
    ]);
    form.insert("sign", sign_form(&form, IOS_SIGN_KEY));
    let payload = post_form(
        context,
        CHECKIN_URL,
        cookie,
        IOS_USER_AGENT,
        &form,
        Some(request_key()?),
    )
    .await?;
    parse_checkin_response(&payload, "iOS")
}

async fn android_checkin(
    context: &PluginContext,
    cookie: &str,
) -> Result<CheckinOutcome, CheckinFailure> {
    let mut token_form = BTreeMap::from([
        ("f", "android".to_string()),
        ("time", unix_millis()?.to_string()),
        ("v", ANDROID_VERSION.to_string()),
        ("weixin", "1".to_string()),
    ]);
    token_form.insert("sign", sign_form(&token_form, ANDROID_SIGN_KEY));
    let token_payload = post_form(
        context,
        TOKEN_URL,
        cookie,
        ANDROID_USER_AGENT,
        &token_form,
        None,
    )
    .await?;
    ensure_api_success(&token_payload)?;
    let token = token_payload
        .pointer("/data/token")
        .and_then(Value::as_str)
        .filter(|value| !value.trim().is_empty())
        .ok_or_else(|| failure(FailureKind::Protocol, "Android token 协议返回格式已变化"))?;

    let mut form = BTreeMap::from([
        ("f", "android".to_string()),
        ("sk", ANDROID_SK.to_string()),
        ("time", unix_millis()?.to_string()),
        ("token", token.to_string()),
        ("v", ANDROID_VERSION.to_string()),
        ("weixin", "1".to_string()),
    ]);
    form.insert("sign", sign_form(&form, ANDROID_SIGN_KEY));
    let payload = post_form(
        context,
        CHECKIN_URL,
        cookie,
        ANDROID_USER_AGENT,
        &form,
        None,
    )
    .await?;
    parse_checkin_response(&payload, "Android")
}

async fn post_form(
    context: &PluginContext,
    url: &str,
    cookie: &str,
    user_agent: &str,
    form: &BTreeMap<&str, String>,
    request_key: Option<String>,
) -> Result<Value, CheckinFailure> {
    for attempt in 0..2 {
        let mut request = context
            .client
            .post(url)
            .header(COOKIE, cookie)
            .header("User-Agent", user_agent)
            .header("Accept", "application/json")
            .header("Accept-Language", "zh-CN,zh;q=0.9")
            .form(form);
        if let Some(value) = &request_key {
            request = request.header("request_key", value);
        }

        match request.send().await {
            Ok(response) => {
                let status = response.status();
                if status.is_server_error() && attempt == 0 {
                    continue;
                }
                if !status.is_success() {
                    let kind = match status {
                        StatusCode::UNAUTHORIZED => FailureKind::Auth,
                        StatusCode::FORBIDDEN | StatusCode::TOO_MANY_REQUESTS => {
                            FailureKind::Verification
                        }
                        status if status.is_server_error() => FailureKind::Temporary,
                        _ => FailureKind::Other,
                    };
                    return Err(failure(kind, format!("接口返回 HTTP {status}")));
                }
                return response
                    .json::<Value>()
                    .await
                    .map_err(|_| failure(FailureKind::Protocol, "接口返回了无法解析的响应"));
            }
            Err(error) if attempt == 0 && (error.is_timeout() || error.is_connect()) => continue,
            Err(error) => {
                return Err(failure(
                    FailureKind::Temporary,
                    format!("请求接口失败: {}", sanitize_error(&error.to_string())),
                ));
            }
        }
    }
    Err(failure(FailureKind::Temporary, "请求接口失败"))
}

fn parse_checkin_response(
    payload: &Value,
    protocol: &'static str,
) -> Result<CheckinOutcome, CheckinFailure> {
    let code = api_code(payload);
    let api_message = api_message(payload);
    if code != Some(0) {
        if is_already_done(&api_message) {
            return Ok(CheckinOutcome {
                status: CheckinStatus::AlreadyDone,
                message: "今日已签到".to_string(),
                points: None,
                continuous_day: None,
                protocol,
            });
        }
        return Err(classify_api_failure(code, &api_message));
    }

    let data = payload.get("data").unwrap_or(&Value::Null);
    let points = data.get("cpadd").and_then(value_i64);
    let continuous_day = data.get("daily_num").and_then(value_i64);
    let status = if is_already_done(&api_message) {
        CheckinStatus::AlreadyDone
    } else {
        CheckinStatus::Success
    };
    let mut message = match status {
        CheckinStatus::Success => "签到成功".to_string(),
        CheckinStatus::AlreadyDone => "今日已签到".to_string(),
    };
    if let Some(points) = points
        && status == CheckinStatus::Success
    {
        message.push_str(&format!("，本次获得 {points} 积分"));
    }
    if let Some(days) = continuous_day {
        message.push_str(&format!("，已连续签到 {days} 天"));
    }
    Ok(CheckinOutcome {
        status,
        message,
        points,
        continuous_day,
        protocol,
    })
}

fn ensure_api_success(payload: &Value) -> Result<(), CheckinFailure> {
    let code = api_code(payload);
    if code == Some(0) {
        Ok(())
    } else {
        Err(classify_api_failure(code, &api_message(payload)))
    }
}

fn classify_api_failure(code: Option<i64>, message: &str) -> CheckinFailure {
    let normalized = message.to_ascii_lowercase();
    let contains_any = |markers: &[&str]| markers.iter().any(|marker| normalized.contains(marker));
    let kind = if code == Some(11111) {
        FailureKind::Temporary
    } else if contains_any(&[
        "未登录",
        "请登录",
        "重新登录",
        "登录失效",
        "登录已失效",
        "登录过期",
        "cookie",
        "sess",
    ]) {
        FailureKind::Auth
    } else if contains_any(&[
        "验证码",
        "安全验证",
        "访问异常",
        "账号异常",
        "操作频繁",
        "请求频繁",
        "风控",
        "captcha",
        "risk",
    ]) {
        FailureKind::Verification
    } else if contains_any(&[
        "签名",
        "sign",
        "token",
        "参数错误",
        "参数异常",
        "invalid param",
        "版本过低",
        "版本升级",
    ]) {
        FailureKind::Protocol
    } else {
        FailureKind::Other
    };
    let code_suffix = code
        .map(|code| format!("，错误码 {code}"))
        .unwrap_or_default();
    let message = match kind {
        FailureKind::Auth => format!("Cookie 已失效，请更新插件配置{code_suffix}"),
        FailureKind::Verification => {
            format!("账号需要在什么值得买 App 完成验证{code_suffix}")
        }
        FailureKind::Protocol => format!("签到协议可能已变化，请更新插件{code_suffix}"),
        FailureKind::Temporary => format!("什么值得买服务暂时不可用，请稍后重试{code_suffix}"),
        FailureKind::Other => match code {
            Some(code) => format!("接口拒绝签到，错误码 {code}"),
            None => "接口未返回成功状态".to_string(),
        },
    };
    failure(kind, message)
}

fn api_code(payload: &Value) -> Option<i64> {
    payload
        .get("error_code")
        .and_then(value_i64)
        .or_else(|| payload.get("code").and_then(value_i64))
}

fn api_message(payload: &Value) -> String {
    ["error_msg", "message", "error"]
        .iter()
        .find_map(|key| payload.get(key).and_then(Value::as_str))
        .unwrap_or_default()
        .trim()
        .to_string()
}

fn is_already_done(message: &str) -> bool {
    ["已签到", "已经签到", "重复签到", "今日已签"]
        .iter()
        .any(|marker| message.contains(marker))
}

fn parse_manual_cookie(raw: &str) -> Result<&str, String> {
    let mut cookies = raw.lines().map(str::trim).filter(|line| !line.is_empty());
    let cookie = cookies
        .next()
        .ok_or_else(|| "请先在插件设置中填写什么值得买 Cookie".to_string())?;
    if cookies.next().is_some() {
        Err("什么值得买签到插件只支持一个账号，请仅保留一个 Cookie".to_string())
    } else {
        Ok(cookie)
    }
}

fn notification_title(status: &CheckinStatus) -> &'static str {
    match status {
        CheckinStatus::Success => "什么值得买签到成功",
        CheckinStatus::AlreadyDone => "什么值得买签到结果",
    }
}

fn sign_form(form: &BTreeMap<&str, String>, key: &str) -> String {
    let mut input = form
        .iter()
        .filter(|(_, value)| !value.is_empty())
        .map(|(name, value)| format!("{name}={value}"))
        .collect::<Vec<_>>()
        .join("&");
    input.push_str("&key=");
    input.push_str(key);
    format!("{:X}", Md5::digest(input.as_bytes()))
}

fn unix_millis() -> Result<u128, CheckinFailure> {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_millis())
        .map_err(|_| failure(FailureKind::Other, "本机时间早于 Unix 时间戳起点"))
}

fn request_key() -> Result<String, CheckinFailure> {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_nanos().to_string().chars().take(18).collect())
        .map_err(|_| failure(FailureKind::Other, "本机时间早于 Unix 时间戳起点"))
}

async fn send_notification(
    context: &PluginContext,
    title: &str,
    content: &str,
) -> Result<(), String> {
    if context.api_url.trim().is_empty() || context.token.trim().is_empty() {
        return Err("缺少 Mediary 插件 API 环境".to_string());
    }
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
        .map_err(|error| format!("请求 Mediary 通知接口失败: {error}"))?;
    if response.status().is_success() {
        Ok(())
    } else {
        Err(format!("Mediary 通知接口返回 HTTP {}", response.status()))
    }
}

#[allow(clippy::too_many_arguments)]
fn record_account(
    history: &mut History,
    account: &str,
    result: &str,
    points: Option<i64>,
    continuous_day: Option<i64>,
    protocol: &str,
    source: &str,
    trigger: &str,
    success: bool,
) {
    let finished_at = Local::now().to_rfc3339();
    history.summary.account_attempts += 1;
    if success {
        history.summary.successes += 1;
    }
    history.items.push_front(HistoryRecord {
        account: account.to_string(),
        result: result.to_string(),
        points,
        continuous_day,
        protocol: protocol.to_string(),
        source: source.to_string(),
        trigger: trigger.to_string(),
        finished_at,
    });
    history.items.truncate(MAX_HISTORY_ITEMS);
}

fn finish_history_run(history: &mut History, result: &str) {
    history.summary.runs += 1;
    let now = Local::now().to_rfc3339();
    history.updated_at = now.clone();
    history.summary.last_result = result.to_string();
    history.summary.last_run_at = now;
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

fn value_i64(value: &Value) -> Option<i64> {
    match value {
        Value::Number(number) => number.as_i64(),
        Value::String(text) => text.trim().parse().ok(),
        _ => None,
    }
}

fn sanitize_error(error: &str) -> String {
    error
        .chars()
        .filter(|character| !character.is_control())
        .take(200)
        .collect()
}

fn failure(kind: FailureKind, message: impl Into<String>) -> CheckinFailure {
    CheckinFailure {
        kind,
        message: message.into(),
    }
}

fn default_true() -> bool {
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ios_signature_matches_reference_algorithm() {
        let form = BTreeMap::from([
            ("basic_v", "0".to_string()),
            ("f", "iphone".to_string()),
            ("time", "1700000000000".to_string()),
            ("v", "11.1.35".to_string()),
            ("weixin", "1".to_string()),
            ("zhuanzai_ab", "b".to_string()),
        ]);
        assert_eq!(
            sign_form(&form, IOS_SIGN_KEY),
            "86452DB19C4BB57C02CE4F0B21F9B60E"
        );
    }

    #[test]
    fn android_signatures_match_reference_algorithm() {
        let token_form = BTreeMap::from([
            ("f", "android".to_string()),
            ("time", "1700000000000".to_string()),
            ("v", "10.4.1".to_string()),
            ("weixin", "1".to_string()),
        ]);
        assert_eq!(
            sign_form(&token_form, ANDROID_SIGN_KEY),
            "A3A7456F0AAF4A12C6B2A906B952A3F5"
        );

        let checkin_form = BTreeMap::from([
            ("f", "android".to_string()),
            ("sk", ANDROID_SK.to_string()),
            ("time", "1700000000000".to_string()),
            ("token", "test-token".to_string()),
            ("v", "10.4.1".to_string()),
            ("weixin", "1".to_string()),
        ]);
        assert_eq!(
            sign_form(&checkin_form, ANDROID_SIGN_KEY),
            "394AF5D9BD56CB097FA9031DBA716651"
        );
    }

    #[test]
    fn manifest_declares_secret_and_minimum_scope() {
        let manifest: Value =
            serde_json::from_str(include_str!("../../smzdm-checkin/plugin.json")).unwrap();
        assert_eq!(manifest["id"], "smzdm-checkin");
        assert_eq!(manifest["action_runtime"]["entrypoint"], "./plugin");
        assert_eq!(manifest["secret_fields"], json!(["cookies"]));
        assert_eq!(
            manifest["requested_scopes"],
            json!(["cookiecloud:read", "notifications:send"])
        );
        assert_eq!(manifest["cookiecloud_domains"], json!(["smzdm.com"]));
    }

    #[test]
    fn notification_image_is_the_project_asset() {
        assert!(
            NOTIFICATION_IMAGE_URL.ends_with("/official/smzdm-checkin/assets/notification.png")
        );
        assert!(Path::new("smzdm-checkin/assets/notification.png").is_file());
    }

    #[test]
    fn parses_success_and_reward_fields() {
        let outcome = parse_checkin_response(
            &json!({
                "error_code": "0",
                "error_msg": "签到成功",
                "data": { "cpadd": "2", "daily_num": 18 }
            }),
            "iOS",
        )
        .unwrap();
        assert_eq!(outcome.status, CheckinStatus::Success);
        assert_eq!(outcome.points, Some(2));
        assert_eq!(outcome.continuous_day, Some(18));
        assert_eq!(outcome.protocol, "iOS");
    }

    #[test]
    fn duplicate_checkin_is_idempotent_success() {
        let outcome = parse_checkin_response(
            &json!({ "error_code": 1, "error_msg": "今日已签到" }),
            "Android",
        )
        .unwrap();
        assert_eq!(outcome.status, CheckinStatus::AlreadyDone);
        assert_eq!(outcome.protocol, "Android");
    }

    #[test]
    fn distinguishes_auth_verification_and_protocol_failures() {
        assert_eq!(
            classify_api_failure(Some(1), "登录已失效").kind,
            FailureKind::Auth
        );
        assert_eq!(
            classify_api_failure(Some(1), "请完成安全验证").kind,
            FailureKind::Verification
        );
        assert_eq!(
            classify_api_failure(Some(1), "sign error").kind,
            FailureKind::Protocol
        );
    }

    #[test]
    fn accepts_one_manual_account_without_splitting_cookie_fields() {
        assert_eq!(parse_manual_cookie("a=1; b=2\n").unwrap(), "a=1; b=2");
    }

    #[test]
    fn rejects_multiple_manual_accounts() {
        let error = parse_manual_cookie("a=1; b=2\n\nc=3; d=4\r\n").unwrap_err();
        assert!(error.contains("只支持一个账号"));
    }

    #[test]
    fn uses_single_account_notification_copy() {
        assert_eq!(
            notification_title(&CheckinStatus::Success),
            "什么值得买签到成功"
        );
        assert_eq!(
            notification_title(&CheckinStatus::AlreadyDone),
            "什么值得买签到结果"
        );
        let outcome = parse_checkin_response(
            &json!({
                "error_code": "0",
                "error_msg": "签到成功",
                "data": { "cpadd": "2", "daily_num": 18 }
            }),
            "Android",
        )
        .unwrap();
        assert_eq!(
            outcome.message,
            "签到成功，本次获得 2 积分，已连续签到 18 天"
        );
        assert!(!outcome.message.contains("账号"));
    }

    #[test]
    fn sanitizes_service_messages_before_persisting_them() {
        assert_eq!(sanitize_error("bad\ntoken\u{0}value"), "badtokenvalue");
    }
}
