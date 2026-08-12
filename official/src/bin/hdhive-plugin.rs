use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};
use chrono::{DateTime, Local, Utc};
use fs2::FileExt;
use hmac::{Hmac, Mac};
use rand::{RngCore, rngs::OsRng};
use reqwest::{Client, Method, Response, Url, header::RETRY_AFTER};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};
use sha2::{Digest, Sha256};
use std::{
    env, fs,
    fs::{File, OpenOptions},
    io::{Read, Write},
    path::{Path, PathBuf},
    time::Duration,
};
use subtle::ConstantTimeEq;

type HmacSha256 = Hmac<Sha256>;

const PROXY_BASE_URL: &str = "https://hdhive.symedia.top";
const PROXY_SERVER_SECRET: &str = match option_env!("HDHIVE_PROXY_SERVER_SECRET") {
    Some(value) => value,
    None => "",
};
const PROXY_USER_AGENT: &str = "python-requests/2.32.3";
const NOTIFICATION_IMAGE_URL: &str = "https://img.andp.cc/icons/upload/hdhive.png";
const MAX_RESPONSE_BYTES: u64 = 2 * 1024 * 1024;
const MAX_MEDIA_RESULTS: usize = 20;
const MAX_HOST_MEDIA_RESULTS: usize = 5;
const MAX_RESOURCE_RESULTS: usize = 200;
const OAUTH_STATE_TTL_SECONDS: i64 = 10 * 60;
const AUTH_STATE_FILE: &str = ".authorization.json";
const AUTH_LOCK_FILE: &str = ".authorization.lock";
const STATUS_FILE: &str = "status.json";

#[derive(Clone)]
struct PluginContext {
    settings: Map<String, Value>,
    client: Client,
    mediary_api_url: String,
    mediary_token: String,
    data_dir: PathBuf,
}

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
struct AuthState {
    #[serde(default)]
    pending_state_hash: String,
    pending_expires_at: Option<i64>,
    #[serde(default)]
    user_id: String,
    #[serde(default)]
    proxy_user_key: String,
    #[serde(default)]
    display_name: String,
    refresh_expires_at: Option<i64>,
}

#[derive(Debug, Clone)]
struct Credential {
    user_id: String,
    proxy_user_key: String,
    display_name: String,
}

#[derive(Debug)]
struct ProxySession {
    id: String,
    key: Vec<u8>,
    sequence: u64,
}

struct ProxyConnection<'a> {
    context: &'a PluginContext,
    credential: Option<Credential>,
    session: ProxySession,
}

#[derive(Debug)]
struct ApiEnvelope {
    status: u16,
    success: bool,
    code: String,
    message: String,
    description: String,
    data: Value,
    retry_after: Option<String>,
}

#[derive(Debug, Default, Deserialize, Serialize)]
struct StatusData {
    #[serde(default)]
    summary: Map<String, Value>,
    #[serde(default)]
    items: Vec<Value>,
}

struct CheckinOutcome {
    response: Value,
    checked_in: bool,
    gained_points: i64,
    message: String,
    username: Option<String>,
    current_points: Option<i64>,
}

#[tokio::main]
async fn main() {
    if let Err(error) = run().await {
        eprintln!("{error}");
        std::process::exit(1);
    }
}

async fn run() -> Result<(), String> {
    let context = PluginContext::from_env()?;
    let action = env::var("MEDIARY_PLUGIN_ACTION").unwrap_or_default();
    let payload = read_payload()?;
    let output = match action.as_str() {
        "search" => search(&context, &payload).await?,
        "resource_search" => resource_search(&context, &payload).await?,
        "resources" => resources(&context, &payload).await?,
        "transfer" => transfer_resource(&context, &payload).await?,
        "status" => account_status(&context).await?,
        "checkin" => {
            checkin(
                &context,
                setting_bool(&context.settings, "gambler_checkin", false),
            )
            .await?
        }
        "checkin_manual" => {
            checkin(
                &context,
                manual_checkin_gambler_mode(&context.settings, &payload),
            )
            .await?
        }
        "authorize" => authorize(&context, &payload).await?,
        "oauth_callback" => oauth_callback(&context, &payload)?,
        "logout" => logout(&context)?,
        _ => return Err(format!("HDHive 不支持动作: {action}")),
    };
    println!("{output}");
    Ok(())
}

impl PluginContext {
    fn from_env() -> Result<Self, String> {
        let settings = env::var("MEDIARY_PLUGIN_SETTINGS_JSON")
            .ok()
            .and_then(|value| serde_json::from_str::<Map<String, Value>>(&value).ok())
            .unwrap_or_default();
        let mediary_api_url = required_env("MEDIARY_PLUGIN_API_URL")?
            .trim_end_matches('/')
            .to_string();
        let mediary_token = required_env("MEDIARY_PLUGIN_TOKEN")?;
        let data_dir = PathBuf::from(required_env("MEDIARY_PLUGIN_DATA_DIR")?);
        fs::create_dir_all(&data_dir)
            .map_err(|error| format!("创建 HDHive 数据目录失败: {error}"))?;
        secure_directory(&data_dir)?;
        let client = Client::builder()
            .connect_timeout(Duration::from_secs(8))
            .timeout(Duration::from_secs(40))
            .redirect(reqwest::redirect::Policy::limited(3))
            .user_agent(PROXY_USER_AGENT)
            .build()
            .map_err(|error| format!("创建 HDHive 客户端失败: {error}"))?;
        Ok(Self {
            settings,
            client,
            mediary_api_url,
            mediary_token,
            data_dir,
        })
    }
}

impl<'a> ProxyConnection<'a> {
    async fn connect(
        context: &'a PluginContext,
        credential: Option<Credential>,
    ) -> Result<Self, String> {
        let session = new_proxy_session(context).await?;
        Ok(Self {
            context,
            credential,
            session,
        })
    }

    async fn oauth_start(&mut self, callback_url: &str) -> Result<ApiEnvelope, String> {
        let mut url = proxy_url(&["api", "v1", "oauth", "start"])?;
        url.query_pairs_mut().append_pair("callback", callback_url);
        self.signed_request(Method::POST, url, None, "").await
    }

    async fn open_api(
        &mut self,
        method: Method,
        path: &str,
        body: Option<Value>,
    ) -> Result<ApiEnvelope, String> {
        let credential = self
            .credential
            .as_ref()
            .ok_or_else(|| "HDHive 尚未登录".to_string())?;
        let logical = logical_open_api_path(path)?;
        let mut segments = vec!["api", "v1", "open", credential.user_id.as_str()];
        segments.extend(
            logical
                .trim_start_matches('/')
                .split('/')
                .filter(|part| !part.is_empty()),
        );
        let url = proxy_url(&segments)?;
        let user_key = credential.proxy_user_key.clone();
        self.signed_request(method, url, body, &user_key).await
    }

    async fn user_status(&mut self) -> Result<ApiEnvelope, String> {
        let credential = self
            .credential
            .as_ref()
            .ok_or_else(|| "HDHive 尚未登录".to_string())?;
        let url = proxy_url(&["api", "v1", "users", &credential.user_id, "status"])?;
        let user_key = credential.proxy_user_key.clone();
        self.signed_request(Method::GET, url, None, &user_key).await
    }

    async fn signed_request(
        &mut self,
        method: Method,
        url: Url,
        body: Option<Value>,
        user_key: &str,
    ) -> Result<ApiEnvelope, String> {
        let body_bytes = body
            .as_ref()
            .map(serde_json::to_vec)
            .transpose()
            .map_err(|error| format!("序列化 HDHive 请求失败: {error}"))?
            .unwrap_or_default();
        self.session.sequence = self.session.sequence.saturating_add(1);
        let sequence = self.session.sequence.to_string();
        let body_hash = sha256_hex(&body_bytes);
        let request_uri = request_uri(&url);
        let signature = proxy_request_signature(
            &self.session.key,
            method.as_str(),
            &request_uri,
            &self.session.id,
            &sequence,
            &body_hash,
            user_key,
        );
        let mut request = self
            .context
            .client
            .request(method, url)
            .header("Accept", "*/*")
            .header("User-Agent", PROXY_USER_AGENT)
            .header("X-Proxy-Session", &self.session.id)
            .header("X-Proxy-Sequence", sequence)
            .header("X-Proxy-Body-SHA256", body_hash)
            .header("X-Proxy-User-Key", user_key)
            .header("X-Proxy-Signature", signature);
        if body.is_some() {
            request = request
                .header("Content-Type", "application/json")
                .body(body_bytes);
        }
        let response = request.send().await.map_err(hdhive_request_error)?;
        envelope_result(parse_hdhive_response(response).await?)
    }
}

fn read_payload() -> Result<Value, String> {
    let mut input = String::new();
    std::io::stdin()
        .read_to_string(&mut input)
        .map_err(|error| format!("读取动作参数失败: {error}"))?;
    if input.trim().is_empty() {
        return Ok(json!({}));
    }
    serde_json::from_str(&input).map_err(|error| format!("动作参数不是有效 JSON: {error}"))
}

