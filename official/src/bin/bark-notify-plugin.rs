use axum::{
    Json, Router,
    body::Bytes,
    extract::State,
    http::{HeaderMap, StatusCode},
    routing::post,
};
use hmac::{Hmac, Mac};
use reqwest::{Client, Url};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::Sha256;
use std::{
    collections::{HashSet, VecDeque},
    env,
    sync::Arc,
    time::{Duration, SystemTime, UNIX_EPOCH},
};
use tokio::sync::Mutex;

const PLUGIN_ID: &str = "bark-notify";
const WEBHOOK_ADDR: &str = "127.0.0.1:18119";
const SIGNATURE_MAX_AGE_SECS: i64 = 300;
const RECENT_EVENT_LIMIT: usize = 1024;
const NOTIFICATION_EVENTS: [&str; 4] = [
    "notification.subscription_added",
    "notification.download_started",
    "notification.transfer_completed",
    "notification.library_added",
];

#[derive(Clone, Deserialize)]
struct Settings {
    #[serde(default = "default_server_url")]
    server_url: String,
    #[serde(default)]
    device_key: String,
    #[serde(default = "default_notification_types")]
    notification_types: Vec<String>,
    #[serde(default = "default_group")]
    group: String,
    #[serde(default)]
    sound: String,
    #[serde(default = "default_level")]
    level: String,
    #[serde(default = "default_icon_url")]
    icon_url: String,
    #[serde(default)]
    webhook_secret: String,
}

#[derive(Clone)]
struct AppContext {
    client: Client,
    settings: Settings,
    recent_events: Arc<Mutex<RecentEvents>>,
}

#[derive(Default)]
struct RecentEvents {
    ids: HashSet<String>,
    order: VecDeque<String>,
}

#[derive(Deserialize)]
struct EventEnvelope {
    api_version: u32,
    event_id: String,
    event: String,
    data: NotificationData,
}

#[derive(Deserialize)]
struct NotificationData {
    title: String,
    content: String,
    #[serde(default)]
    image_url: Option<String>,
}

#[derive(Debug, Serialize)]
struct BarkPayload<'a> {
    device_key: &'a str,
    title: &'a str,
    body: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    group: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    sound: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    level: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    icon: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    image: Option<&'a str>,
}

#[tokio::main]
async fn main() {
    if let Err(error) = run().await {
        eprintln!("{error}");
        std::process::exit(1);
    }
}

async fn run() -> Result<(), String> {
    let settings = load_settings()?;
    let client = Client::builder()
        .timeout(Duration::from_secs(20))
        .user_agent("Mediary/bark-notify")
        .build()
        .map_err(|error| format!("创建 Bark HTTP 客户端失败: {error}"))?;

    if let Ok(action) = env::var("MEDIARY_PLUGIN_ACTION") {
        if action != "test" {
            return Err(format!("不支持的 Bark 插件动作: {action}"));
        }
        let image_url = default_icon_url();
        send_bark(
            &client,
            &settings,
            "Mediary Bark 测试",
            "Bark 通知插件连接正常。",
            Some(&image_url),
        )
        .await?;
        println!("{}", json!({"notice": "Bark 测试通知已发送", "items": []}));
        return Ok(());
    }

    validate_runtime_settings(&settings)?;
    let context = AppContext {
        client,
        settings,
        recent_events: Arc::new(Mutex::new(RecentEvents::default())),
    };
    let app = Router::new()
        .route("/events", post(receive_event))
        .with_state(context);
    let listener = tokio::net::TcpListener::bind(WEBHOOK_ADDR)
        .await
        .map_err(|error| format!("Bark 插件监听 {WEBHOOK_ADDR} 失败: {error}"))?;
    axum::serve(listener, app)
        .await
        .map_err(|error| format!("Bark 插件 Webhook 服务异常: {error}"))
}