async fn authorize(context: &PluginContext, payload: &Value) -> Result<Value, String> {
    let callback_url = validate_callback_url(required_text(payload, "callback_url")?)?;
    let state = random_nonce(24)?;
    let state_hash = sha256_hex(state.as_bytes());
    let expires_at = Utc::now().timestamp() + OAUTH_STATE_TTL_SECONDS;
    update_auth_state(&context.data_dir, |auth| {
        auth.pending_state_hash = state_hash.clone();
        auth.pending_expires_at = Some(expires_at);
        Ok(())
    })?;

    let mut callback = callback_url;
    callback.query_pairs_mut().append_pair("state", &state);
    let result = async {
        let mut proxy = ProxyConnection::connect(context, None).await?;
        let envelope = proxy.oauth_start(callback.as_str()).await?;
        first_text(
            &envelope.data,
            &["authorization_url", "authorize_url", "url"],
        )
        .or_else(|| (!envelope.message.is_empty()).then_some(envelope.message))
        .filter(|value| safe_http_url(value))
        .ok_or_else(|| "HDHive 未返回有效的登录地址".to_string())
    }
    .await;

    let authorization_url = match result {
        Ok(url) => url,
        Err(error) => {
            let _ = update_auth_state(&context.data_dir, |auth| {
                if auth.pending_state_hash == state_hash {
                    auth.pending_state_hash.clear();
                    auth.pending_expires_at = None;
                }
                Ok(())
            });
            return Err(error);
        }
    };
    Ok(json!({
        "open_url": authorization_url,
        "notice": "已在新窗口打开 HDHive 登录。"
    }))
}

fn oauth_callback(context: &PluginContext, payload: &Value) -> Result<Value, String> {
    let state = required_text(payload, "state")?;
    let supplied_hash = sha256_hex(state.as_bytes());
    let callback_error = first_text(payload, &["error", "error_description"]);
    let user_id = first_text(payload, &["userid", "user_id", "userId", "id"]);
    let proxy_user_key = first_text(
        payload,
        &[
            "proxy_user_key",
            "proxyUserKey",
            "user_key",
            "userKey",
            "key",
        ],
    );
    let display_name = first_text(payload, &["display_name", "username"]);
    let refresh_expires_at = first_i64(payload, &["refresh_expires_at"]);

    let lock = lock_auth_state(&context.data_dir)?;
    let mut auth = read_auth_state(&context.data_dir).unwrap_or_default();
    let expected_hash = auth.pending_state_hash.clone();
    let expires_at = auth.pending_expires_at;
    auth.pending_state_hash.clear();
    auth.pending_expires_at = None;

    let state_matches = expected_hash.len() == supplied_hash.len()
        && bool::from(expected_hash.as_bytes().ct_eq(supplied_hash.as_bytes()));
    let validation = if callback_error.is_some() {
        Err("HDHive 登录未完成".to_string())
    } else if expected_hash.is_empty()
        || !state_matches
        || expires_at.is_some_and(|value| value <= Utc::now().timestamp())
    {
        Err("HDHive 登录回调已失效，请重新登录".to_string())
    } else {
        match (user_id, proxy_user_key) {
            (Some(user_id), Some(proxy_user_key)) => {
                auth.user_id = user_id;
                auth.proxy_user_key = proxy_user_key;
                auth.display_name = display_name.unwrap_or_else(|| auth.user_id.clone());
                auth.refresh_expires_at = refresh_expires_at;
                Ok(())
            }
            _ => Err("HDHive 登录回调缺少授权信息".to_string()),
        }
    };
    write_auth_state(&context.data_dir, &auth)?;
    drop(lock);
    validation?;
    Ok(json!({"success": true}))
}

fn logout(context: &PluginContext) -> Result<Value, String> {
    update_auth_state(&context.data_dir, |auth| {
        *auth = AuthState::default();
        Ok(())
    })?;
    let mut response = authorization_response();
    response["notice"] = json!("已退出 HDHive 登录。");
    Ok(response)
}

async fn search(context: &PluginContext, payload: &Value) -> Result<Value, String> {
    if credential(context).is_err() {
        return Ok(json!({
            "notice": "HDHive 尚未登录，请先在插件配置中登录。",
            "items": []
        }));
    }
    let query = payload
        .get("query")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .trim();
    let media_type = normalize_media_type(
        payload
            .get("media_type")
            .and_then(Value::as_str)
            .unwrap_or("multi"),
        true,
    )?;
    if query.is_empty() {
        return Ok(json!({
            "notice": "请输入电影、剧集名称或 TMDB ID。",
            "items": []
        }));
    }
    if let Ok(tmdb_id) = query.parse::<i64>()
        && tmdb_id > 0
    {
        if media_type == "multi" {
            return Err("使用 TMDB ID 搜索时请选择电影或剧集".to_string());
        }
        return resources(
            context,
            &json!({
                "tmdb_id": tmdb_id,
                "media_type": media_type,
                "title": format!("TMDB {tmdb_id}"),
                "query": query
            }),
        )
        .await;
    }

    let candidates = search_tmdb_candidates(context, query, media_type).await?;
    let items = candidates
        .iter()
        .filter_map(|candidate| media_candidate_item(candidate, query))
        .take(MAX_MEDIA_RESULTS)
        .collect::<Vec<_>>();
    Ok(json!({"items": items}))
}

async fn search_tmdb_candidates(
    context: &PluginContext,
    query: &str,
    media_type: &str,
) -> Result<Vec<Value>, String> {
    let response = context
        .client
        .get(format!("{}/search/tmdb", context.mediary_api_url))
        .bearer_auth(&context.mediary_token)
        .query(&[("query", query), ("media_type", media_type)])
        .send()
        .await
        .map_err(mediary_request_error)?;
    let value = parse_mediary_response(response).await?;
    value
        .as_array()
        .cloned()
        .ok_or_else(|| "Mediary TMDB 搜索响应格式无效".to_string())
}

async fn resource_search(context: &PluginContext, payload: &Value) -> Result<Value, String> {
    let query = required_text(payload, "query")?;
    let credential = credential(context)?;
    let candidates = search_tmdb_candidates(context, query, "multi").await?;
    let mut proxy = ProxyConnection::connect(context, Some(credential)).await?;
    let mut results = Vec::new();

    for candidate in candidates.iter().take(MAX_HOST_MEDIA_RESULTS) {
        let Some(tmdb_id) = candidate
            .get("id")
            .and_then(Value::as_i64)
            .filter(|id| *id > 0)
        else {
            continue;
        };
        let Ok(media_type) = normalize_media_type(
            candidate
                .get("media_type")
                .and_then(Value::as_str)
                .unwrap_or_default(),
            false,
        ) else {
            continue;
        };
        let media_title = first_text(candidate, &["mediary_title", "title", "name"])
            .unwrap_or_else(|| format!("TMDB {tmdb_id}"));
        let year = candidate
            .get("mediary_year")
            .and_then(Value::as_i64)
            .or_else(|| {
                first_text(candidate, &["release_date", "first_air_date"])
                    .and_then(|date| date.get(0..4)?.parse::<i64>().ok())
            });
        let envelope = match fetch_resources(&mut proxy, media_type, tmdb_id).await {
            Ok(envelope) => envelope,
            Err(error) => {
                eprintln!("跳过 HDHive TMDB {tmdb_id} 资源: {error}");
                continue;
            }
        };
        results.extend(
            extract_resource_items(&envelope.data)
                .iter()
                .filter_map(|resource| {
                    host_resource_item(resource, media_type, tmdb_id, &media_title, year)
                }),
        );
        if results.len() >= MAX_RESOURCE_RESULTS {
            break;
        }
    }
    results.truncate(MAX_RESOURCE_RESULTS);
    Ok(json!({"results": results}))
}

fn media_candidate_item(candidate: &Value, query: &str) -> Option<Value> {
    let tmdb_id = candidate.get("id")?.as_i64()?;
    if tmdb_id <= 0 {
        return None;
    }
    let media_type = normalize_media_type(
        candidate
            .get("media_type")
            .and_then(Value::as_str)
            .unwrap_or("movie"),
        false,
    )
    .ok()?;
    let title = first_text(candidate, &["mediary_title", "title", "name"])
        .unwrap_or_else(|| format!("TMDB {tmdb_id}"));
    let original_title = first_text(candidate, &["original_title", "original_name"]);
    let overview = first_text(candidate, &["overview", "description"])
        .unwrap_or_else(|| "暂无简介".to_string());
    let year = candidate
        .get("mediary_year")
        .and_then(Value::as_i64)
        .or_else(|| {
            first_text(candidate, &["release_date", "first_air_date"])
                .and_then(|date| date.get(0..4)?.parse::<i64>().ok())
        });
    let poster = candidate
        .get("poster_path")
        .and_then(Value::as_str)
        .filter(|value| value.starts_with('/'))
        .map(|path| format!("https://image.tmdb.org/t/p/w342{path}"));
    let mut metadata = vec![format!("TMDB {tmdb_id}")];
    if let Some(original_title) = original_title.filter(|value| value != &title) {
        metadata.push(original_title);
    }
    if let Some(rating) = candidate.get("vote_average").and_then(Value::as_f64)
        && rating > 0.0
    {
        metadata.push(format!("评分 {rating:.1}"));
    }
    let mut badges = vec![json!({
        "label": if media_type == "tv" { "剧集" } else { "电影" },
        "tone": if media_type == "tv" { "info" } else { "success" }
    })];
    if let Some(year) = year {
        badges.push(json!({"label": year.to_string(), "tone": "neutral"}));
    }
    Some(json!({
        "key": format!("{media_type}:{tmdb_id}"),
        "title": title,
        "subtitle": overview,
        "image_url": poster,
        "badges": badges,
        "metadata": metadata,
        "click_action": {
            "type": "plugin_action",
            "action": "resources",
            "label": "查看资源",
            "pending_label": "查询中",
            "icon": "search",
            "payload": {
                "tmdb_id": tmdb_id,
                "media_type": media_type,
                "title": title,
                "query": query
            },
            "error_message": "查询 HDHive 资源失败。"
        }
    }))
}