async fn receive_event(
    State(context): State<AppContext>,
    headers: HeaderMap,
    body: Bytes,
) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    verify_webhook(&headers, &body, &context.settings.webhook_secret)
        .map_err(|error| response_error(StatusCode::UNAUTHORIZED, error))?;
    let envelope: EventEnvelope = serde_json::from_slice(&body)
        .map_err(|_| response_error(StatusCode::BAD_REQUEST, "事件数据格式无效"))?;
    validate_envelope(&headers, &envelope)
        .map_err(|error| response_error(StatusCode::BAD_REQUEST, error))?;

    if !context
        .settings
        .notification_types
        .iter()
        .any(|event| event == &envelope.event)
    {
        return Ok(Json(json!({"success": true, "skipped": true})));
    }
    if context
        .recent_events
        .lock()
        .await
        .ids
        .contains(&envelope.event_id)
    {
        return Ok(Json(json!({"success": true, "duplicate": true})));
    }

    send_bark(
        &context.client,
        &context.settings,
        &envelope.data.title,
        &envelope.data.content,
        envelope.data.image_url.as_deref(),
    )
    .await
    .map_err(|error| response_error(StatusCode::BAD_GATEWAY, error))?;
    context.recent_events.lock().await.insert(envelope.event_id);
    Ok(Json(json!({"success": true})))
}

fn verify_webhook(headers: &HeaderMap, body: &[u8], secret: &str) -> Result<(), &'static str> {
    if secret.trim().is_empty() {
        return Err("Webhook 签名密钥未配置");
    }
    if header(headers, "x-mediary-plugin-id")? != PLUGIN_ID {
        return Err("插件 ID 不匹配");
    }
    let timestamp = header(headers, "x-mediary-timestamp")?
        .parse::<i64>()
        .map_err(|_| "Webhook 时间戳无效")?;
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|_| "系统时间无效")?
        .as_secs() as i64;
    if now.abs_diff(timestamp) > SIGNATURE_MAX_AGE_SECS as u64 {
        return Err("Webhook 请求已过期");
    }
    let signature = header(headers, "x-mediary-signature")?
        .strip_prefix("sha256=")
        .ok_or("Webhook 签名格式无效")?;
    let signature = hex::decode(signature).map_err(|_| "Webhook 签名格式无效")?;
    let mut mac =
        Hmac::<Sha256>::new_from_slice(secret.as_bytes()).map_err(|_| "Webhook 签名密钥无效")?;
    mac.update(timestamp.to_string().as_bytes());
    mac.update(b".");
    mac.update(body);
    mac.verify_slice(&signature)
        .map_err(|_| "Webhook 签名校验失败")
}

fn validate_envelope(headers: &HeaderMap, envelope: &EventEnvelope) -> Result<(), &'static str> {
    if envelope.api_version != 1 {
        return Err("不支持的事件 API 版本");
    }
    if envelope.event_id.is_empty() || header(headers, "x-mediary-event-id")? != envelope.event_id {
        return Err("事件 ID 不匹配");
    }
    if header(headers, "x-mediary-event")? != envelope.event {
        return Err("事件类型不匹配");
    }
    if !NOTIFICATION_EVENTS.contains(&envelope.event.as_str()) {
        return Err("不支持的通知事件");
    }
    if envelope.data.title.trim().is_empty() || envelope.data.content.trim().is_empty() {
        return Err("通知标题或内容为空");
    }
    Ok(())
}

async fn send_bark(
    client: &Client,
    settings: &Settings,
    title: &str,
    content: &str,
    image_url: Option<&str>,
) -> Result<(), String> {
    validate_delivery_settings(settings)?;
    let url = bark_push_url(&settings.server_url)?;
    let payload = bark_payload(settings, title, content, image_url);
    let response = client
        .post(url)
        .json(&payload)
        .send()
        .await
        .map_err(|error| format!("请求 Bark 服务失败: {error}"))?;
    let status = response.status();
    let response_body = response.bytes().await.unwrap_or_default();
    if !status.is_success() {
        return Err(format!("Bark 服务响应 HTTP {status}"));
    }
    if let Ok(value) = serde_json::from_slice::<Value>(&response_body)
        && value
            .get("code")
            .and_then(Value::as_i64)
            .is_some_and(|code| code != 200)
    {
        let message = value
            .get("message")
            .or_else(|| value.get("msg"))
            .and_then(Value::as_str)
            .unwrap_or("未知错误");
        return Err(format!("Bark 推送失败: {message}"));
    }
    Ok(())
}