async fn resources(context: &PluginContext, payload: &Value) -> Result<Value, String> {
    let tmdb_id = positive_i64(payload, "tmdb_id")?;
    let media_type = normalize_media_type(required_text(payload, "media_type")?, false)?;
    let title = first_text(payload, &["title"]).unwrap_or_else(|| format!("TMDB {tmdb_id}"));
    let mut proxy = ProxyConnection::connect(context, Some(credential(context)?)).await?;
    let envelope = fetch_resources(&mut proxy, media_type, tmdb_id).await?;
    let resources = extract_resource_items(&envelope.data);
    let mut items = resources
        .iter()
        .filter_map(|resource| resource_item(resource, media_type, tmdb_id, &title))
        .collect::<Vec<_>>();
    let transferable_count = items.len();
    items.truncate(MAX_RESOURCE_RESULTS);
    let query = first_text(payload, &["query"]).unwrap_or_else(|| title.clone());
    let notice = if items.is_empty() {
        format!("影巢暂无《{title}》的可用资源。")
    } else if transferable_count > MAX_RESOURCE_RESULTS {
        format!("资源较多，已显示前 {MAX_RESOURCE_RESULTS} 条。")
    } else {
        format!("找到 {} 条影巢资源。", items.len())
    };
    Ok(json!({
        "notice": notice,
        "items": items,
        "actions": [{
            "type": "plugin_action",
            "action": "search",
            "label": "返回搜索",
            "pending_label": "加载中",
            "icon": "arrow-left",
            "payload": {"query": query, "media_type": "multi"}
        }]
    }))
}

async fn fetch_resources(
    proxy: &mut ProxyConnection<'_>,
    media_type: &str,
    tmdb_id: i64,
) -> Result<ApiEnvelope, String> {
    proxy
        .open_api(
            Method::GET,
            &format!("/api/open/resources/{media_type}/{tmdb_id}"),
            None,
        )
        .await
}

fn extract_resource_items(data: &Value) -> Vec<Value> {
    if let Some(items) = data.as_array() {
        return items.clone();
    }
    for key in ["resources", "items", "results", "list"] {
        if let Some(items) = data.get(key).and_then(Value::as_array) {
            return items.clone();
        }
    }
    Vec::new()
}

fn resource_item(
    resource: &Value,
    media_type: &str,
    tmdb_id: i64,
    media_title: &str,
) -> Option<Value> {
    let slug = first_text(resource, &["slug"])?;
    let title = first_text(resource, &["title", "name"]).unwrap_or_else(|| media_title.to_string());
    let pan_type =
        first_text(resource, &["pan_type", "website"]).unwrap_or_else(|| "资源".to_string());
    if !is_transferable_pan_type(&pan_type) {
        return None;
    }
    let unlock_points = first_i64(resource, &["unlock_points", "points", "cost"]).unwrap_or(0);
    let unlocked =
        first_bool(resource, &["is_unlocked", "unlocked", "already_owned"]).unwrap_or(false);
    let mut metadata = Vec::new();
    let publisher = resource_publisher(resource);
    if let Some(publisher) = &publisher {
        metadata.push(json!({"label": "发布者", "value": publisher}));
    }
    if let Some(published_at) = resource_publish_date(resource) {
        metadata.push(json!({"label": "发布于", "value": published_at}));
    }
    push_metadata(&mut metadata, resource, "share_size", "大小");
    push_metadata(&mut metadata, resource, "video_resolution", "分辨率");
    push_metadata(&mut metadata, resource, "source", "片源");
    push_metadata(&mut metadata, resource, "subtitle_language", "字幕");
    if !unlocked {
        metadata.push(json!({"label": "解锁", "value": format!("{unlock_points} 积分")}));
    }
    let paid = !unlocked && unlock_points > 0;
    let action_label = if paid {
        format!("{unlock_points} 积分解锁并转存")
    } else {
        "转存推送".to_string()
    };
    let actions = vec![json!({
        "type": "plugin_action",
        "action": "transfer",
        "label": action_label,
        "pending_label": "推送中",
        "icon": "folder-input",
        "tone": if paid { "warning" } else { "success" },
        "payload": {
            "slug": slug,
            "title": title,
            "pan_type": pan_type,
            "unlock_points": unlock_points,
            "tmdb_id": tmdb_id,
            "media_type": media_type
        },
        "confirm": paid.then(|| json!({
            "title": "确认解锁并转存",
            "message": format!("确定使用 {unlock_points} 积分解锁《{title}》并立即转存吗？"),
            "confirm_text": format!("{unlock_points} 积分解锁并转存")
        })),
        "error_message": "HDHive 资源转存失败。"
    })];
    let item = json!({
        "key": slug,
        "title": title,
        "subtitle": first_text(resource, &["remark", "description"]),
        "badges": [{
            "label": pan_type,
            "tone": "info"
        }, {
            "label": if unlocked { "已解锁" } else if unlock_points == 0 { "免积分" } else { "待解锁" },
            "tone": if unlocked || unlock_points == 0 { "success" } else { "warning" }
        }],
        "metadata": metadata,
        "actions": actions
    });
    Some(item)
}

fn host_resource_item(
    resource: &Value,
    media_type: &str,
    tmdb_id: i64,
    media_title: &str,
    year: Option<i64>,
) -> Option<Value> {
    let slug = first_text(resource, &["slug"])?;
    let title = first_text(resource, &["title", "name"]).unwrap_or_else(|| media_title.to_string());
    let pan_type =
        first_text(resource, &["pan_type", "website"]).unwrap_or_else(|| "资源".to_string());
    if !is_transferable_pan_type(&pan_type) {
        return None;
    }
    let unlock_points = first_i64(resource, &["unlock_points", "points", "cost"]).unwrap_or(0);
    let unlocked =
        first_bool(resource, &["is_unlocked", "unlocked", "already_owned"]).unwrap_or(false);
    let resolution = first_text(resource, &["video_resolution", "resolution", "quality"]);
    let source = first_text(resource, &["source"]);
    let subtitles = first_text(resource, &["subtitle_language", "subtitles"]);
    let labels = [
        resolution.clone(),
        source.clone(),
        subtitles,
        Some(if unlocked {
            "已解锁".to_string()
        } else if unlock_points == 0 {
            "免积分".to_string()
        } else {
            format!("{unlock_points} 积分")
        }),
    ]
    .into_iter()
    .flatten()
    .collect::<Vec<_>>();

    Some(json!({
        "title": title,
        "tmdb_id": tmdb_id,
        "site_name": "HDHive",
        "site_id": -2,
        "size": resource_size_bytes(resource),
        "download_url": format!("hdhive:{slug}"),
        "description": first_text(resource, &["remark", "description"]),
        "publish_time": first_text(resource, &[
            "published_at", "publishedAt", "created_at", "createdAt", "publish_date", "publishDate", "date"
        ]),
        "category": pan_type,
        "uploader": resource_publisher(resource),
        "seeders": 0,
        "leechers": 0,
        "labels": labels,
        "hit_and_run": false,
        "source_kind": "plugin",
        "plugin_id": "hdhive",
        "plugin_action": "transfer",
        "plugin_key": slug,
        "plugin_payload": {
            "slug": slug,
            "title": title,
            "pan_type": pan_type,
            "unlock_points": unlock_points,
            "tmdb_id": tmdb_id,
            "media_type": media_type
        },
        "unlock_points": unlock_points,
        "unlocked": unlocked,
        "parsed": {
            "title": media_title,
            "year": year,
            "media_type": media_type,
            "resolution": resolution,
            "resource_type": source,
            "raw_title": title
        }
    }))
}

fn resource_size_bytes(resource: &Value) -> i64 {
    let Some(value) = resource.get("share_size").or_else(|| resource.get("size")) else {
        return 0;
    };
    if let Some(size) = value.as_i64() {
        return size.max(0);
    }
    parse_size_bytes(value.as_str().unwrap_or_default()).unwrap_or(0)
}

fn parse_size_bytes(value: &str) -> Option<i64> {
    let compact = value.trim().to_ascii_uppercase().replace([',', ' '], "");
    let unit_start = compact
        .char_indices()
        .find_map(|(index, character)| character.is_ascii_alphabetic().then_some(index))
        .unwrap_or(compact.len());
    let amount = compact.get(..unit_start)?.parse::<f64>().ok()?;
    if !amount.is_finite() || amount < 0.0 {
        return None;
    }
    let multiplier = match compact.get(unit_start..).unwrap_or_default() {
        "" | "B" => 1_f64,
        "K" | "KB" | "KIB" => 1024_f64,
        "M" | "MB" | "MIB" => 1024_f64.powi(2),
        "G" | "GB" | "GIB" => 1024_f64.powi(3),
        "T" | "TB" | "TIB" => 1024_f64.powi(4),
        _ => return None,
    };
    let bytes = amount * multiplier;
    (bytes <= i64::MAX as f64).then_some(bytes.round() as i64)
}

fn resource_publisher(resource: &Value) -> Option<String> {
    first_text(
        resource,
        &[
            "uploader",
            "up",
            "username",
            "user_name",
            "userName",
            "author",
            "publisher",
            "publisher_name",
            "publisherName",
            "owner",
            "owner_name",
            "ownerName",
            "creator",
            "created_by",
            "createdBy",
        ],
    )
    .or_else(|| {
        first_nested_text(
            resource,
            &[
                "user",
                "publisher_info",
                "publisherInfo",
                "creator_info",
                "creatorInfo",
            ],
            &[
                "nickname",
                "nick_name",
                "nickName",
                "username",
                "user_name",
                "userName",
                "name",
                "display_name",
                "displayName",
            ],
        )
    })
}

fn resource_publish_date(resource: &Value) -> Option<String> {
    let value = first_text(
        resource,
        &[
            "published_at",
            "publishedAt",
            "created_at",
            "createdAt",
            "publish_date",
            "publishDate",
            "date",
        ],
    )?;
    if let Ok(timestamp) = DateTime::parse_from_rfc3339(&value) {
        return Some(
            timestamp
                .with_timezone(&Local)
                .format("%Y/%m/%d %H:%M")
                .to_string(),
        );
    }
    let date = value.get(0..10).filter(|candidate| {
        candidate
            .chars()
            .enumerate()
            .all(|(index, character)| match index {
                4 | 7 => character == '-',
                _ => character.is_ascii_digit(),
            })
    });
    Some(
        date.map(|candidate| candidate.replace('-', "/"))
            .unwrap_or(value),
    )
}

fn first_nested_text(value: &Value, object_keys: &[&str], field_keys: &[&str]) -> Option<String> {
    object_keys
        .iter()
        .filter_map(|key| value.get(key))
        .find_map(|object| first_text(object, field_keys))
}

fn is_transferable_pan_type(value: &str) -> bool {
    let normalized = value
        .trim()
        .to_ascii_lowercase()
        .replace([' ', '-', '_'], "");
    matches!(normalized.as_str(), "115" | "p115" | "115网盘" | "ed2k")
}

async fn transfer_resource(context: &PluginContext, payload: &Value) -> Result<Value, String> {
    let slug = required_text(payload, "slug")?;
    let tmdb_id = positive_i64(payload, "tmdb_id")?;
    let media_type = normalize_media_type(required_text(payload, "media_type")?, false)?;
    let mut proxy = ProxyConnection::connect(context, Some(credential(context)?)).await?;
    let current = fetch_resources(&mut proxy, media_type, tmdb_id).await?;
    let resource = extract_resource_items(&current.data)
        .into_iter()
        .find(|item| first_text(item, &["slug"]).as_deref() == Some(slug))
        .ok_or_else(|| "解锁前未找到该资源，请重新搜索".to_string())?;
    let current_points = first_i64(&resource, &["unlock_points", "points", "cost"]).unwrap_or(0);
    let expected_points = first_i64(payload, &["unlock_points"]).unwrap_or(current_points);
    let already_unlocked =
        first_bool(&resource, &["is_unlocked", "unlocked", "already_owned"]).unwrap_or(false);
    let link = if already_unlocked {
        first_text(&resource, &["full_url", "url", "media_url", "share_url"])
            .ok_or_else(|| "HDHive 资源已解锁，但响应中没有资源链接".to_string())?
    } else {
        ensure_unlock_points_unchanged(expected_points, current_points, false)?;
        let envelope = proxy
            .open_api(
                Method::POST,
                "/api/open/resources/unlock",
                Some(json!({"slug": slug})),
            )
            .await?;
        first_text(
            &envelope.data,
            &["full_url", "url", "media_url", "share_url"],
        )
        .ok_or_else(|| "HDHive 解锁成功，但响应中没有资源链接".to_string())?
    };
    let title = first_text(payload, &["title"])
        .or_else(|| first_text(&resource, &["title", "name"]))
        .unwrap_or_else(|| "HDHive 资源".to_string());
    let pan_type = first_text(payload, &["pan_type"])
        .or_else(|| first_text(&resource, &["pan_type", "website"]))
        .unwrap_or_else(|| "资源".to_string());
    let season = first_i64(payload, &["season"])
        .filter(|season| *season > 0 && *season <= i32::MAX as i64)
        .map(|season| season as i32);
    let media_hint = mediary_media_hint(payload, &title, &media_type, tmdb_id, season, &[]);
    let submitted = submit_transfer_link(context, &link, Some(&media_hint)).await?;
    let message =
        first_text(&submitted, &["message"]).unwrap_or_else(|| "资源已提交转存".to_string());
    let mode = first_text(&submitted, &["mode"]).unwrap_or_default();
    let target = first_text(&submitted, &["target"]).unwrap_or_default();
    let notification = transfer_submitted_notification(&title, &pan_type, &message);
    if let Err(error) = send_plugin_notification(context, &notification).await {
        eprintln!("HDHive 转存通知发送失败: {error}");
    }
    Ok(json!({
        "notice": message,
        "items": [{
            "key": slug,
            "title": title,
            "badges": [
                {"label": pan_type, "tone": "info"},
                {"label": if mode == "offline" && target == "transfer" { "已提交转存目录" } else if mode == "offline" { "已提交离线下载" } else { "已提交转存" }, "tone": "success"}
            ],
            "metadata": [format!("本次解锁 {current_points} 积分")]
        }]
    }))
}

fn transfer_submitted_notification(title: &str, pan_type: &str, message: &str) -> Value {
    json!({
        "title": "📥 HDHive 转存已提交",
        "content": format!("资源：{title}\n网盘：{pan_type}\n结果：{message}"),
        "image_url": NOTIFICATION_IMAGE_URL
    })
}

async fn submit_transfer_link(
    context: &PluginContext,
    link: &str,
    media_hint: Option<&Value>,
) -> Result<Value, String> {
    let payload = mediary_link_submit_payload(link, media_hint);
    let response = context
        .client
        .post(format!("{}/link/submit", context.mediary_api_url))
        .bearer_auth(&context.mediary_token)
        .json(&payload)
        .send()
        .await
        .map_err(mediary_request_error)?;
    parse_mediary_response(response).await
}

fn mediary_link_submit_payload(link: &str, media_hint: Option<&Value>) -> Value {
    let mut payload = if link.trim().to_ascii_lowercase().starts_with("ed2k://") {
        json!({
            "link": link,
            "offline_target": "transfer",
            "flowlink_move_all_delay_seconds": 10
        })
    } else {
        json!({"link": link})
    };
    if let (Some(fields), Some(media_hint)) = (payload.as_object_mut(), media_hint) {
        fields.insert("media_hint".to_string(), media_hint.clone());
    }
    payload
}

fn mediary_media_hint(
    payload: &Value,
    title: &str,
    media_type: &str,
    tmdb_id: i64,
    season: Option<i32>,
    episodes: &[i32],
) -> Value {
    json!({
        "schema_version": 1,
        "source": "mediary",
        "tmdb_id": tmdb_id,
        "media_type": media_type,
        "title": title,
        "year": first_i64(payload, &["year"])
            .filter(|value| (1800..=3000).contains(value)),
        "season": season,
        "episodes": episodes,
        "secondary_category": first_text(payload, &["secondary_category"]),
        "sha1": Value::Null,
        "receive_title": Value::Null,
    })
}

fn ensure_unlock_points_unchanged(
    expected_points: i64,
    current_points: i64,
    already_unlocked: bool,
) -> Result<(), String> {
    if !already_unlocked && current_points != expected_points {
        return Err(format!(
            "资源解锁积分已从 {expected_points} 变为 {current_points}，请返回资源列表重新确认"
        ));
    }
    Ok(())
}