fn bark_payload<'a>(
    settings: &'a Settings,
    title: &'a str,
    content: &'a str,
    image_url: Option<&'a str>,
) -> BarkPayload<'a> {
    BarkPayload {
        device_key: settings.device_key.trim(),
        title,
        body: content,
        group: non_empty(&settings.group),
        sound: non_empty(&settings.sound),
        level: non_empty(&settings.level),
        icon: non_empty(&settings.icon_url),
        image: image_url.and_then(non_empty),
    }
}

fn bark_push_url(server_url: &str) -> Result<Url, String> {
    let mut url = Url::parse(server_url.trim()).map_err(|_| "Bark 服务器地址无效".to_string())?;
    if !matches!(url.scheme(), "http" | "https")
        || url.host_str().is_none()
        || !url.username().is_empty()
        || url.password().is_some()
        || url.query().is_some()
        || url.fragment().is_some()
    {
        return Err("Bark 服务器地址必须是无凭据、查询参数和片段的 HTTP(S) 地址".to_string());
    }
    let path = format!("{}/push", url.path().trim_end_matches('/'));
    url.set_path(&path);
    Ok(url)
}

fn validate_runtime_settings(settings: &Settings) -> Result<(), String> {
    validate_delivery_settings(settings)?;
    if settings.webhook_secret.trim().is_empty() {
        return Err("缺少宿主生成的 Webhook 签名密钥".to_string());
    }
    Ok(())
}

fn validate_delivery_settings(settings: &Settings) -> Result<(), String> {
    bark_push_url(&settings.server_url)?;
    if settings.device_key.trim().is_empty() {
        return Err("请先配置 Bark 设备 Key".to_string());
    }
    if !matches!(
        settings.level.as_str(),
        "active" | "timeSensitive" | "passive" | "critical"
    ) {
        return Err("Bark 提醒级别无效".to_string());
    }
    Ok(())
}

fn load_settings() -> Result<Settings, String> {
    env::var("MEDIARY_PLUGIN_SETTINGS_JSON")
        .ok()
        .filter(|value| !value.trim().is_empty())
        .map(|raw| serde_json::from_str(&raw).map_err(|error| format!("解析插件配置失败: {error}")))
        .transpose()
        .map(|settings| settings.unwrap_or_else(default_settings))
}

fn default_settings() -> Settings {
    Settings {
        server_url: default_server_url(),
        device_key: String::new(),
        notification_types: default_notification_types(),
        group: default_group(),
        sound: String::new(),
        level: default_level(),
        icon_url: default_icon_url(),
        webhook_secret: String::new(),
    }
}

fn default_server_url() -> String {
    "https://api.day.app".to_string()
}

fn default_group() -> String {
    "Mediary".to_string()
}

fn default_level() -> String {
    "active".to_string()
}

fn default_icon_url() -> String {
    "https://img.andp.cc/icons/upload/Mediary.png".to_string()
}

fn default_notification_types() -> Vec<String> {
    NOTIFICATION_EVENTS.map(str::to_string).to_vec()
}

fn non_empty(value: &str) -> Option<&str> {
    let value = value.trim();
    (!value.is_empty()).then_some(value)
}

fn header<'a>(headers: &'a HeaderMap, name: &str) -> Result<&'a str, &'static str> {
    headers
        .get(name)
        .and_then(|value| value.to_str().ok())
        .ok_or("Webhook 请求头不完整")
}

fn response_error(status: StatusCode, message: impl Into<String>) -> (StatusCode, Json<Value>) {
    (status, Json(json!({"error": message.into()})))
}