async fn account_status(context: &PluginContext) -> Result<Value, String> {
    let credential = match credential(context) {
        Ok(credential) => credential,
        Err(_) => return Ok(authorization_response()),
    };
    let mut proxy = ProxyConnection::connect(context, Some(credential.clone())).await?;
    let mut account = Map::new();
    let mut last_error = None;
    match proxy.user_status().await {
        Ok(envelope) => merge_object(&mut account, envelope.data),
        Err(error) => last_error = Some(error),
    }
    match proxy.open_api(Method::GET, "/api/open/me", None).await {
        Ok(envelope) => merge_object(&mut account, envelope.data),
        Err(error) => last_error = Some(error),
    }
    if account.is_empty() {
        return Err(last_error.unwrap_or_else(|| "HDHive 账号信息未返回".to_string()));
    }
    let account = Value::Object(account);
    let username = first_text(&account, &["username", "name", "nickname", "display_name"])
        .unwrap_or_else(|| credential.display_name.clone());
    let points = first_i64(&account, &["points", "point"]).unwrap_or(0);
    let checked_in = first_bool(
        &account,
        &["checked_in_today", "checked_in", "is_checked_in"],
    )
    .unwrap_or(false)
        || locally_checked_in_today(&context.data_dir);
    let level = first_text(&account, &["level", "user_level"]);
    let mut actions = Vec::new();
    actions.push(json!({
        "type": "browser_auth",
        "action": "authorize",
        "label": "重新登录",
        "pending_label": "正在打开",
        "icon": "log-in"
    }));
    actions.push(json!({
        "type": "plugin_action",
        "action": "logout",
        "label": "退出登录",
        "icon": "trash",
        "tone": "danger",
        "confirm": {
            "title": "退出 HDHive",
            "message": "确定清除当前 HDHive 登录吗？",
            "confirm_text": "退出登录",
            "danger": true
        }
    }));

    Ok(json!({
        "notice": "HDHive 已登录。",
        "items": [{
            "key": "account",
            "title": username,
            "badges": [{
                "label": if checked_in { "今日已签到" } else { "今日未签到" },
                "tone": if checked_in { "success" } else { "warning" }
            }],
            "metadata": [
                json!({"label": "积分", "value": points}),
                json!({"label": "等级", "value": level.unwrap_or_else(|| "-".to_string())})
            ],
            "actions": actions
        }]
    }))
}

async fn checkin(context: &PluginContext, gambler: bool) -> Result<Value, String> {
    match perform_checkin(context, gambler).await {
        Ok(outcome) => {
            let notification = successful_checkin_notification(&outcome, gambler);
            if let Err(error) = send_plugin_notification(context, &notification).await {
                eprintln!("HDHive 签到结果通知发送失败: {error}");
            }
            Ok(outcome.response)
        }
        Err(error) => {
            let notification = failed_checkin_notification(&error, gambler);
            if let Err(notification_error) = send_plugin_notification(context, &notification).await
            {
                eprintln!("HDHive 签到失败通知发送失败: {notification_error}");
            }
            Err(error)
        }
    }
}

async fn perform_checkin(context: &PluginContext, gambler: bool) -> Result<CheckinOutcome, String> {
    let mut proxy = ProxyConnection::connect(context, Some(credential(context)?)).await?;
    let envelope = proxy
        .open_api(
            Method::POST,
            "/api/open/checkin",
            Some(json!({"is_gambler": gambler})),
        )
        .await?;
    let checked_in =
        first_bool(&envelope.data, &["checked_in", "checked_in_today"]).unwrap_or(envelope.success);
    let gained_points = first_i64(&envelope.data, &["points", "point"]).unwrap_or(0);
    let message = first_text(&envelope.data, &["message"])
        .filter(|value| !value.is_empty())
        .or_else(|| (!envelope.message.is_empty()).then_some(envelope.message.clone()))
        .unwrap_or_else(|| "签到成功".to_string());
    let account = proxy.open_api(Method::GET, "/api/open/me", None).await.ok();
    let account_data = account.as_ref().map(|value| &value.data);
    let username = account_data
        .and_then(|value| first_text(value, &["username", "name", "nickname", "display_name"]));
    let current_points = account_data.and_then(|value| first_i64(value, &["points", "point"]));
    update_status_data(context, account_data, &message, gained_points, checked_in)?;
    let mut response = account_status(context)
        .await
        .unwrap_or_else(|_| json!({"items": []}));
    if let Some(object) = response.as_object_mut() {
        object.insert("notice".into(), Value::String(message.clone()));
        object.insert(
            "report".into(),
            json!({"checked_in": checked_in, "points": gained_points, "gambler": gambler}),
        );
    }
    Ok(CheckinOutcome {
        response,
        checked_in,
        gained_points,
        message,
        username,
        current_points,
    })
}

fn successful_checkin_notification(outcome: &CheckinOutcome, gambler: bool) -> Value {
    let mut lines = Vec::new();
    if let Some(username) = &outcome.username {
        lines.push(format!("账号：{username}"));
    }
    lines.push(format!("结果：{}", outcome.message));
    lines.push(format!(
        "签到方式：{}",
        if gambler {
            "赌狗签到"
        } else {
            "普通签到"
        }
    ));
    lines.push(format!("本次积分：{}", outcome.gained_points));
    if let Some(points) = outcome.current_points {
        lines.push(format!("当前积分：{points}"));
    }
    json!({
        "title": if outcome.checked_in { "HDHive 签到成功" } else { "HDHive 签到结果" },
        "content": lines.join("\n"),
        "image_url": NOTIFICATION_IMAGE_URL
    })
}

fn failed_checkin_notification(error: &str, gambler: bool) -> Value {
    json!({
        "title": "HDHive 签到任务失败",
        "content": format!(
            "签到方式：{}\n失败原因：{error}",
            if gambler { "赌狗签到" } else { "普通签到" }
        ),
        "image_url": NOTIFICATION_IMAGE_URL
    })
}

async fn send_plugin_notification(
    context: &PluginContext,
    notification: &Value,
) -> Result<(), String> {
    let response = context
        .client
        .post(format!("{}/plugin/notifications", context.mediary_api_url))
        .bearer_auth(&context.mediary_token)
        .json(notification)
        .send()
        .await
        .map_err(mediary_request_error)?;
    parse_mediary_response(response).await.map(|_| ())
}

fn logical_open_api_path(path: &str) -> Result<&str, String> {
    path.strip_prefix("/api/open")
        .filter(|logical| logical.starts_with('/'))
        .ok_or_else(|| "HDHive API 路径无效".to_string())
}

fn authorization_response() -> Value {
    json!({
        "notice": "HDHive 尚未登录。",
        "items": [{
            "key": "authorization",
            "title": "登录 HDHive",
            "subtitle": "点击后在浏览器完成影巢账号授权，无需填写应用参数。",
            "badges": [{"label": "待登录", "tone": "warning"}],
            "actions": [{
                "type": "browser_auth",
                "action": "authorize",
                "label": "登录影巢",
                "pending_label": "正在打开",
                "icon": "log-in",
                "error_message": "无法打开 HDHive 登录。"
            }]
        }]
    })
}

async fn new_proxy_session(context: &PluginContext) -> Result<ProxySession, String> {
    let proxy_server_secret = PROXY_SERVER_SECRET.trim();
    if proxy_server_secret.is_empty() {
        return Err("HDHive 插件发行包缺少服务鉴权配置".to_string());
    }
    let client_nonce = random_nonce(32)?;
    let payload = json!({
        "client_nonce": client_nonce,
        "client_proof": proxy_client_proof(proxy_server_secret, &client_nonce)
    });
    let url = proxy_url(&["api", "v1", "auth", "session"])?;
    let response = context
        .client
        .post(url)
        .header("Accept", "application/json")
        .header("Content-Type", "application/json")
        .header("User-Agent", PROXY_USER_AGENT)
        .json(&payload)
        .send()
        .await
        .map_err(hdhive_request_error)?;
    let envelope = envelope_result(parse_hdhive_response(response).await?)?;
    let session_id = required_text(&envelope.data, "session_id")?.to_string();
    let server_nonce = required_text(&envelope.data, "server_nonce")?.to_string();
    let server_proof = required_text(&envelope.data, "server_proof")?.to_ascii_lowercase();
    let expected_proof = proxy_server_proof(proxy_server_secret, &server_nonce);
    if server_proof.len() != expected_proof.len()
        || !bool::from(server_proof.as_bytes().ct_eq(expected_proof.as_bytes()))
    {
        return Err("HDHive 服务端身份校验失败".to_string());
    }
    Ok(ProxySession {
        id: session_id,
        key: proxy_session_key(proxy_server_secret, &client_nonce, &server_nonce),
        sequence: 0,
    })
}

fn proxy_url(segments: &[&str]) -> Result<Url, String> {
    let mut url = Url::parse(PROXY_BASE_URL).map_err(|_| "HDHive 服务地址无效".to_string())?;
    url.path_segments_mut()
        .map_err(|_| "HDHive 服务地址无效".to_string())?
        .clear()
        .extend(segments.iter().copied());
    Ok(url)
}

fn request_uri(url: &Url) -> String {
    match url.query() {
        Some(query) => format!("{}?{query}", url.path()),
        None => url.path().to_string(),
    }
}

fn proxy_session_key(secret: &str, client_nonce: &str, server_nonce: &str) -> Vec<u8> {
    let salt = format!(
        "hdhive-openproxy-session:{}:{}",
        client_nonce.trim(),
        server_nonce.trim()
    );
    let prk = hmac_raw(salt.as_bytes(), secret.trim().as_bytes());
    hmac_raw(&prk, b"hdhive-openproxy-session-key\x01")
}

fn proxy_client_proof(secret: &str, client_nonce: &str) -> String {
    hmac_hex(
        secret.as_bytes(),
        format!("hdhive-openproxy-proof\nclient\n{}", client_nonce.trim()).as_bytes(),
    )
}

fn proxy_server_proof(secret: &str, server_nonce: &str) -> String {
    hmac_hex(
        secret.as_bytes(),
        format!("hdhive-openproxy-proof\nserver\n{}", server_nonce.trim()).as_bytes(),
    )
}

fn proxy_request_signature(
    session_key: &[u8],
    method: &str,
    request_uri: &str,
    session_id: &str,
    sequence: &str,
    body_hash: &str,
    user_key: &str,
) -> String {
    let signing_text = [
        method.trim().to_ascii_uppercase(),
        request_uri.trim().to_string(),
        session_id.trim().to_string(),
        sequence.trim().to_string(),
        body_hash.trim().to_string(),
        user_key.trim().to_string(),
    ]
    .join("\n");
    hmac_hex(session_key, signing_text.as_bytes())
}

fn hmac_raw(key: &[u8], payload: &[u8]) -> Vec<u8> {
    let mut mac = HmacSha256::new_from_slice(key).expect("HMAC accepts any key length");
    mac.update(payload);
    mac.finalize().into_bytes().to_vec()
}

fn hmac_hex(key: &[u8], payload: &[u8]) -> String {
    hex::encode(hmac_raw(key, payload))
}

fn sha256_hex(value: &[u8]) -> String {
    hex::encode(Sha256::digest(value))
}

fn random_nonce(bytes: usize) -> Result<String, String> {
    let mut value = vec![0_u8; bytes];
    OsRng
        .try_fill_bytes(&mut value)
        .map_err(|error| format!("生成 HDHive 登录状态失败: {error}"))?;
    Ok(URL_SAFE_NO_PAD.encode(value))
}

fn validate_callback_url(value: &str) -> Result<Url, String> {
    let url = Url::parse(value).map_err(|_| "插件登录回调地址无效".to_string())?;
    if !matches!(url.scheme(), "http" | "https")
        || url.host_str().is_none()
        || !url.username().is_empty()
        || url.password().is_some()
        || url.fragment().is_some()
        || url.path() != "/api/plugins/hdhive/oauth/callback"
    {
        return Err("插件登录回调地址无效".to_string());
    }
    Ok(url)
}

fn safe_http_url(value: &str) -> bool {
    Url::parse(value)
        .ok()
        .is_some_and(|url| matches!(url.scheme(), "http" | "https") && url.host_str().is_some())
}

fn credential(context: &PluginContext) -> Result<Credential, String> {
    let lock = lock_auth_state(&context.data_dir)?;
    let auth = read_auth_state(&context.data_dir).unwrap_or_default();
    drop(lock);
    if auth.user_id.trim().is_empty() || auth.proxy_user_key.trim().is_empty() {
        return Err("HDHive 尚未登录，请先完成浏览器授权".to_string());
    }
    Ok(Credential {
        user_id: auth.user_id,
        proxy_user_key: auth.proxy_user_key,
        display_name: if auth.display_name.trim().is_empty() {
            "HDHive 用户".to_string()
        } else {
            auth.display_name
        },
    })
}

fn update_auth_state<T>(
    data_dir: &Path,
    mutate: impl FnOnce(&mut AuthState) -> Result<T, String>,
) -> Result<T, String> {
    let lock = lock_auth_state(data_dir)?;
    let mut auth = read_auth_state(data_dir).unwrap_or_default();
    let output = mutate(&mut auth)?;
    write_auth_state(data_dir, &auth)?;
    drop(lock);
    Ok(output)
}

fn lock_auth_state(data_dir: &Path) -> Result<File, String> {
    let file = secure_open(&data_dir.join(AUTH_LOCK_FILE))?;
    file.lock_exclusive()
        .map_err(|error| format!("锁定 HDHive 授权状态失败: {error}"))?;
    Ok(file)
}

fn read_auth_state(data_dir: &Path) -> Option<AuthState> {
    let raw = fs::read(data_dir.join(AUTH_STATE_FILE)).ok()?;
    serde_json::from_slice(&raw).ok()
}

fn write_auth_state(data_dir: &Path, state: &AuthState) -> Result<(), String> {
    let raw = serde_json::to_vec(state)
        .map_err(|error| format!("序列化 HDHive 授权状态失败: {error}"))?;
    atomic_secure_write(&data_dir.join(AUTH_STATE_FILE), &raw)
}

async fn parse_hdhive_response(response: Response) -> Result<ApiEnvelope, String> {
    let status = response.status().as_u16();
    let retry_after = response
        .headers()
        .get(RETRY_AFTER)
        .and_then(|value| value.to_str().ok())
        .map(str::to_string);
    if response
        .content_length()
        .is_some_and(|length| length > MAX_RESPONSE_BYTES)
    {
        return Err("HDHive 响应超过 2 MB 上限".to_string());
    }
    let bytes = response
        .bytes()
        .await
        .map_err(|error| format!("读取 HDHive 响应失败: {error}"))?;
    if bytes.len() as u64 > MAX_RESPONSE_BYTES {
        return Err("HDHive 响应超过 2 MB 上限".to_string());
    }
    let value: Value = serde_json::from_slice(&bytes)
        .map_err(|_| format!("HDHive 返回了无效 JSON (HTTP {status})"))?;
    Ok(ApiEnvelope {
        status,
        success: value
            .get("success")
            .and_then(Value::as_bool)
            .unwrap_or(status < 400),
        code: first_text(&value, &["code"]).unwrap_or_default(),
        message: first_text(&value, &["message"]).unwrap_or_default(),
        description: first_text(&value, &["description"]).unwrap_or_default(),
        data: value.get("data").cloned().unwrap_or_else(|| value.clone()),
        retry_after,
    })
}

fn envelope_result(envelope: ApiEnvelope) -> Result<ApiEnvelope, String> {
    if envelope.success && envelope.status < 400 {
        return Ok(envelope);
    }
    let detail = if !envelope.description.is_empty() {
        envelope.description.clone()
    } else if !envelope.message.is_empty() {
        envelope.message.clone()
    } else {
        "未知错误".to_string()
    };
    let message = match envelope.code.as_str() {
        "OPENAPI_REAUTH_REQUIRED" | "INVALID_OPENAPI_USER_TOKEN" => {
            "HDHive 登录已失效，请重新登录".to_string()
        }
        "SCOPE_NOT_ALLOWED" | "USER_SCOPE_NOT_ALLOWED" => {
            format!("HDHive 授权缺少所需权限: {detail}")
        }
        "INSUFFICIENT_POINTS" => "HDHive 积分不足，无法解锁该资源".to_string(),
        "GLOBAL_RATE_LIMIT_EXCEEDED"
        | "OPENAPI_COOLDOWN"
        | "APP_RATE_LIMIT_EXCEEDED"
        | "USER_RATE_LIMIT_EXCEEDED" => format!(
            "HDHive 请求频率受限{}",
            envelope
                .retry_after
                .as_deref()
                .map(|seconds| format!("，请在 {seconds} 秒后重试"))
                .unwrap_or_default()
        ),
        _ => format!(
            "HDHive 请求失败 (HTTP {}{}): {detail}",
            envelope.status,
            if envelope.code.is_empty() {
                String::new()
            } else {
                format!(", {}", envelope.code)
            }
        ),
    };
    Err(message)
}

async fn parse_mediary_response(response: Response) -> Result<Value, String> {
    let status = response.status();
    if response
        .content_length()
        .is_some_and(|length| length > MAX_RESPONSE_BYTES)
    {
        return Err("Mediary API 响应超过 2 MB 上限".to_string());
    }
    let bytes = response
        .bytes()
        .await
        .map_err(|error| format!("读取 Mediary API 响应失败: {error}"))?;
    if bytes.len() as u64 > MAX_RESPONSE_BYTES {
        return Err("Mediary API 响应超过 2 MB 上限".to_string());
    }
    let value: Value = serde_json::from_slice(&bytes)
        .map_err(|_| format!("Mediary API 返回了无效 JSON (HTTP {})", status.as_u16()))?;
    if status.is_success() {
        return Ok(value);
    }
    let message = first_text(&value, &["error", "message"]).unwrap_or_else(|| "未知错误".into());
    Err(format!("Mediary API 错误 ({}): {message}", status.as_u16()))
}