impl RecentEvents {
    fn insert(&mut self, event_id: String) {
        if !self.ids.insert(event_id.clone()) {
            return;
        }
        self.order.push_back(event_id);
        while self.order.len() > RECENT_EVENT_LIMIT {
            if let Some(expired) = self.order.pop_front() {
                self.ids.remove(&expired);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::sync::mpsc;

    #[test]
    fn appends_push_to_bark_server_root_or_subpath() {
        assert_eq!(
            bark_push_url("https://api.day.app").unwrap().as_str(),
            "https://api.day.app/push"
        );
        assert_eq!(
            bark_push_url("https://example.test/bark/")
                .unwrap()
                .as_str(),
            "https://example.test/bark/push"
        );
        assert!(bark_push_url("https://user:pass@example.test").is_err());
    }

    #[test]
    fn bark_payload_omits_blank_optional_fields() {
        let mut settings = default_settings();
        settings.device_key = " device-key ".to_string();
        settings.group.clear();
        settings.sound.clear();
        let payload = serde_json::to_value(bark_payload(
            &settings,
            "下载完成",
            "测试内容",
            Some(" https://example.test/image.jpg "),
        ))
        .unwrap();
        assert_eq!(payload["device_key"], "device-key");
        assert_eq!(payload["title"], "下载完成");
        assert_eq!(payload["image"], "https://example.test/image.jpg");
        assert!(payload.get("group").is_none());
        assert!(payload.get("sound").is_none());
    }

    #[test]
    fn verifies_host_signature_and_rejects_tampering() {
        let body = br#"{"api_version":1}"#;
        let timestamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs()
            .to_string();
        let secret = "test-secret";
        let mut mac = Hmac::<Sha256>::new_from_slice(secret.as_bytes()).unwrap();
        mac.update(timestamp.as_bytes());
        mac.update(b".");
        mac.update(body);
        let signature = hex::encode(mac.finalize().into_bytes());
        let mut headers = HeaderMap::new();
        headers.insert("x-mediary-plugin-id", PLUGIN_ID.parse().unwrap());
        headers.insert("x-mediary-timestamp", timestamp.parse().unwrap());
        headers.insert(
            "x-mediary-signature",
            format!("sha256={signature}").parse().unwrap(),
        );

        assert!(verify_webhook(&headers, body, secret).is_ok());
        assert!(verify_webhook(&headers, b"tampered", secret).is_err());
    }

    #[test]
    fn notification_type_selection_supports_individual_categories() {
        let settings: Settings = serde_json::from_value(json!({
            "device_key": "key",
            "notification_types": ["notification.library_added"]
        }))
        .unwrap();
        assert_eq!(settings.notification_types, ["notification.library_added"]);
        assert!(
            !settings
                .notification_types
                .contains(&"notification.download_started".to_string())
        );
    }

    #[test]
    fn manifest_event_contract_matches_runtime() {
        let manifest: Value =
            serde_json::from_str(include_str!("../../bark-notify/plugin.json")).unwrap();
        assert_eq!(manifest["id"], PLUGIN_ID);
        assert_eq!(
            manifest["webhook_url"],
            format!("http://{WEBHOOK_ADDR}/events")
        );
        assert_eq!(
            manifest["events"],
            serde_json::to_value(NOTIFICATION_EVENTS).unwrap()
        );
    }

    #[tokio::test]
    async fn posts_json_to_bark_push_endpoint() {
        let (sender, mut receiver) = mpsc::unbounded_channel::<Value>();
        let app = Router::new().route(
            "/push",
            post(move |Json(payload): Json<Value>| {
                let sender = sender.clone();
                async move {
                    sender.send(payload).unwrap();
                    Json(json!({"code": 200, "message": "success"}))
                }
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let mut settings = default_settings();
        settings.server_url = format!("http://{address}");
        settings.device_key = "test-device".to_string();

        send_bark(
            &Client::new(),
            &settings,
            "入库完成",
            "测试影片已入库",
            Some("https://example.test/poster.jpg"),
        )
        .await
        .unwrap();
        let payload = receiver.recv().await.unwrap();
        assert_eq!(payload["device_key"], "test-device");
        assert_eq!(payload["title"], "入库完成");
        assert_eq!(payload["body"], "测试影片已入库");
        assert_eq!(payload["image"], "https://example.test/poster.jpg");
        server.abort();
    }
}