fn update_status_data(
    context: &PluginContext,
    account: Option<&Value>,
    message: &str,
    gained_points: i64,
    checked_in: bool,
) -> Result<(), String> {
    let path = context.data_dir.join(STATUS_FILE);
    let mut status = fs::read(&path)
        .ok()
        .and_then(|raw| serde_json::from_slice::<StatusData>(&raw).ok())
        .unwrap_or_default();
    if let Some(account) = account {
        if let Some(username) = first_text(account, &["username", "name", "nickname"]) {
            status.summary.insert("username".into(), json!(username));
        }
        if let Some(points) = first_i64(account, &["points", "point"]) {
            status.summary.insert("points".into(), json!(points));
        }
    }
    status.summary.insert(
        "checked_in_today".into(),
        json!(if checked_in { "已签到" } else { "未签到" }),
    );
    let created_at: DateTime<Local> = Local::now();
    status.summary.insert(
        "last_checkin_at".into(),
        json!(created_at.format("%Y-%m-%d %H:%M:%S").to_string()),
    );
    status.items.insert(
        0,
        json!({
            "message": message,
            "points": gained_points,
            "created_at": created_at.to_rfc3339()
        }),
    );
    status.items.truncate(30);
    let raw = serde_json::to_vec_pretty(&status)
        .map_err(|error| format!("序列化 HDHive 状态失败: {error}"))?;
    atomic_secure_write(&path, &raw)
}

fn locally_checked_in_today(data_dir: &Path) -> bool {
    let status = fs::read(data_dir.join(STATUS_FILE))
        .ok()
        .and_then(|raw| serde_json::from_slice::<StatusData>(&raw).ok());
    let today = Local::now().format("%Y-%m-%d").to_string();
    status
        .as_ref()
        .is_some_and(|status| status_checked_in_on_date(status, &today))
}

fn status_checked_in_on_date(status: &StatusData, date: &str) -> bool {
    status
        .summary
        .get("checked_in_today")
        .and_then(Value::as_str)
        == Some("已签到")
        && status
            .summary
            .get("last_checkin_at")
            .and_then(Value::as_str)
            .is_some_and(|value| value.starts_with(date))
}

fn merge_object(target: &mut Map<String, Value>, value: Value) {
    if let Value::Object(source) = value {
        target.extend(source);
    }
}

fn atomic_secure_write(path: &Path, content: &[u8]) -> Result<(), String> {
    let parent = path
        .parent()
        .ok_or_else(|| "HDHive 数据路径无效".to_string())?;
    fs::create_dir_all(parent).map_err(|error| format!("创建 HDHive 数据目录失败: {error}"))?;
    secure_directory(parent)?;
    let temp = path.with_extension(format!("tmp-{}", std::process::id()));
    let mut file = secure_create(&temp)?;
    file.write_all(content)
        .and_then(|_| file.sync_all())
        .map_err(|error| format!("写入 HDHive 数据失败: {error}"))?;
    fs::rename(&temp, path).map_err(|error| format!("替换 HDHive 数据失败: {error}"))?;
    Ok(())
}

fn secure_open(path: &Path) -> Result<File, String> {
    let mut options = OpenOptions::new();
    options.read(true).write(true).create(true);
    set_private_mode(&mut options);
    options
        .open(path)
        .map_err(|error| format!("打开 HDHive 状态文件失败: {error}"))
}

fn secure_create(path: &Path) -> Result<File, String> {
    let mut options = OpenOptions::new();
    options.write(true).create(true).truncate(true);
    set_private_mode(&mut options);
    options
        .open(path)
        .map_err(|error| format!("创建 HDHive 状态文件失败: {error}"))
}

#[cfg(unix)]
fn set_private_mode(options: &mut OpenOptions) {
    use std::os::unix::fs::OpenOptionsExt;
    options.mode(0o600);
}

#[cfg(not(unix))]
fn set_private_mode(_options: &mut OpenOptions) {}

#[cfg(unix)]
fn secure_directory(path: &Path) -> Result<(), String> {
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(path, fs::Permissions::from_mode(0o700))
        .map_err(|error| format!("设置 HDHive 数据目录权限失败: {error}"))
}

#[cfg(not(unix))]
fn secure_directory(_path: &Path) -> Result<(), String> {
    Ok(())
}

fn required_env(key: &str) -> Result<String, String> {
    env::var(key)
        .ok()
        .filter(|value| !value.trim().is_empty())
        .ok_or_else(|| format!("缺少 {key} 环境变量"))
}

fn required_text<'a>(value: &'a Value, key: &str) -> Result<&'a str, String> {
    value
        .get(key)
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .ok_or_else(|| format!("缺少 {key} 参数"))
}

fn positive_i64(value: &Value, key: &str) -> Result<i64, String> {
    value
        .get(key)
        .and_then(|value| {
            value
                .as_i64()
                .or_else(|| value.as_str()?.parse::<i64>().ok())
        })
        .filter(|value| *value > 0)
        .ok_or_else(|| format!("{key} 必须是正整数"))
}

fn normalize_media_type(value: &str, allow_multi: bool) -> Result<&'static str, String> {
    match value.trim().to_ascii_lowercase().as_str() {
        "movie" => Ok("movie"),
        "tv" => Ok("tv"),
        "multi" if allow_multi => Ok("multi"),
        _ => Err("媒体类型必须是 movie 或 tv".to_string()),
    }
}

fn first_text(value: &Value, keys: &[&str]) -> Option<String> {
    for key in keys {
        let Some(field) = value.get(key) else {
            continue;
        };
        if let Some(text) = field
            .as_str()
            .map(str::trim)
            .filter(|text| !text.is_empty())
        {
            return Some(text.to_string());
        }
        if field.is_number() || field.is_boolean() {
            return Some(field.to_string());
        }
    }
    None
}

fn first_i64(value: &Value, keys: &[&str]) -> Option<i64> {
    for key in keys {
        if let Some(field) = value.get(key)
            && let Some(number) = field
                .as_i64()
                .or_else(|| field.as_str()?.trim().parse::<i64>().ok())
        {
            return Some(number);
        }
    }
    None
}

fn first_bool(value: &Value, keys: &[&str]) -> Option<bool> {
    for key in keys {
        if let Some(field) = value.get(key) {
            if let Some(boolean) = field.as_bool() {
                return Some(boolean);
            }
            if let Some(text) = field.as_str() {
                match text.trim().to_ascii_lowercase().as_str() {
                    "true" | "1" | "yes" => return Some(true),
                    "false" | "0" | "no" => return Some(false),
                    _ => {}
                }
            }
        }
    }
    None
}

fn push_metadata(metadata: &mut Vec<Value>, resource: &Value, key: &str, label: &str) {
    if let Some(value) = display_value(resource.get(key)) {
        metadata.push(json!({"label": label, "value": value}));
    }
}

fn display_value(value: Option<&Value>) -> Option<String> {
    match value? {
        Value::String(value) => (!value.trim().is_empty()).then(|| value.trim().to_string()),
        Value::Number(value) => Some(value.to_string()),
        Value::Array(values) => {
            let joined = values
                .iter()
                .filter_map(|value| value.as_str().map(str::trim))
                .filter(|value| !value.is_empty())
                .collect::<Vec<_>>()
                .join(" / ");
            (!joined.is_empty()).then_some(joined)
        }
        _ => None,
    }
}

fn setting_bool(settings: &Map<String, Value>, key: &str, default: bool) -> bool {
    settings
        .get(key)
        .and_then(Value::as_bool)
        .unwrap_or(default)
}

fn manual_checkin_gambler_mode(settings: &Map<String, Value>, payload: &Value) -> bool {
    payload
        .get("gambler_checkin")
        .and_then(Value::as_bool)
        .unwrap_or_else(|| setting_bool(settings, "gambler_checkin", false))
}

fn mediary_request_error(error: reqwest::Error) -> String {
    if error.is_timeout() {
        "Mediary TMDB 搜索超时".to_string()
    } else if error.is_connect() {
        format!("无法连接 Mediary API: {error}")
    } else {
        format!("Mediary API 请求失败: {error}")
    }
}

fn hdhive_request_error(error: reqwest::Error) -> String {
    if error.is_timeout() {
        "HDHive 请求超时".to_string()
    } else if error.is_connect() {
        format!("无法连接 HDHive: {error}")
    } else {
        format!("HDHive 请求失败: {error}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn proxy_crypto_matches_reference_vectors() {
        let session_key = proxy_session_key(
            "test-proxy-secret",
            "client-nonce-for-hkdf",
            "server-nonce-for-hkdf",
        );
        assert_eq!(
            hex::encode(&session_key),
            "735408a4dffc0badeb9d6cea807fed0846d8c981b6bfc428c68373d60b2acf28"
        );
        let signature = proxy_request_signature(
            &session_key,
            "POST",
            "/api/v1/oauth/start?callback=https%3A%2F%2Fexample.test%2Fcb",
            "probe-session",
            "1",
            &sha256_hex(&[]),
            "",
        );
        assert_eq!(
            signature,
            "2ac833dec76009c9da041a57c44c7782f85d9e674fdb3a7c5699a33127fd7392"
        );
    }

    #[test]
    fn random_nonce_uses_url_safe_expected_length() {
        assert_eq!(random_nonce(32).unwrap().len(), 43);
    }

    #[test]
    fn callback_url_is_limited_to_this_plugin() {
        assert!(
            validate_callback_url("http://localhost:8118/api/plugins/hdhive/oauth/callback")
                .is_ok()
        );
        assert!(validate_callback_url("https://example.test/other").is_err());
        assert!(validate_callback_url("javascript:alert(1)").is_err());
    }

    #[test]
    fn authorization_response_uses_browser_login_action() {
        let response = authorization_response();
        assert_eq!(response["items"][0]["actions"][0]["type"], "browser_auth");
        assert_eq!(response["items"][0]["actions"][0]["action"], "authorize");
    }

    #[test]
    fn checkin_keeps_the_documented_openapi_path() {
        assert_eq!(
            logical_open_api_path("/api/open/checkin").unwrap(),
            "/checkin"
        );
    }

    #[test]
    fn successful_checkin_notification_contains_result_and_default_image() {
        let notification = successful_checkin_notification(
            &CheckinOutcome {
                response: json!({}),
                checked_in: true,
                gained_points: 12,
                message: "签到成功".to_string(),
                username: Some("tester".to_string()),
                current_points: Some(345),
            },
            true,
        );
        assert_eq!(notification["title"], "HDHive 签到成功");
        let content = notification["content"].as_str().unwrap();
        assert!(content.contains("账号：tester"));
        assert!(content.contains("结果：签到成功"));
        assert!(content.contains("签到方式：赌狗签到"));
        assert!(content.contains("本次积分：12"));
        assert!(content.contains("当前积分：345"));
        assert_eq!(notification["image_url"], NOTIFICATION_IMAGE_URL);
    }

    #[test]
    fn failed_checkin_notification_preserves_failure_reason() {
        let notification = failed_checkin_notification("授权已过期", false);
        assert_eq!(notification["title"], "HDHive 签到任务失败");
        assert_eq!(
            notification["content"],
            "签到方式：普通签到\n失败原因：授权已过期"
        );
        assert_eq!(notification["image_url"], NOTIFICATION_IMAGE_URL);
    }

    #[test]
    fn transfer_notification_contains_resource_pan_and_result() {
        let notification =
            transfer_submitted_notification("重器 (2026)", "115", "115 分享已提交转存整理");
        assert_eq!(notification["title"], "📥 HDHive 转存已提交");
        assert_eq!(
            notification["content"],
            "资源：重器 (2026)\n网盘：115\n结果：115 分享已提交转存整理"
        );
        assert_eq!(notification["image_url"], NOTIFICATION_IMAGE_URL);
    }

    #[test]
    fn local_checkin_status_only_applies_to_the_same_day() {
        let mut status = StatusData::default();
        status
            .summary
            .insert("checked_in_today".into(), json!("已签到"));
        status
            .summary
            .insert("last_checkin_at".into(), json!("2026-08-10 09:00:00"));

        assert!(status_checked_in_on_date(&status, "2026-08-10"));
        assert!(!status_checked_in_on_date(&status, "2026-08-11"));
    }

    #[test]
    fn manual_checkin_uses_the_current_form_selection() {
        let mut settings = Map::new();
        settings.insert("gambler_checkin".into(), json!(false));
        assert!(manual_checkin_gambler_mode(
            &settings,
            &json!({"gambler_checkin": true})
        ));

        settings.insert("gambler_checkin".into(), json!(true));
        assert!(!manual_checkin_gambler_mode(
            &settings,
            &json!({"gambler_checkin": false})
        ));
    }

    #[test]
    fn extracts_resources_from_supported_envelopes() {
        let direct = json!([{"slug": "a"}]);
        let nested = json!({"resources": [{"slug": "b"}]});
        assert_eq!(extract_resource_items(&direct)[0]["slug"], "a");
        assert_eq!(extract_resource_items(&nested)[0]["slug"], "b");
    }

    #[test]
    fn tmdb_result_uses_poster_overview_and_row_click() {
        let candidate = json!({
            "id": 550,
            "media_type": "movie",
            "title": "搏击俱乐部",
            "original_title": "Fight Club",
            "poster_path": "/poster.jpg",
            "overview": "一段用于验证搜索结果简介的文本。",
            "release_date": "1999-10-15",
            "vote_average": 8.4
        });
        let item = media_candidate_item(&candidate, "搏击俱乐部").unwrap();
        assert_eq!(
            item["image_url"],
            "https://image.tmdb.org/t/p/w342/poster.jpg"
        );
        assert_eq!(item["subtitle"], "一段用于验证搜索结果简介的文本。");
        assert_eq!(item["click_action"]["action"], "resources");
        assert!(item.get("actions").is_none());
    }

    #[test]
    fn paid_resource_requires_manual_confirmation() {
        let resource = json!({
            "slug": "resource-a",
            "title": "Test",
            "pan_type": "115",
            "unlock_points": 8,
            "is_unlocked": false
        });
        let item = resource_item(&resource, "movie", 550, "Test").unwrap();
        assert_eq!(item["actions"][0]["action"], "transfer");
        assert_eq!(
            item["actions"][0]["confirm"]["confirm_text"],
            "8 积分解锁并转存"
        );
    }

    #[test]
    fn host_resource_contains_transfer_contract() {
        let resource = json!({
            "slug": "resource-host",
            "title": "Test 2160p",
            "pan_type": "115",
            "share_size": "1.5 GB",
            "unlock_points": 8,
            "user": {"nickname": "publisher"},
            "created_at": "2026-08-06T12:30:00+08:00",
            "video_resolution": "2160p"
        });
        let item = host_resource_item(&resource, "movie", 550, "Test", Some(1999)).unwrap();
        assert_eq!(item["source_kind"], "plugin");
        assert_eq!(item["site_id"], -2);
        assert_eq!(item["size"], 1_610_612_736_i64);
        assert_eq!(item["uploader"], "publisher");
        assert_eq!(item["plugin_action"], "transfer");
        assert_eq!(item["plugin_payload"]["slug"], "resource-host");
        assert_eq!(item["unlock_points"], 8);
        assert_eq!(item["parsed"]["resolution"], "2160p");
    }

    #[test]
    fn parses_resource_sizes_without_guessing_unknown_units() {
        assert_eq!(parse_size_bytes("512 MB"), Some(536_870_912));
        assert_eq!(parse_size_bytes("1.25GiB"), Some(1_342_177_280));
        assert_eq!(parse_size_bytes("unknown"), None);
    }

    #[test]
    fn resource_displays_publisher_and_date_without_avatar() {
        let resource = json!({
            "slug": "resource-publisher",
            "title": "Test",
            "pan_type": "115",
            "unlock_points": 0,
            "user": {
                "nickname": "AApig",
                "avatar_url": "/uploads/avatar.png"
            },
            "created_at": "2026-08-06T12:30:00+08:00"
        });
        let item = resource_item(&resource, "movie", 550, "Test").unwrap();
        assert!(item.get("image_url").is_none());
        assert!(item.get("image_alt").is_none());
        assert_eq!(item["metadata"][0]["label"], "发布者");
        assert_eq!(item["metadata"][0]["value"], "AApig");
        assert_eq!(item["metadata"][1]["label"], "发布于");
        assert!(
            item["metadata"][1]["value"]
                .as_str()
                .unwrap()
                .starts_with("2026/08/06")
        );
    }

    #[test]
    fn changed_unlock_points_require_a_new_confirmation() {
        assert!(ensure_unlock_points_unchanged(8, 8, false).is_ok());
        assert!(ensure_unlock_points_unchanged(8, 20, true).is_ok());
        assert_eq!(
            ensure_unlock_points_unchanged(8, 20, false).unwrap_err(),
            "资源解锁积分已从 8 变为 20，请返回资源列表重新确认"
        );
    }

    #[test]
    fn free_and_unlocked_resources_offer_direct_transfer() {
        let free = json!({
            "slug": "resource-free",
            "title": "Test",
            "pan_type": "ed2k",
            "unlock_points": 0,
            "is_unlocked": false
        });
        let free_item = resource_item(&free, "movie", 550, "Test").unwrap();
        assert_eq!(free_item["actions"][0]["label"], "转存推送");
        assert!(free_item["actions"][0]["confirm"].is_null());

        let resource = json!({
            "slug": "resource-b",
            "title": "Test",
            "pan_type": "115",
            "media_url": "https://115.com/s/example",
            "is_unlocked": true
        });
        let item = resource_item(&resource, "movie", 550, "Test").unwrap();
        assert_eq!(item["actions"][0]["action"], "transfer");
        assert_eq!(item["actions"][0]["label"], "转存推送");
    }

    #[test]
    fn only_resources_supported_by_mediary_are_shown() {
        let unsupported = json!({
            "slug": "quark-resource",
            "title": "不支持的网盘资源",
            "pan_type": "quark",
            "unlock_points": 0
        });
        assert!(resource_item(&unsupported, "movie", 550, "电影").is_none());
        assert!(is_transferable_pan_type("115网盘"));
        assert!(is_transferable_pan_type("eD2k"));
        assert!(!is_transferable_pan_type("magnet"));
    }

    #[test]
    fn ed2k_uses_transfer_directory_and_schedules_move_all() {
        let payload = mediary_link_submit_payload("ed2k://|file|movie.mkv|1|HASH|/", None);
        assert_eq!(payload["offline_target"], "transfer");
        assert_eq!(payload["flowlink_move_all_delay_seconds"], 10);

        let share = mediary_link_submit_payload("https://115.com/s/example", None);
        assert!(share.get("offline_target").is_none());
        assert!(share.get("flowlink_move_all_delay_seconds").is_none());
    }
}
