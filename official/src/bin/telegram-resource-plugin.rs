use base64::{Engine, engine::general_purpose::STANDARD as BASE64_STANDARD};
use chrono::Utc;
use fs2::FileExt;
use grammers_client::{
    Client as TelegramClient, SignInError,
    client::{PasswordToken, UpdateStream, UpdatesConfiguration},
    media::Media,
    message::Message,
    peer::Peer,
    update::Update,
};
use grammers_mtsender::{ConnectionParams, InvocationError, SenderPool, SenderPoolFatHandle};
use grammers_session::{Session, storages::SqliteSession, types::PeerId};
use grammers_tl_types as tl;
use percent_encoding::percent_decode_str;
use regex::Regex;
use reqwest::{Client as HttpClient, Url};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};
use sha2::{Digest, Sha256};
use std::{
    collections::{HashMap, HashSet},
    env, fs,
    fs::{File, OpenOptions},
    io::{self, Read},
    net::{IpAddr, SocketAddr},
    path::{Path, PathBuf},
    sync::{Arc, OnceLock},
    time::Duration,
};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::{TcpListener, TcpStream},
    task::{JoinHandle, JoinSet},
};

#[cfg(unix)]
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};

const SESSION_FILE: &str = "telegram.session";
const LOGIN_STATE_FILE: &str = "login-state.json";
const ACTION_LOCK_FILE: &str = ".telegram.lock";
const LOGIN_TTL_SECONDS: i64 = 15 * 60;
const MAX_CHANNELS: usize = 50;
const MAX_DESCRIPTION_CHARS: usize = 800;
const SECRET_PLACEHOLDER: &str = "******";
const FLOWLINK_MOVE_ALL_DELAY_SECONDS: u64 = 10;
const MAX_PROXY_RESPONSE_HEADER_BYTES: usize = 16 * 1024;

#[derive(Clone)]
struct PluginContext {
    settings: Map<String, Value>,
    http: HttpClient,
    mediary_api_url: String,
    mediary_token: String,
    data_dir: PathBuf,
}

#[derive(Debug, Clone)]
struct TelegramCredentials {
    api_id: i32,
    api_hash: String,
}

struct TelegramConnection {
    client: TelegramClient,
    session: Arc<SqliteSession>,
    updates: Option<UpdateStream>,
    handle: SenderPoolFatHandle,
    runner: JoinHandle<()>,
    _http_proxy_bridge: Option<HttpConnectBridge>,
}

#[derive(Clone)]
struct HttpProxyConfig {
    host: String,
    port: u16,
    authorization: Option<String>,
}

struct HttpConnectBridge {
    address: SocketAddr,
    runner: JoinHandle<()>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct LoginState {
    phone: String,
    phone_code_hash: String,
    phase: LoginPhase,
    expires_at: i64,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
enum LoginPhase {
    Code,
    Password,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ResourceLinkKind {
    Share115,
    Magnet,
    Ed2k,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct ResourceLink {
    kind: ResourceLinkKind,
    value: String,
    key: String,
}

#[derive(Debug)]
struct ChannelSearchResult {
    items: Vec<Value>,
    searched: bool,
}

#[derive(Debug, Clone)]
struct TelegramMediaMetadata {
    title: String,
    tmdb_id: i64,
    media_type: String,
    year: Option<i32>,
    season: Option<i32>,
    episode: Option<i32>,
    end_episode: Option<i32>,
    quality: String,
    size: i64,
}

#[derive(Debug, Clone)]
struct DirectResource {
    payload: Value,
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
    if action.trim().is_empty() {
        return run_realtime_reporter(&context).await;
    }
    let _lock = lock_action(&context.data_dir)?;
    let payload = read_payload()?;
    let output = match action.as_str() {
        "status" => account_status(&context).await?,
        "send_code" => send_login_code(&context, &payload).await?,
        "complete_login" => complete_login(&context, &payload).await?,
        "logout" => logout(&context).await?,
        "resource_search" => resource_search(&context, &payload).await?,
        "transfer" => transfer_resource(&context, &payload).await?,
        _ => return Err(format!("Telegram 插件不支持动作: {action}")),
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
            .map_err(|error| format!("创建 Telegram 插件数据目录失败: {error}"))?;
        secure_directory(&data_dir)?;
        let http = HttpClient::builder()
            .no_proxy()
            .connect_timeout(Duration::from_secs(8))
            .timeout(Duration::from_secs(70))
            .build()
            .map_err(|error| format!("创建 HTTP 客户端失败: {error}"))?;
        Ok(Self {
            settings,
            http,
            mediary_api_url,
            mediary_token,
            data_dir,
        })
    }
}

impl TelegramConnection {
    async fn connect(
        context: &PluginContext,
        credentials: &TelegramCredentials,
    ) -> Result<Self, String> {
        let session_path = context.data_dir.join(SESSION_FILE);
        let session = Arc::new(
            SqliteSession::open(&session_path)
                .await
                .map_err(|error| format!("打开 Telegram Session 失败: {error}"))?,
        );
        secure_file_if_exists(&session_path)?;
        let (connection_params, http_proxy_bridge) = telegram_connection_params(context).await?;
        let SenderPool {
            runner,
            handle,
            updates,
        } = SenderPool::with_configuration(
            Arc::clone(&session),
            credentials.api_id,
            connection_params,
        );
        let client = TelegramClient::new(handle.clone());
        let runner = tokio::spawn(runner.run());
        let updates = client
            .stream_updates(
                updates,
                UpdatesConfiguration {
                    catch_up: true,
                    update_queue_limit: Some(1_000),
                },
            )
            .await
            .map_err(|error| format!("创建 Telegram 实时消息流失败: {error}"))?;
        Ok(Self {
            client,
            session,
            updates: Some(updates),
            handle,
            runner,
            _http_proxy_bridge: http_proxy_bridge,
        })
    }

    async fn ensure_authorized(&self) -> Result<(), String> {
        match self.client.is_authorized().await {
            Ok(true) => Ok(()),
            Ok(false) => Err("Telegram 尚未登录，请先在插件配置中完成用户登录".to_string()),
            Err(error) => Err(format_telegram_error("检查登录状态失败", &error)),
        }
    }
}

impl Drop for TelegramConnection {
    fn drop(&mut self) {
        let _ = self.handle.quit();
        self.runner.abort();
    }
}

impl HttpConnectBridge {
    async fn start(proxy_url: &str) -> Result<Self, String> {
        let config = parse_http_proxy(proxy_url)?;
        let listener = TcpListener::bind(("127.0.0.1", 0))
            .await
            .map_err(|error| format!("创建 Telegram HTTP 代理桥接失败: {error}"))?;
        let address = listener
            .local_addr()
            .map_err(|error| format!("读取 Telegram HTTP 代理桥接地址失败: {error}"))?;
        let runner = tokio::spawn(run_http_connect_bridge(listener, config));
        Ok(Self { address, runner })
    }

    fn socks_url(&self) -> String {
        format!("socks5://{}", self.address)
    }
}

impl Drop for HttpConnectBridge {
    fn drop(&mut self) {
        self.runner.abort();
    }
}

async fn telegram_connection_params(
    context: &PluginContext,
) -> Result<(ConnectionParams, Option<HttpConnectBridge>), String> {
    let mut params = ConnectionParams::default();
    let Some(proxy_url) = configured_proxy_url(context).await? else {
        return Ok((params, None));
    };
    let parsed = Url::parse(&proxy_url).map_err(|_| "Mediary 系统代理 URL 无效".to_string())?;
    match parsed.scheme() {
        "socks5" | "socks5h" => {
            params.proxy_url = Some(proxy_url.replacen(parsed.scheme(), "socks5", 1));
            Ok((params, None))
        }
        "http" => {
            let bridge = HttpConnectBridge::start(&proxy_url).await?;
            params.proxy_url = Some(bridge.socks_url());
            Ok((params, Some(bridge)))
        }
        "https" => Err("Telegram 暂不支持 HTTPS 代理端点，请使用 http:// 或 socks5://".to_string()),
        _ => Err("Telegram 代理仅支持 http://、socks5:// 或 socks5h://".to_string()),
    }
}

async fn configured_proxy_url(context: &PluginContext) -> Result<Option<String>, String> {
    if let Ok(value) = env::var("MEDIARY_PLUGIN_PROXY_URL")
        && !value.trim().is_empty()
    {
        return Ok(Some(value.trim().to_string()));
    }
    let response = context
        .http
        .get(format!("{}/settings", context.mediary_api_url))
        .bearer_auth(&context.mediary_token)
        .send()
        .await
        .map_err(|error| format!("读取 Mediary 系统代理失败: {error}"))?;
    let status = response.status();
    let body = response
        .text()
        .await
        .map_err(|error| format!("读取 Mediary 系统代理响应失败: {error}"))?;
    if !status.is_success() {
        return Err(if status == reqwest::StatusCode::FORBIDDEN {
            "TG搜索插件缺少 settings:read 权限，请从商店更新后重新打开配置".to_string()
        } else {
            format!("读取 Mediary 系统代理失败: HTTP {}", status.as_u16())
        });
    }
    let settings: Value = serde_json::from_str(&body)
        .map_err(|error| format!("解析 Mediary 系统代理响应失败: {error}"))?;
    Ok(settings
        .get("tmdb_proxy_url")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_string))
}

fn parse_http_proxy(proxy_url: &str) -> Result<HttpProxyConfig, String> {
    let parsed = Url::parse(proxy_url).map_err(|_| "Mediary HTTP 代理 URL 无效".to_string())?;
    if parsed.scheme() != "http" {
        return Err("HTTP CONNECT 桥接只接受 http:// 代理".to_string());
    }
    let host = parsed
        .host_str()
        .filter(|value| !value.is_empty())
        .ok_or_else(|| "Mediary HTTP 代理缺少主机".to_string())?
        .to_string();
    let port = parsed
        .port_or_known_default()
        .ok_or_else(|| "Mediary HTTP 代理缺少端口".to_string())?;
    let authorization = if parsed.username().is_empty() {
        None
    } else {
        let credentials = format!("{}:{}", parsed.username(), parsed.password().unwrap_or(""));
        Some(format!("Basic {}", BASE64_STANDARD.encode(credentials)))
    };
    Ok(HttpProxyConfig {
        host,
        port,
        authorization,
    })
}

async fn run_http_connect_bridge(listener: TcpListener, config: HttpProxyConfig) {
    let mut connections = JoinSet::new();
    loop {
        tokio::select! {
            accepted = listener.accept() => match accepted {
                Ok((stream, _)) => {
                    let config = config.clone();
                    connections.spawn(async move {
                        if let Err(error) = proxy_socks_client(stream, config).await {
                            eprintln!("Telegram HTTP 代理桥接连接失败: {error}");
                        }
                    });
                }
                Err(error) => {
                    eprintln!("Telegram HTTP 代理桥接监听失败: {error}");
                    break;
                }
            },
            Some(_) = connections.join_next(), if !connections.is_empty() => {}
        }
    }
}

async fn proxy_socks_client(
    mut client: TcpStream,
    config: HttpProxyConfig,
) -> Result<(), io::Error> {
    let mut greeting = [0u8; 2];
    client.read_exact(&mut greeting).await?;
    if greeting[0] != 5 || greeting[1] == 0 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "无效的 SOCKS5 握手",
        ));
    }
    let mut methods = vec![0u8; greeting[1] as usize];
    client.read_exact(&mut methods).await?;
    if !methods.contains(&0) {
        client.write_all(&[5, 0xff]).await?;
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "SOCKS5 客户端不支持免认证",
        ));
    }
    client.write_all(&[5, 0]).await?;

    let mut request = [0u8; 4];
    client.read_exact(&mut request).await?;
    if request[0] != 5 || request[1] != 1 {
        send_socks_failure(&mut client, 7).await;
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "仅支持 SOCKS5 CONNECT",
        ));
    }
    let target = read_socks_target(&mut client, request[3]).await?;
    let mut upstream = match TcpStream::connect((config.host.as_str(), config.port)).await {
        Ok(stream) => stream,
        Err(error) => {
            send_socks_failure(&mut client, 1).await;
            return Err(error);
        }
    };
    let request = http_connect_request(&target, config.authorization.as_deref());
    upstream.write_all(request.as_bytes()).await?;
    let status = read_http_connect_status(&mut upstream).await?;
    if !(200..300).contains(&status) {
        send_socks_failure(&mut client, 1).await;
        return Err(io::Error::new(
            io::ErrorKind::ConnectionRefused,
            format!("HTTP 代理拒绝 CONNECT，状态码 {status}"),
        ));
    }
    client.write_all(&[5, 0, 0, 1, 0, 0, 0, 0, 0, 0]).await?;
    tokio::io::copy_bidirectional(&mut client, &mut upstream).await?;
    Ok(())
}

async fn read_socks_target(client: &mut TcpStream, address_type: u8) -> Result<String, io::Error> {
    let host = match address_type {
        1 => {
            let mut bytes = [0u8; 4];
            client.read_exact(&mut bytes).await?;
            IpAddr::from(bytes).to_string()
        }
        4 => {
            let mut bytes = [0u8; 16];
            client.read_exact(&mut bytes).await?;
            format!("[{}]", IpAddr::from(bytes))
        }
        3 => {
            let length = client.read_u8().await? as usize;
            if length == 0 {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "SOCKS5 目标域名为空",
                ));
            }
            let mut bytes = vec![0u8; length];
            client.read_exact(&mut bytes).await?;
            String::from_utf8(bytes)
                .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "SOCKS5 目标域名无效"))?
        }
        _ => {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "SOCKS5 地址类型无效",
            ));
        }
    };
    let port = client.read_u16().await?;
    Ok(format!("{host}:{port}"))
}

fn http_connect_request(target: &str, authorization: Option<&str>) -> String {
    let authorization = authorization
        .map(|value| format!("Proxy-Authorization: {value}\r\n"))
        .unwrap_or_default();
    format!(
        "CONNECT {target} HTTP/1.1\r\nHost: {target}\r\nProxy-Connection: Keep-Alive\r\n{authorization}\r\n"
    )
}

async fn read_http_connect_status(stream: &mut TcpStream) -> Result<u16, io::Error> {
    let mut header = Vec::new();
    let mut byte = [0u8; 1];
    while header.len() < MAX_PROXY_RESPONSE_HEADER_BYTES {
        if stream.read(&mut byte).await? == 0 {
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "HTTP 代理提前关闭连接",
            ));
        }
        header.push(byte[0]);
        if header.ends_with(b"\r\n\r\n") {
            let text = String::from_utf8_lossy(&header);
            return text
                .lines()
                .next()
                .and_then(|line| line.split_whitespace().nth(1))
                .and_then(|value| value.parse::<u16>().ok())
                .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "HTTP 代理响应无效"));
        }
    }
    Err(io::Error::new(
        io::ErrorKind::InvalidData,
        "HTTP 代理响应头过大",
    ))
}

async fn send_socks_failure(client: &mut TcpStream, code: u8) {
    let _ = client.write_all(&[5, code, 0, 1, 0, 0, 0, 0, 0, 0]).await;
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

async fn account_status(context: &PluginContext) -> Result<Value, String> {
    let credentials = credentials_from_settings(&context.settings)?;
    let connection = TelegramConnection::connect(context, &credentials).await?;
    match connection.client.is_authorized().await {
        Ok(true) => {
            let user = connection
                .client
                .get_me()
                .await
                .map_err(|error| format_telegram_error("读取 Telegram 账号失败", &error))?;
            Ok(logged_in_response(
                display_user_name(&user),
                user.username().map(str::to_string),
                configured_channels(&context.settings)?.len(),
                configured_report_channels(&context.settings)?.len(),
                "Telegram 已登录。",
            ))
        }
        Ok(false) => {
            let pending = current_login_state(&context.data_dir)?;
            Ok(pending_login_response(pending.as_ref()))
        }
        Err(error) => Err(format_telegram_error("检查 Telegram 登录状态失败", &error)),
    }
}

async fn send_login_code(context: &PluginContext, payload: &Value) -> Result<Value, String> {
    let credentials = credentials_for_action(context, payload)?;
    let phone = effective_text(context, payload, "phone")?;
    validate_phone(&phone)?;
    let connection = TelegramConnection::connect(context, &credentials).await?;
    if connection
        .client
        .is_authorized()
        .await
        .map_err(|error| format_telegram_error("检查 Telegram 登录状态失败", &error))?
    {
        return Ok(json!({"notice": "Telegram 已登录，无需重复发送验证码。"}));
    }

    let sent_code = invoke_send_code(&connection, &credentials, &phone).await?;
    let phone_code_hash = match sent_code {
        tl::enums::auth::SentCode::Code(code) => code.phone_code_hash,
        tl::enums::auth::SentCode::Success(_) => {
            return Ok(json!({"notice": "Telegram 已直接确认登录，请重新打开插件配置检查状态。"}));
        }
        tl::enums::auth::SentCode::PaymentRequired(_) => {
            return Err("Telegram 要求完成付费登录验证，请先在官方客户端处理后重试".to_string());
        }
    };
    write_login_state(
        &context.data_dir,
        &LoginState {
            phone,
            phone_code_hash,
            phase: LoginPhase::Code,
            expires_at: Utc::now().timestamp() + LOGIN_TTL_SECONDS,
        },
    )?;
    Ok(json!({
        "notice": "验证码已发送。请查看已登录的 Telegram 客户端，填写验证码后点击“完成登录”。"
    }))
}

async fn invoke_send_code(
    connection: &TelegramConnection,
    credentials: &TelegramCredentials,
    phone: &str,
) -> Result<tl::enums::auth::SentCode, String> {
    let request = || tl::functions::auth::SendCode {
        phone_number: phone.to_string(),
        api_id: credentials.api_id,
        api_hash: credentials.api_hash.clone(),
        settings: tl::types::CodeSettings {
            allow_flashcall: false,
            current_number: false,
            allow_app_hash: false,
            allow_missed_call: false,
            allow_firebase: false,
            logout_tokens: None,
            token: None,
            app_sandbox: None,
            unknown_number: false,
        }
        .into(),
    };

    match connection.client.invoke(&request()).await {
        Ok(value) => Ok(value),
        Err(InvocationError::Rpc(error)) if error.code == 303 && error.value.is_some() => {
            let old_dc = connection
                .session
                .home_dc_id()
                .map_err(|error| format!("读取 Telegram 数据中心失败: {error}"))?;
            let new_dc = error.value.unwrap() as i32;
            let _ = connection.handle.disconnect_from_dc(old_dc);
            connection
                .session
                .set_home_dc_id(new_dc)
                .await
                .map_err(|error| format!("切换 Telegram 数据中心失败: {error}"))?;
            connection
                .client
                .invoke(&request())
                .await
                .map_err(|error| format_telegram_error("发送 Telegram 验证码失败", &error))
        }
        Err(error) => Err(format_telegram_error("发送 Telegram 验证码失败", &error)),
    }
}

async fn complete_login(context: &PluginContext, payload: &Value) -> Result<Value, String> {
    let credentials = credentials_for_action(context, payload)?;
    let connection = TelegramConnection::connect(context, &credentials).await?;
    if connection
        .client
        .is_authorized()
        .await
        .map_err(|error| format_telegram_error("检查 Telegram 登录状态失败", &error))?
    {
        remove_login_state(&context.data_dir)?;
        return Ok(json!({"notice": "Telegram 已登录。"}));
    }
    let mut state = current_login_state(&context.data_dir)?
        .ok_or_else(|| "登录验证码已失效，请重新发送验证码".to_string())?;

    if state.phase == LoginPhase::Password {
        let password = action_secret(payload, "two_factor_password")?;
        complete_password_login(&connection, &password).await?;
        remove_login_state(&context.data_dir)?;
        return Ok(json!({"notice": "Telegram 登录成功。"}));
    }

    let code = action_secret(payload, "login_code")?;
    let request = tl::functions::auth::SignIn {
        phone_number: state.phone.clone(),
        phone_code_hash: state.phone_code_hash.clone(),
        phone_code: Some(code),
        email_verification: None,
    };
    match connection.client.invoke(&request).await {
        Ok(tl::enums::auth::Authorization::Authorization(_)) => {
            connection
                .client
                .get_me()
                .await
                .map_err(|error| format_telegram_error("验证 Telegram 登录失败", &error))?;
            remove_login_state(&context.data_dir)?;
            Ok(json!({"notice": "Telegram 登录成功。"}))
        }
        Ok(tl::enums::auth::Authorization::SignUpRequired(_)) => {
            Err("该手机号尚未注册 Telegram，请先使用官方客户端完成注册".to_string())
        }
        Err(error) if error.is("SESSION_PASSWORD_NEEDED") => {
            state.phase = LoginPhase::Password;
            state.expires_at = Utc::now().timestamp() + LOGIN_TTL_SECONDS;
            write_login_state(&context.data_dir, &state)?;
            if let Ok(password) = action_secret(payload, "two_factor_password") {
                complete_password_login(&connection, &password).await?;
                remove_login_state(&context.data_dir)?;
                Ok(json!({"notice": "Telegram 登录成功。"}))
            } else {
                Ok(json!({
                    "notice": "验证码已确认，账号启用了两步验证。请填写两步验证密码并再次点击“完成登录”。"
                }))
            }
        }
        Err(error) if error.is("PHONE_CODE_EXPIRED") => {
            remove_login_state(&context.data_dir)?;
            Err("Telegram 验证码已过期，请重新发送".to_string())
        }
        Err(error) if error.is("PHONE_CODE_*") => Err("Telegram 验证码不正确".to_string()),
        Err(error) => Err(format_telegram_error("Telegram 登录失败", &error)),
    }
}

async fn complete_password_login(
    connection: &TelegramConnection,
    password: &str,
) -> Result<(), String> {
    let password_info: tl::types::account::Password = connection
        .client
        .invoke(&tl::functions::account::GetPassword {})
        .await
        .map_err(|error| format_telegram_error("读取两步验证信息失败", &error))?
        .into();
    match connection
        .client
        .check_password(PasswordToken::new(password_info), password.as_bytes())
        .await
    {
        Ok(_) => Ok(()),
        Err(SignInError::InvalidPassword(_)) => Err("Telegram 两步验证密码不正确".to_string()),
        Err(error) => Err(format!("Telegram 两步验证失败: {error}")),
    }
}

async fn logout(context: &PluginContext) -> Result<Value, String> {
    let credentials = credentials_from_settings(&context.settings)?;
    let connection = TelegramConnection::connect(context, &credentials).await?;
    if connection
        .client
        .is_authorized()
        .await
        .map_err(|error| format_telegram_error("检查 Telegram 登录状态失败", &error))?
    {
        connection
            .client
            .sign_out()
            .await
            .map_err(|error| format_telegram_error("退出 Telegram 登录失败", &error))?;
    }
    remove_login_state(&context.data_dir)?;
    Ok(json!({"notice": "已退出 Telegram 登录。"}))
}

async fn resource_search(context: &PluginContext, payload: &Value) -> Result<Value, String> {
    let query = payload
        .get("query")
        .and_then(Value::as_str)
        .map(str::trim)
        .unwrap_or_default()
        .to_string();
    let credentials = credentials_from_settings(&context.settings)?;
    let channels = configured_channels(&context.settings)?;
    if channels.is_empty() {
        return Err("请先在 Telegram 插件配置中填写至少一个公开频道".to_string());
    }
    let per_channel_limit = setting_usize(&context.settings, "per_channel_limit", 50, 1, 100);
    let max_results = setting_usize(&context.settings, "max_results", 200, 1, 500);
    let connection = TelegramConnection::connect(context, &credentials).await?;
    connection.ensure_authorized().await?;

    let mut results = Vec::new();
    let mut seen = HashSet::new();
    let mut failures = Vec::new();
    let mut searched_channels = 0usize;
    for channel in channels {
        if results.len() >= max_results {
            break;
        }
        let remaining = max_results - results.len();
        let search = search_channel(
            &connection.client,
            &channel,
            &query,
            per_channel_limit,
            remaining,
            &mut seen,
        );
        match tokio::time::timeout(Duration::from_secs(25), search).await {
            Ok(Ok(result)) => {
                searched_channels += usize::from(result.searched);
                results.extend(result.items);
            }
            Ok(Err(error)) => {
                eprintln!("Telegram 频道 @{channel} 搜索失败: {error}");
                failures.push(format!("@{channel}: {error}"));
            }
            Err(_) => {
                eprintln!("Telegram 频道 @{channel} 搜索超时");
                failures.push(format!("@{channel}: 搜索超时"));
            }
        }
    }
    if searched_channels == 0 && !failures.is_empty() {
        return Err(format!(
            "Telegram 频道搜索全部失败：{}",
            failures.into_iter().take(3).collect::<Vec<_>>().join("；")
        ));
    }
    results.sort_by(|left, right| {
        right
            .get("publish_time")
            .and_then(Value::as_str)
            .cmp(&left.get("publish_time").and_then(Value::as_str))
    });
    Ok(json!({"results": results}))
}

async fn run_realtime_reporter(context: &PluginContext) -> Result<(), String> {
    ensure_cloudhub_enabled(context).await?;
    let credentials = credentials_from_settings(&context.settings)?;
    let channels = configured_report_channels(&context.settings)?;
    if channels.is_empty() {
        return Err("请先在 Telegram 插件配置中填写至少一个资源上报频道".to_string());
    }
    let mut connection = TelegramConnection::connect(context, &credentials).await?;
    connection.ensure_authorized().await?;
    let report_peers = resolve_report_peers(&connection.client, &channels).await?;
    let mut updates = connection
        .updates
        .take()
        .ok_or_else(|| "Telegram 实时消息流不可用".to_string())?;
    eprintln!(
        "TG 资源实时上报已启动，仅监听 {} 个已配置频道: {}",
        report_peers.len(),
        channels
            .iter()
            .map(|channel| format!("@{channel}"))
            .collect::<Vec<_>>()
            .join("、")
    );

    loop {
        tokio::select! {
            _ = shutdown_signal() => {
                updates
                    .sync_update_state()
                    .await
                    .map_err(|error| format!("保存 Telegram 实时消息游标失败: {error}"))?;
                eprintln!("TG 资源实时上报已停止");
                return Ok(());
            }
            update = updates.next() => {
                let update = update.map_err(|error| format_telegram_error("接收 Telegram 实时消息失败", &error))?;
                let (message, event_name) = match update {
                    Update::NewMessage(message) => (message.into_inner(), "新消息"),
                    Update::MessageEdited(message) => (message.into_inner(), "消息编辑"),
                    _ => continue,
                };
                let Some(channel) = report_channel_for_peer(&report_peers, message.peer_id()) else {
                    continue;
                };
                match report_realtime_message(context, &message, channel).await {
                    Ok(0) => {}
                    Ok(count) => eprintln!(
                        "Telegram @{channel} {event_name} #{} 已实时上报 {count} 条 CloudHub 资源",
                        message.id()
                    ),
                    Err(error) => eprintln!(
                        "Telegram @{channel} {event_name} #{} 资源上报失败: {error}",
                        message.id()
                    ),
                }
            }
        }
    }
}

fn report_channel_for_peer(
    report_peers: &HashMap<PeerId, String>,
    peer_id: PeerId,
) -> Option<&str> {
    report_peers.get(&peer_id).map(String::as_str)
}

async fn resolve_report_peers(
    client: &TelegramClient,
    channels: &[String],
) -> Result<HashMap<PeerId, String>, String> {
    let mut peers = HashMap::new();
    for channel in channels {
        let peer = resolve_public_channel(client, channel).await?;
        peers.insert(peer.id(), channel.clone());
    }
    Ok(peers)
}

async fn report_realtime_message(
    context: &PluginContext,
    message: &Message,
    channel: &str,
) -> Result<usize, String> {
    let resources = direct_resources_from_message(context, message, channel).await?;
    if resources.is_empty() {
        return Ok(0);
    }
    push_direct_resources(context, &resources).await?;
    Ok(resources.len())
}

async fn shutdown_signal() {
    #[cfg(unix)]
    {
        let mut terminate =
            tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
                .expect("install SIGTERM handler");
        tokio::select! {
            _ = tokio::signal::ctrl_c() => {}
            _ = terminate.recv() => {}
        }
    }
    #[cfg(not(unix))]
    {
        let _ = tokio::signal::ctrl_c().await;
    }
}

async fn ensure_cloudhub_enabled(context: &PluginContext) -> Result<(), String> {
    let response = context
        .http
        .get(format!("{}/settings", context.mediary_api_url))
        .bearer_auth(&context.mediary_token)
        .send()
        .await
        .map_err(|error| format!("读取 Mediary CloudHub 配置失败: {error}"))?;
    let status = response.status();
    let body = response
        .text()
        .await
        .map_err(|error| format!("读取 Mediary CloudHub 配置响应失败: {error}"))?;
    if !status.is_success() {
        return Err(format!(
            "读取 Mediary CloudHub 配置失败: HTTP {}",
            status.as_u16()
        ));
    }
    let settings: Value = serde_json::from_str(&body)
        .map_err(|error| format!("解析 Mediary CloudHub 配置失败: {error}"))?;
    let cloudhub = settings.get("cloudhub").unwrap_or(&Value::Null);
    if !cloudhub
        .get("enabled")
        .and_then(Value::as_bool)
        .unwrap_or(false)
    {
        return Err("Mediary 尚未启用 CloudHub，请先在系统设置中启用并配置 CloudHub".to_string());
    }
    Ok(())
}

async fn search_channel(
    client: &TelegramClient,
    channel_username: &str,
    query: &str,
    message_limit: usize,
    result_limit: usize,
    seen: &mut HashSet<String>,
) -> Result<ChannelSearchResult, String> {
    let peer = resolve_public_channel(client, channel_username).await?;
    let channel_name = peer.name().unwrap_or(channel_username).to_string();
    let public_username = peer.username().unwrap_or(channel_username).to_string();
    let peer_ref = peer
        .to_ref()
        .await
        .map_err(|error| format!("解析频道引用失败: {error}"))?
        .ok_or_else(|| "无法获取频道访问凭据，请确认账号已加入该频道".to_string())?;
    let mut items = Vec::new();
    if query.is_empty() {
        let mut messages = client.iter_messages(peer_ref).limit(message_limit);
        while let Some(message) = messages
            .next()
            .await
            .map_err(|error| format_telegram_error("读取最新消息失败", &error))?
        {
            append_message_resources(
                &mut items,
                &message,
                &channel_name,
                &public_username,
                result_limit,
                seen,
            );
            if items.len() >= result_limit {
                break;
            }
        }
    } else {
        let mut messages = client
            .search_messages(peer_ref)
            .query(query)
            .limit(message_limit);
        while let Some(message) = messages
            .next()
            .await
            .map_err(|error| format_telegram_error("搜索消息失败", &error))?
        {
            append_message_resources(
                &mut items,
                &message,
                &channel_name,
                &public_username,
                result_limit,
                seen,
            );
            if items.len() >= result_limit {
                break;
            }
        }
    }
    Ok(ChannelSearchResult {
        items,
        searched: true,
    })
}

fn append_message_resources(
    items: &mut Vec<Value>,
    message: &Message,
    channel_name: &str,
    public_username: &str,
    result_limit: usize,
    seen: &mut HashSet<String>,
) {
    for link in extract_message_links(message) {
        let global_key = format!("{}:{}", public_username.to_ascii_lowercase(), link.key);
        if !seen.insert(global_key) {
            continue;
        }
        items.push(resource_result_item(
            message,
            channel_name,
            public_username,
            &link,
        ));
        if items.len() >= result_limit {
            break;
        }
    }
}

async fn resolve_public_channel(client: &TelegramClient, username: &str) -> Result<Peer, String> {
    let peer = client
        .resolve_username(username)
        .await
        .map_err(|error| format_telegram_error("解析频道失败", &error))?
        .ok_or_else(|| format!("找不到公开频道 @{username}"))?;
    match peer {
        Peer::Channel(_) => Ok(peer),
        _ => Err(format!("@{username} 不是公开频道")),
    }
}

fn resource_result_item(
    message: &Message,
    channel_name: &str,
    channel_username: &str,
    link: &ResourceLink,
) -> Value {
    let title = message_title(message, channel_name);
    let description = truncate_chars(message.text().trim(), MAX_DESCRIPTION_CHARS);
    let message_url = format!("https://t.me/{channel_username}/{}", message.id());
    let (kind_label, action_kind, action_label, pending_label) = match link.kind {
        ResourceLinkKind::Share115 => ("115", "transfer", "转存", "转存中"),
        ResourceLinkKind::Magnet => ("磁力", "download", "离线下载", "下载中"),
        ResourceLinkKind::Ed2k => ("ed2k", "download", "离线下载", "下载中"),
    };
    json!({
        "title": title,
        "site_name": "TG搜索",
        "site_id": -3,
        "size": 0,
        "download_url": link.value,
        "description": if description.is_empty() { Value::Null } else { json!(description) },
        "publish_time": message.date().to_rfc3339(),
        "category": kind_label,
        "uploader": channel_name,
        "seeders": 0,
        "leechers": 0,
        "labels": [kind_label, channel_name],
        "hit_and_run": false,
        "source_kind": "plugin",
        "plugin_key": format!("{channel_username}:{}:{}", message.id(), link.key),
        "plugin_payload": {
            "channel": channel_username,
            "message_id": message.id(),
            "link_key": link.key,
            "message_url": message_url,
            "title": title
        },
        "plugin_action_kind": action_kind,
        "plugin_action_label": action_label,
        "plugin_action_pending_label": pending_label,
        "parsed": {
            "title": title,
            "raw_title": title
        }
    })
}

async fn transfer_resource(context: &PluginContext, payload: &Value) -> Result<Value, String> {
    let channel = normalize_channel_token(&required_text(payload, "channel")?)?;
    let message_id = payload
        .get("message_id")
        .and_then(Value::as_i64)
        .filter(|value| *value > 0 && *value <= i32::MAX as i64)
        .map(|value| value as i32)
        .ok_or_else(|| "Telegram 消息 ID 无效".to_string())?;
    let link_key = required_text(payload, "link_key")?;
    let credentials = credentials_from_settings(&context.settings)?;
    let connection = TelegramConnection::connect(context, &credentials).await?;
    connection.ensure_authorized().await?;
    let peer = resolve_public_channel(&connection.client, &channel).await?;
    let peer_ref = peer
        .to_ref()
        .await
        .map_err(|error| format!("解析频道引用失败: {error}"))?
        .ok_or_else(|| "无法读取原频道，请确认账号仍在频道中".to_string())?;
    let message = connection
        .client
        .get_messages_by_id(peer_ref, &[message_id])
        .await
        .map_err(|error| format_telegram_error("读取 Telegram 原消息失败", &error))?
        .into_iter()
        .next()
        .flatten()
        .ok_or_else(|| "Telegram 原消息不存在或已删除".to_string())?;
    let link = extract_message_links(&message)
        .into_iter()
        .find(|link| link.key == link_key)
        .ok_or_else(|| "原消息中的资源链接已变更，请重新搜索".to_string())?;
    let response = submit_link(context, &link).await?;
    let notice = first_text(&response, &["message", "notice"]).unwrap_or_else(|| match link.kind {
        ResourceLinkKind::Share115 => "115 分享已提交转存整理".to_string(),
        ResourceLinkKind::Magnet | ResourceLinkKind::Ed2k => {
            "资源已提交到 115 转存监控目录并等待 FlowLink 整理".to_string()
        }
    });
    Ok(json!({"notice": notice}))
}

async fn submit_link(context: &PluginContext, link: &ResourceLink) -> Result<Value, String> {
    let payload = mediary_link_submit_payload(link);
    submit_link_payload(context, &payload).await
}

async fn submit_link_payload(context: &PluginContext, payload: &Value) -> Result<Value, String> {
    let response = context
        .http
        .post(format!("{}/link/submit", context.mediary_api_url))
        .bearer_auth(&context.mediary_token)
        .json(&payload)
        .send()
        .await
        .map_err(|error| format!("提交资源到 Mediary 失败: {error}"))?;
    let status = response.status();
    let body = response
        .text()
        .await
        .map_err(|error| format!("读取 Mediary 响应失败: {error}"))?;
    let value = serde_json::from_str::<Value>(&body).unwrap_or(Value::Null);
    if !status.is_success() {
        let message = first_text(&value, &["error", "message"])
            .unwrap_or_else(|| format!("Mediary 返回 HTTP {}", status.as_u16()));
        return Err(message);
    }
    Ok(value)
}

async fn direct_resources_from_message(
    context: &PluginContext,
    message: &Message,
    channel: &str,
) -> Result<Vec<DirectResource>, String> {
    let sources = message_link_sources(message);
    let mut links = extract_resource_links(message.text(), &sources)
        .into_iter()
        .filter(|link| {
            matches!(
                link.kind,
                ResourceLinkKind::Share115 | ResourceLinkKind::Ed2k
            )
        })
        .collect::<Vec<_>>();
    let telegraph_urls = sources
        .iter()
        .flat_map(|source| http_url_regex().find_iter(source).map(|item| item.as_str()))
        .filter(|url| {
            Url::parse(url)
                .ok()
                .and_then(|url| url.host_str().map(str::to_ascii_lowercase))
                .is_some_and(|host| host == "telegra.ph" || host.ends_with(".telegra.ph"))
        })
        .map(str::to_string)
        .collect::<HashSet<_>>();
    if !telegraph_urls.is_empty() {
        let client = external_http_client(context).await?;
        for url in telegraph_urls {
            let response = client
                .get(&url)
                .send()
                .await
                .map_err(|error| format!("读取 Telegra.ph ED2K 列表失败: {error}"))?;
            let status = response.status();
            let body = response
                .text()
                .await
                .map_err(|error| format!("读取 Telegra.ph 响应失败: {error}"))?;
            if !status.is_success() {
                return Err(format!(
                    "读取 Telegra.ph ED2K 列表失败: HTTP {}",
                    status.as_u16()
                ));
            }
            let decoded = body
                .replace("&amp;", "&")
                .replace("&#124;", "|")
                .replace("&quot;", "\"");
            links.extend(
                extract_resource_links(message.text(), &[decoded])
                    .into_iter()
                    .filter(|link| link.kind == ResourceLinkKind::Ed2k),
            );
        }
    }
    let mut seen = HashSet::new();
    links.retain(|link| seen.insert(link.key.clone()));
    if links.is_empty() {
        return Ok(Vec::new());
    }
    let metadata = telegram_media_metadata_from_text(message.text())
        .ok_or_else(|| "资源消息缺少可识别的标题或 TMDB ID".to_string())?;
    Ok(links
        .into_iter()
        .filter_map(|link| direct_resource_payload(&metadata, &link, channel, message.id()))
        .collect())
}

async fn external_http_client(context: &PluginContext) -> Result<HttpClient, String> {
    let mut builder = HttpClient::builder()
        .connect_timeout(Duration::from_secs(10))
        .timeout(Duration::from_secs(30));
    if let Some(proxy_url) = configured_proxy_url(context).await? {
        let proxy = reqwest::Proxy::all(&proxy_url)
            .map_err(|error| format!("创建 Telegra.ph 代理失败: {error}"))?;
        builder = builder.proxy(proxy);
    }
    builder
        .build()
        .map_err(|error| format!("创建 Telegra.ph 客户端失败: {error}"))
}

async fn push_direct_resources(
    context: &PluginContext,
    resources: &[DirectResource],
) -> Result<Value, String> {
    let mut last = Value::Null;
    for chunk in resources.chunks(100) {
        let payload = chunk
            .iter()
            .map(|resource| resource.payload.clone())
            .collect::<Vec<_>>();
        let response = context
            .http
            .post(format!(
                "{}/cloudhub/resources/push",
                context.mediary_api_url
            ))
            .bearer_auth(&context.mediary_token)
            .json(&payload)
            .send()
            .await
            .map_err(|error| format!("上报 CloudHub 直链资源失败: {error}"))?;
        let status = response.status();
        let body = response
            .text()
            .await
            .map_err(|error| format!("读取 CloudHub 上报响应失败: {error}"))?;
        last = serde_json::from_str(&body).unwrap_or(Value::Null);
        if !status.is_success() {
            return Err(first_text(&last, &["error", "message"])
                .unwrap_or_else(|| format!("Mediary 返回 HTTP {}", status.as_u16())));
        }
    }
    Ok(last)
}

fn direct_resource_payload(
    metadata: &TelegramMediaMetadata,
    link: &ResourceLink,
    channel: &str,
    message_id: i32,
) -> Option<DirectResource> {
    let (virtual_owner, link_field, name, size, season, episode, start_episode, is_full_season) =
        match link.kind {
            ResourceLinkKind::Share115 => (
                "share115",
                "share_link",
                bundle_resource_name(metadata),
                metadata.size,
                metadata.season,
                None,
                metadata.episode,
                metadata.end_episode.is_some(),
            ),
            ResourceLinkKind::Ed2k => {
                let (name, size) = parse_ed2k_file(&link.value)?;
                let captures = season_episode_regex().captures(&name);
                let season = captures
                    .as_ref()
                    .and_then(|value| value.get(1))
                    .and_then(|value| value.as_str().parse::<i32>().ok())
                    .or(metadata.season);
                let episode = captures
                    .as_ref()
                    .and_then(|value| value.get(2))
                    .and_then(|value| value.as_str().parse::<i32>().ok())
                    .or(metadata.episode);
                ("ed2k", "ed2k", name, size, season, episode, episode, false)
            }
            ResourceLinkKind::Magnet => return None,
        };
    if name.trim().is_empty() || size <= 0 {
        return None;
    }
    let sha1 = format!("{:X}", Sha256::digest(link.key.as_bytes()));
    let mut payload = json!({
        "schema": "cloud_resource.v1",
        "source_app": "telegram-resource",
        "source_node_id": channel,
        "sha1": sha1,
        "size": size,
        "name": name,
        "raw_name": name,
        "title": metadata.title,
        "tmdb_id": metadata.tmdb_id,
        "type": metadata.media_type,
        "season": season.unwrap_or_default(),
        "episode": episode.unwrap_or_default(),
        "start_season": season.unwrap_or_default(),
        "start_episode": start_episode.unwrap_or_default(),
        "is_full_season": is_full_season,
        "quality": metadata.quality,
        "year": metadata.year,
        "virtual_owner": virtual_owner,
        "owner_name": if virtual_owner == "share115" { "115 分享" } else { "ED2K" },
        "telegram_channel": channel,
        "telegram_message_id": message_id,
    });
    payload
        .as_object_mut()?
        .insert(link_field.to_string(), json!(link.value));
    Some(DirectResource { payload })
}

fn parse_ed2k_file(link: &str) -> Option<(String, i64)> {
    let parts = link.split('|').collect::<Vec<_>>();
    if parts.len() < 6 || !parts[0].eq_ignore_ascii_case("ed2k://") || parts[1] != "file" {
        return None;
    }
    let name = percent_decode_str(parts[2])
        .decode_utf8_lossy()
        .into_owned();
    let size = parts[3].parse::<i64>().ok()?;
    Some((name, size))
}

fn bundle_resource_name(metadata: &TelegramMediaMetadata) -> String {
    let mut value = metadata.title.clone();
    if let Some(year) = metadata.year {
        value.push_str(&format!(" ({year})"));
    }
    if let Some(season) = metadata.season {
        value.push_str(&format!(" S{season:02}"));
        if let Some(start) = metadata.episode {
            value.push_str(&format!("E{start:02}"));
            if let Some(end) = metadata.end_episode {
                value.push_str(&format!("-E{end:02}"));
            }
        }
    }
    if !metadata.quality.is_empty() {
        value.push(' ');
        value.push_str(&metadata.quality);
    }
    value.push_str(" [115分享]");
    value
}

#[cfg(test)]
fn telegram_media_hint_from_text(text: &str) -> Option<Value> {
    let metadata = telegram_media_metadata_from_text(text)?;
    let episodes = metadata.episode.into_iter().collect::<Vec<_>>();
    Some(json!({
        "schema_version": 1,
        "source": "telegram-resource",
        "subscription_id": Value::Null,
        "tmdb_id": metadata.tmdb_id,
        "media_type": metadata.media_type,
        "title": metadata.title,
        "year": metadata.year,
        "season": metadata.season,
        "episodes": episodes,
        "select_episodes": false,
        "secondary_category": Value::Null,
        "sha1": Value::Null,
        "receive_title": metadata.title
    }))
}

fn telegram_media_metadata_from_text(text: &str) -> Option<TelegramMediaMetadata> {
    let text = text.trim();
    let first_line = text.lines().find(|line| !line.trim().is_empty())?.trim();
    let tmdb_id = tmdb_id_regex()
        .captures(text)
        .and_then(|captures| captures.get(1))?
        .as_str()
        .parse::<i64>()
        .ok()?;
    let media_type = if first_line.contains("电影") {
        "movie"
    } else if first_line.contains("剧集")
        || first_line.contains("电视剧")
        || season_episode_regex().is_match(first_line)
    {
        "tv"
    } else {
        return None;
    };
    let title_section = first_line
        .split_once(['：', ':'])
        .map(|(_, value)| value.trim())
        .unwrap_or(first_line);
    let title = title_year_regex()
        .captures(title_section)
        .and_then(|captures| captures.get(1))
        .map(|value| value.as_str().trim())
        .filter(|value| !value.is_empty())
        .unwrap_or(title_section)
        .trim_matches(['[', ']', '【', '】'])
        .trim();
    if title.is_empty() {
        return None;
    }
    let year = year_regex()
        .captures(first_line)
        .and_then(|captures| captures.get(1))
        .and_then(|value| value.as_str().parse::<i32>().ok());
    let season_episode = season_episode_regex().captures(text);
    let season = season_episode
        .as_ref()
        .and_then(|captures| captures.get(1))
        .and_then(|value| value.as_str().parse::<i32>().ok());
    let episode = season_episode
        .as_ref()
        .and_then(|captures| captures.get(2))
        .and_then(|value| value.as_str().parse::<i32>().ok());
    let end_episode = season_episode_range_regex()
        .captures(text)
        .and_then(|captures| captures.get(3))
        .and_then(|value| value.as_str().parse::<i32>().ok());
    let quality = quality_from_message(text);
    let size = size_from_message(text).unwrap_or_default();
    Some(TelegramMediaMetadata {
        title: title.to_string(),
        tmdb_id,
        media_type: media_type.to_string(),
        year,
        season,
        episode,
        end_episode,
        quality,
        size,
    })
}

fn mediary_link_submit_payload(link: &ResourceLink) -> Value {
    match link.kind {
        ResourceLinkKind::Share115 => json!({"link": link.value}),
        ResourceLinkKind::Magnet | ResourceLinkKind::Ed2k => json!({
            "link": link.value,
            "offline_target": "transfer",
            "flowlink_move_all_delay_seconds": FLOWLINK_MOVE_ALL_DELAY_SECONDS
        }),
    }
}

fn extract_message_links(message: &Message) -> Vec<ResourceLink> {
    let sources = message_link_sources(message);
    extract_resource_links(message.text(), &sources)
}

fn message_link_sources(message: &Message) -> Vec<String> {
    let mut sources = vec![message.text().to_string()];
    if let Some(entities) = message.fmt_entities() {
        for entity in entities {
            if let tl::enums::MessageEntity::TextUrl(entity) = entity {
                sources.push(entity.url.clone());
            }
        }
    }
    if let Some(tl::enums::ReplyMarkup::ReplyInlineMarkup(markup)) = message.reply_markup() {
        for row in markup.rows {
            let tl::enums::KeyboardButtonRow::Row(row) = row;
            for button in row.buttons {
                match button {
                    tl::enums::KeyboardButton::Url(button) => {
                        sources.push(button.url);
                    }
                    tl::enums::KeyboardButton::UrlAuth(button) => {
                        sources.push(button.url);
                    }
                    tl::enums::KeyboardButton::WebView(button) => {
                        sources.push(button.url);
                    }
                    tl::enums::KeyboardButton::SimpleWebView(button) => {
                        sources.push(button.url);
                    }
                    _ => {}
                }
            }
        }
    }
    sources
}

fn extract_resource_links(message_text: &str, sources: &[String]) -> Vec<ResourceLink> {
    let mut candidates = Vec::new();
    for source in sources {
        candidates.extend(
            http_url_regex()
                .find_iter(source)
                .map(|value| value.as_str()),
        );
        candidates.extend(magnet_regex().find_iter(source).map(|value| value.as_str()));
        candidates.extend(ed2k_regex().find_iter(source).map(|value| value.as_str()));
    }

    let password = password_regex()
        .captures(message_text)
        .and_then(|captures| captures.get(1))
        .map(|value| value.as_str().to_string());
    let mut results = Vec::new();
    let mut seen = HashSet::new();
    for candidate in candidates {
        let candidate = trim_link_punctuation(candidate);
        let Some(mut link) = classify_resource_link(candidate) else {
            continue;
        };
        if link.kind == ResourceLinkKind::Share115 {
            link.value = append_115_password(&link.value, password.as_deref());
        }
        if seen.insert(link.key.clone()) {
            results.push(link);
        }
    }
    results
}

fn classify_resource_link(candidate: &str) -> Option<ResourceLink> {
    let candidate = candidate.trim();
    let lower = candidate.to_ascii_lowercase();
    if lower.starts_with("magnet:?") {
        let key = reqwest::Url::parse(candidate)
            .ok()
            .and_then(|url| {
                url.query_pairs()
                    .find(|(name, _)| name.eq_ignore_ascii_case("xt"))
                    .map(|(_, value)| format!("magnet:{}", value.to_ascii_lowercase()))
            })
            .unwrap_or_else(|| format!("magnet:{lower}"));
        return Some(ResourceLink {
            kind: ResourceLinkKind::Magnet,
            value: candidate.to_string(),
            key,
        });
    }
    if lower.starts_with("ed2k://") {
        return Some(ResourceLink {
            kind: ResourceLinkKind::Ed2k,
            value: candidate.to_string(),
            key: format!("ed2k:{lower}"),
        });
    }

    let url = reqwest::Url::parse(candidate).ok()?;
    if !matches!(url.scheme(), "http" | "https") || !is_115_share_host(url.host_str()?) {
        return None;
    }
    let segments = url
        .path_segments()?
        .filter(|segment| !segment.is_empty())
        .collect::<Vec<_>>();
    let share_index = segments
        .windows(2)
        .position(|parts| parts[0].eq_ignore_ascii_case("s") && !parts[1].is_empty())?;
    let share_code = segments[share_index + 1].to_ascii_lowercase();
    Some(ResourceLink {
        kind: ResourceLinkKind::Share115,
        value: url.to_string(),
        key: format!("115:{share_code}"),
    })
}

fn is_115_share_host(host: &str) -> bool {
    let host = host.to_ascii_lowercase();
    ["115.com", "115cdn.com", "115v.com"]
        .iter()
        .any(|allowed| host == *allowed || host.ends_with(&format!(".{allowed}")))
}

fn append_115_password(link: &str, password: Option<&str>) -> String {
    let Some(password) = password.filter(|value| !value.trim().is_empty()) else {
        return link.to_string();
    };
    let Ok(mut url) = reqwest::Url::parse(link) else {
        return link.to_string();
    };
    if url
        .query_pairs()
        .any(|(name, _)| name.eq_ignore_ascii_case("password"))
    {
        return url.to_string();
    }
    url.query_pairs_mut()
        .append_pair("password", password.trim());
    url.to_string()
}

fn trim_link_punctuation(value: &str) -> &str {
    value.trim().trim_end_matches([
        '.', ',', ';', ':', '!', '?', ')', ']', '}', '。', '，', '；', '：', '！', '？', '）',
        '】', '》', '"', '\'',
    ])
}

fn message_title(message: &Message, channel_name: &str) -> String {
    if let Some(Media::Document(document)) = message.media()
        && let Some(name) = document.name().filter(|value| !value.trim().is_empty())
    {
        return truncate_chars(name.trim(), 180);
    }
    for line in message.text().lines() {
        let line = http_url_regex().replace_all(line, "");
        let line = magnet_regex().replace_all(&line, "");
        let line = ed2k_regex().replace_all(&line, "");
        let line = line.trim().trim_matches(['-', '—', '|', '·']).trim();
        if !line.is_empty() {
            return truncate_chars(line, 180);
        }
    }
    format!("{channel_name} 消息 #{}", message.id())
}

fn configured_channels(settings: &Map<String, Value>) -> Result<Vec<String>, String> {
    configured_channel_setting(settings, "channels")
}

fn configured_report_channels(settings: &Map<String, Value>) -> Result<Vec<String>, String> {
    if !setting_text(settings, "report_channels").trim().is_empty() {
        configured_channel_setting(settings, "report_channels")
    } else {
        configured_channel_setting(settings, "subscription_channels")
    }
}

fn configured_channel_setting(
    settings: &Map<String, Value>,
    setting_key: &str,
) -> Result<Vec<String>, String> {
    let raw = setting_text(settings, setting_key);
    let mut channels = Vec::new();
    let mut seen = HashSet::new();
    for item in raw.split(|character: char| {
        character.is_whitespace() || matches!(character, ',' | ';' | '，' | '；')
    }) {
        if item.trim().is_empty() {
            continue;
        }
        let channel = normalize_channel_token(item)?;
        if seen.insert(channel.to_ascii_lowercase()) {
            channels.push(channel);
        }
        if channels.len() > MAX_CHANNELS {
            return Err(format!("Telegram 频道最多配置 {MAX_CHANNELS} 个"));
        }
    }
    Ok(channels)
}

fn normalize_channel_token(value: &str) -> Result<String, String> {
    let value = value.trim().trim_end_matches('/');
    if value.is_empty() {
        return Err("Telegram 频道用户名不能为空".to_string());
    }
    let candidate = if value.starts_with("http://") || value.starts_with("https://") {
        let url =
            reqwest::Url::parse(value).map_err(|_| format!("Telegram 频道地址无效: {value}"))?;
        if !matches!(
            url.host_str(),
            Some("t.me" | "www.t.me" | "telegram.me" | "www.telegram.me")
        ) {
            return Err(format!("不是 Telegram 频道地址: {value}"));
        }
        url.path_segments()
            .into_iter()
            .flatten()
            .find(|part| !part.is_empty())
            .ok_or_else(|| format!("Telegram 频道地址缺少用户名: {value}"))?
            .to_string()
    } else {
        value.trim_start_matches('@').to_string()
    };
    if candidate.starts_with('+')
        || candidate.eq_ignore_ascii_case("joinchat")
        || candidate.eq_ignore_ascii_case("c")
        || !channel_username_regex().is_match(&candidate)
    {
        return Err(format!("仅支持公开频道用户名，不支持邀请链接: {value}"));
    }
    Ok(candidate)
}

fn credentials_from_settings(settings: &Map<String, Value>) -> Result<TelegramCredentials, String> {
    let api_id = value_i64(settings.get("api_id"))
        .filter(|value| *value > 0 && *value <= i32::MAX as i64)
        .map(|value| value as i32)
        .ok_or_else(|| "请先配置有效的 Telegram API ID".to_string())?;
    let api_hash = setting_text(settings, "api_hash").trim().to_string();
    validate_api_hash(&api_hash)?;
    Ok(TelegramCredentials { api_id, api_hash })
}

fn credentials_for_action(
    context: &PluginContext,
    payload: &Value,
) -> Result<TelegramCredentials, String> {
    let api_id = payload
        .get("api_id")
        .and_then(value_i64_ref)
        .or_else(|| value_i64(context.settings.get("api_id")))
        .filter(|value| *value > 0 && *value <= i32::MAX as i64)
        .map(|value| value as i32)
        .ok_or_else(|| "请先配置有效的 Telegram API ID".to_string())?;
    let api_hash = effective_text(context, payload, "api_hash")?;
    validate_api_hash(&api_hash)?;
    Ok(TelegramCredentials { api_id, api_hash })
}

fn validate_api_hash(value: &str) -> Result<(), String> {
    if value.len() != 32 || !value.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return Err("Telegram API Hash 应为 32 位十六进制字符串".to_string());
    }
    Ok(())
}

fn validate_phone(value: &str) -> Result<(), String> {
    let digits = value.chars().filter(char::is_ascii_digit).count();
    if !value.starts_with('+') || !(7..=15).contains(&digits) {
        return Err("手机号请使用 +国家代码 的国际格式".to_string());
    }
    Ok(())
}

fn effective_text(context: &PluginContext, payload: &Value, key: &str) -> Result<String, String> {
    let current = payload
        .get(key)
        .and_then(Value::as_str)
        .unwrap_or_default()
        .trim();
    if !current.is_empty() && current != SECRET_PLACEHOLDER {
        return Ok(current.to_string());
    }
    let saved = setting_text(&context.settings, key).trim();
    if saved.is_empty() {
        Err(format!("请先配置 {key}"))
    } else {
        Ok(saved.to_string())
    }
}

fn action_secret(payload: &Value, key: &str) -> Result<String, String> {
    let value = payload
        .get(key)
        .and_then(Value::as_str)
        .unwrap_or_default()
        .trim();
    if value.is_empty() || value == SECRET_PLACEHOLDER {
        Err(match key {
            "login_code" => "请填写本次收到的 Telegram 验证码".to_string(),
            "two_factor_password" => "请填写 Telegram 两步验证密码".to_string(),
            _ => format!("缺少 {key}"),
        })
    } else {
        Ok(value.to_string())
    }
}

fn current_login_state(data_dir: &Path) -> Result<Option<LoginState>, String> {
    let path = data_dir.join(LOGIN_STATE_FILE);
    if !path.exists() {
        return Ok(None);
    }
    let body = fs::read_to_string(&path)
        .map_err(|error| format!("读取 Telegram 登录状态失败: {error}"))?;
    let state = serde_json::from_str::<LoginState>(&body)
        .map_err(|error| format!("解析 Telegram 登录状态失败: {error}"))?;
    if state.expires_at <= Utc::now().timestamp() {
        remove_login_state(data_dir)?;
        return Ok(None);
    }
    Ok(Some(state))
}

fn write_login_state(data_dir: &Path, state: &LoginState) -> Result<(), String> {
    let path = data_dir.join(LOGIN_STATE_FILE);
    write_secure_json(&path, state)
}

fn remove_login_state(data_dir: &Path) -> Result<(), String> {
    let path = data_dir.join(LOGIN_STATE_FILE);
    match fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(format!("清理 Telegram 登录状态失败: {error}")),
    }
}

fn write_secure_json<T: Serialize>(path: &Path, value: &T) -> Result<(), String> {
    let bytes = serde_json::to_vec(value)
        .map_err(|error| format!("序列化 Telegram 登录状态失败: {error}"))?;
    let temp = path.with_extension(format!("tmp-{}", std::process::id()));
    let mut options = OpenOptions::new();
    options.create(true).truncate(true).write(true);
    #[cfg(unix)]
    options.mode(0o600);
    let mut file = options
        .open(&temp)
        .map_err(|error| format!("写入 Telegram 登录状态失败: {error}"))?;
    use std::io::Write;
    file.write_all(&bytes)
        .map_err(|error| format!("写入 Telegram 登录状态失败: {error}"))?;
    file.sync_all()
        .map_err(|error| format!("同步 Telegram 登录状态失败: {error}"))?;
    fs::rename(&temp, path).map_err(|error| format!("保存 Telegram 登录状态失败: {error}"))?;
    secure_file_if_exists(path)
}

fn lock_action(data_dir: &Path) -> Result<File, String> {
    let path = data_dir.join(ACTION_LOCK_FILE);
    let mut options = OpenOptions::new();
    options.create(true).read(true).write(true);
    #[cfg(unix)]
    options.mode(0o600);
    let file = options
        .open(path)
        .map_err(|error| format!("打开 Telegram 插件锁失败: {error}"))?;
    file.try_lock_exclusive()
        .map_err(|_| "Telegram 插件正在执行其他操作，请稍后重试".to_string())?;
    Ok(file)
}

fn secure_directory(path: &Path) -> Result<(), String> {
    #[cfg(unix)]
    fs::set_permissions(path, fs::Permissions::from_mode(0o700))
        .map_err(|error| format!("设置 Telegram 数据目录权限失败: {error}"))?;
    Ok(())
}

fn secure_file_if_exists(path: &Path) -> Result<(), String> {
    if !path.exists() {
        return Ok(());
    }
    #[cfg(unix)]
    fs::set_permissions(path, fs::Permissions::from_mode(0o600))
        .map_err(|error| format!("设置 Telegram Session 权限失败: {error}"))?;
    Ok(())
}

fn logged_in_response(
    display_name: String,
    username: Option<String>,
    channel_count: usize,
    report_channel_count: usize,
    notice: &str,
) -> Value {
    json!({
        "notice": notice,
        "items": [{
            "key": "telegram-account",
            "title": display_name,
            "badges": [{"label": "已登录", "tone": "success"}],
            "metadata": [
                {"label": "用户名", "value": username.map(|value| format!("@{value}")).unwrap_or_else(|| "-".to_string())},
                {"label": "搜索频道", "value": channel_count},
                {"label": "资源上报频道", "value": report_channel_count}
            ],
            "actions": [{
                "type": "plugin_action",
                "action": "logout",
                "label": "退出登录",
                "icon": "trash",
                "tone": "danger",
                "confirm": {
                    "title": "退出 Telegram",
                    "message": "确定注销当前插件使用的 Telegram Session 吗？",
                    "confirm_text": "退出登录",
                    "danger": true
                }
            }]
        }]
    })
}

fn pending_login_response(state: Option<&LoginState>) -> Value {
    let (notice, subtitle, label) = match state.map(|state| state.phase) {
        Some(LoginPhase::Code) => (
            "Telegram 等待输入验证码。",
            "填写本次收到的验证码，然后点击“完成登录”。",
            "待验证码",
        ),
        Some(LoginPhase::Password) => (
            "Telegram 等待两步验证密码。",
            "填写两步验证密码，然后再次点击“完成登录”。",
            "待两步验证",
        ),
        None => (
            "Telegram 尚未登录。",
            "点击“发送验证码”开始登录。",
            "待登录",
        ),
    };
    json!({
        "notice": notice,
        "items": [{
            "key": "telegram-login",
            "title": "Telegram 用户登录",
            "subtitle": subtitle,
            "badges": [{"label": label, "tone": "warning"}]
        }]
    })
}

fn display_user_name(user: &grammers_client::peer::User) -> String {
    let mut parts = Vec::new();
    if let Some(first_name) = user.first_name().filter(|value| !value.trim().is_empty()) {
        parts.push(first_name.trim());
    }
    if let Some(last_name) = user.last_name().filter(|value| !value.trim().is_empty()) {
        parts.push(last_name.trim());
    }
    if parts.is_empty() {
        user.username()
            .map(|value| format!("@{value}"))
            .unwrap_or_else(|| user.id().to_string())
    } else {
        parts.join(" ")
    }
}

fn format_telegram_error(context: &str, error: &InvocationError) -> String {
    match error {
        InvocationError::Rpc(rpc) if rpc.name == "FLOOD_WAIT" => format!(
            "{context}: Telegram 请求过于频繁，请等待 {} 秒后重试",
            rpc.value.unwrap_or(30)
        ),
        InvocationError::Rpc(rpc) if rpc.name == "CHANNEL_PRIVATE" => {
            format!("{context}: 账号尚未加入频道或没有访问权限")
        }
        InvocationError::Rpc(rpc) if rpc.name == "AUTH_KEY_UNREGISTERED" => {
            format!("{context}: Telegram Session 已失效，请重新登录")
        }
        _ => format!("{context}: {error}"),
    }
}

fn required_env(key: &str) -> Result<String, String> {
    env::var(key)
        .ok()
        .filter(|value| !value.trim().is_empty())
        .ok_or_else(|| format!("缺少环境变量 {key}"))
}

fn required_text(payload: &Value, key: &str) -> Result<String, String> {
    payload
        .get(key)
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_string)
        .ok_or_else(|| format!("缺少参数 {key}"))
}

fn setting_text<'a>(settings: &'a Map<String, Value>, key: &str) -> &'a str {
    settings
        .get(key)
        .and_then(Value::as_str)
        .unwrap_or_default()
}

fn setting_usize(
    settings: &Map<String, Value>,
    key: &str,
    default: usize,
    min: usize,
    max: usize,
) -> usize {
    value_i64(settings.get(key))
        .and_then(|value| usize::try_from(value).ok())
        .unwrap_or(default)
        .clamp(min, max)
}

fn value_i64(value: Option<&Value>) -> Option<i64> {
    value.and_then(value_i64_ref)
}

fn value_i64_ref(value: &Value) -> Option<i64> {
    value
        .as_i64()
        .or_else(|| value.as_str()?.trim().parse::<i64>().ok())
}

fn first_text(value: &Value, keys: &[&str]) -> Option<String> {
    for key in keys {
        if let Some(text) = value.get(key).and_then(Value::as_str) {
            let text = text.trim();
            if !text.is_empty() {
                return Some(text.to_string());
            }
        }
    }
    None
}

fn truncate_chars(value: &str, max: usize) -> String {
    let mut chars = value.chars();
    let truncated = chars.by_ref().take(max).collect::<String>();
    if chars.next().is_some() {
        format!("{truncated}…")
    } else {
        truncated
    }
}

fn http_url_regex() -> &'static Regex {
    static VALUE: OnceLock<Regex> = OnceLock::new();
    VALUE.get_or_init(|| Regex::new(r#"(?i)https?://[^\s<>\"']+"#).unwrap())
}

fn magnet_regex() -> &'static Regex {
    static VALUE: OnceLock<Regex> = OnceLock::new();
    VALUE.get_or_init(|| Regex::new(r#"(?i)magnet:\?[^\s<>\"']+"#).unwrap())
}

fn ed2k_regex() -> &'static Regex {
    static VALUE: OnceLock<Regex> = OnceLock::new();
    VALUE.get_or_init(|| Regex::new(r#"(?i)ed2k://\|file\|[^\r\n<>\"']*?\|/"#).unwrap())
}

fn password_regex() -> &'static Regex {
    static VALUE: OnceLock<Regex> = OnceLock::new();
    VALUE.get_or_init(|| {
        Regex::new(r"(?i)(?:提取码|访问码|密码|password|pwd)\s*[:：=]?\s*([a-z0-9]{4,8})").unwrap()
    })
}

fn size_from_message(text: &str) -> Option<i64> {
    let captures = size_regex().captures(text)?;
    let value = captures.get(1)?.as_str().parse::<f64>().ok()?;
    let multiplier = match captures.get(2)?.as_str().to_ascii_uppercase().as_str() {
        "TB" => 1024_f64.powi(4),
        "GB" => 1024_f64.powi(3),
        "MB" => 1024_f64.powi(2),
        "KB" => 1024_f64,
        "B" => 1.0,
        _ => return None,
    };
    Some((value * multiplier).round() as i64)
}

fn quality_from_message(text: &str) -> String {
    if let Some(value) = quality_regex()
        .captures(text)
        .and_then(|captures| captures.get(1))
    {
        return value.as_str().trim().to_string();
    }
    text.lines()
        .map(str::trim)
        .find(|line| {
            !line.to_ascii_lowercase().starts_with("ed2k://")
                && resolution_regex().is_match(line)
                && release_source_regex().is_match(line)
        })
        .map(str::to_string)
        .unwrap_or_default()
}

fn size_regex() -> &'static Regex {
    static VALUE: OnceLock<Regex> = OnceLock::new();
    VALUE.get_or_init(|| {
        Regex::new(r"(?i)(?:大小|体积)\s*[:：]\s*([0-9]+(?:\.[0-9]+)?)\s*(TB|GB|MB|KB|B)").unwrap()
    })
}

fn quality_regex() -> &'static Regex {
    static VALUE: OnceLock<Regex> = OnceLock::new();
    VALUE.get_or_init(|| Regex::new(r"(?im)^(?:🎞️?\s*)?(?:质量|版本)\s*[:：]\s*(.+)$").unwrap())
}

fn resolution_regex() -> &'static Regex {
    static VALUE: OnceLock<Regex> = OnceLock::new();
    VALUE.get_or_init(|| Regex::new(r"(?i)\b(?:480p|720p|1080p|2160p|4K|8K)\b").unwrap())
}

fn release_source_regex() -> &'static Regex {
    static VALUE: OnceLock<Regex> = OnceLock::new();
    VALUE.get_or_init(|| Regex::new(r"(?i)(?:WEB[ .-]?DL|Blu[ .-]?Ray|REMUX|HDTV)").unwrap())
}

fn tmdb_id_regex() -> &'static Regex {
    static VALUE: OnceLock<Regex> = OnceLock::new();
    VALUE
        .get_or_init(|| Regex::new(r"(?i)(?:tmdb\s*id|tmdbid|tmdb)[\s:：_{}-]*(\d{2,10})").unwrap())
}

fn title_year_regex() -> &'static Regex {
    static VALUE: OnceLock<Regex> = OnceLock::new();
    VALUE.get_or_init(|| Regex::new(r"^(.+?)\s*[\(（](?:19|20)\d{2}[\)）]").unwrap())
}

fn year_regex() -> &'static Regex {
    static VALUE: OnceLock<Regex> = OnceLock::new();
    VALUE.get_or_init(|| Regex::new(r"[\(（]((?:19|20)\d{2})[\)）]").unwrap())
}

fn season_episode_regex() -> &'static Regex {
    static VALUE: OnceLock<Regex> = OnceLock::new();
    VALUE.get_or_init(|| Regex::new(r"(?i)\bS0*(\d{1,3})E0*(\d{1,5})").unwrap())
}

fn season_episode_range_regex() -> &'static Regex {
    static VALUE: OnceLock<Regex> = OnceLock::new();
    VALUE.get_or_init(|| {
        Regex::new(r"(?i)\bS0*(\d{1,3})E0*(\d{1,5})(?:\s*-\s*E?0*(\d{1,5}))?").unwrap()
    })
}

fn channel_username_regex() -> &'static Regex {
    static VALUE: OnceLock<Regex> = OnceLock::new();
    VALUE.get_or_init(|| Regex::new(r"^[A-Za-z][A-Za-z0-9_]{2,31}$").unwrap())
}

#[cfg(test)]
mod tests {
    use super::{
        FLOWLINK_MOVE_ALL_DELAY_SECONDS, HttpConnectBridge, ResourceLinkKind,
        TelegramMediaMetadata, append_115_password, classify_resource_link,
        direct_resource_payload, extract_resource_links, mediary_link_submit_payload,
        normalize_channel_token, parse_ed2k_file, report_channel_for_peer, size_from_message,
        telegram_media_hint_from_text, telegram_media_metadata_from_text, trim_link_punctuation,
    };
    use grammers_session::types::PeerId;
    use std::collections::HashMap;
    use tokio::{
        io::{AsyncReadExt, AsyncWriteExt},
        net::{TcpListener, TcpStream},
    };

    #[tokio::test]
    async fn bridges_socks5_clients_through_an_http_connect_proxy() {
        let proxy_listener = TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
        let proxy_address = proxy_listener.local_addr().unwrap();
        let proxy = tokio::spawn(async move {
            let (mut stream, _) = proxy_listener.accept().await.unwrap();
            let mut header = Vec::new();
            let mut byte = [0u8; 1];
            while !header.ends_with(b"\r\n\r\n") {
                stream.read_exact(&mut byte).await.unwrap();
                header.push(byte[0]);
            }
            let header = String::from_utf8(header).unwrap();
            assert!(header.starts_with("CONNECT 149.154.167.50:443 HTTP/1.1\r\n"));
            stream
                .write_all(b"HTTP/1.1 200 Connection Established\r\n\r\n")
                .await
                .unwrap();
            let mut ping = [0u8; 4];
            stream.read_exact(&mut ping).await.unwrap();
            assert_eq!(&ping, b"ping");
            stream.write_all(b"pong").await.unwrap();
        });

        let bridge = HttpConnectBridge::start(&format!("http://{proxy_address}"))
            .await
            .unwrap();
        let mut client = TcpStream::connect(bridge.address).await.unwrap();
        client.write_all(&[5, 1, 0]).await.unwrap();
        let mut greeting = [0u8; 2];
        client.read_exact(&mut greeting).await.unwrap();
        assert_eq!(greeting, [5, 0]);
        client
            .write_all(&[5, 1, 0, 1, 149, 154, 167, 50, 0x01, 0xbb])
            .await
            .unwrap();
        let mut connected = [0u8; 10];
        client.read_exact(&mut connected).await.unwrap();
        assert_eq!(&connected[..2], &[5, 0]);
        client.write_all(b"ping").await.unwrap();
        let mut pong = [0u8; 4];
        client.read_exact(&mut pong).await.unwrap();
        assert_eq!(&pong, b"pong");
        proxy.await.unwrap();
    }

    #[test]
    fn manifest_declares_realtime_runtime_without_scheduled_polling() {
        let manifest: serde_json::Value =
            serde_json::from_str(include_str!("../../telegram-resource/plugin.json")).unwrap();
        assert_eq!(manifest["version"], "0.2.2");
        assert_eq!(manifest["runtime"]["entrypoint"], "./plugin");
        assert!(manifest.get("scheduled_actions").is_none());
        let fields = manifest["settings_schema"]["sections"]
            .as_array()
            .unwrap()
            .iter()
            .flat_map(|section| section["fields"].as_array().into_iter().flatten())
            .filter_map(|field| field["key"].as_str())
            .collect::<Vec<_>>();
        assert!(fields.contains(&"report_channels"));
        assert!(
            !fields
                .iter()
                .any(|key| key.starts_with("report_") && *key != "report_channels")
        );
        assert_eq!(
            manifest["requested_scopes"],
            serde_json::json!(["integrations:run", "settings:read"])
        );
    }

    #[test]
    fn extracts_supported_links_and_attaches_115_password() {
        let text = "资源 https://115.com/s/swExample 提取码: 7788\nmagnet:?xt=urn:btih:ABCDEF&dn=Demo\ned2k://|file|demo.mkv|1|HASH|/";
        let links = extract_resource_links(text, &[text.to_string()]);
        assert_eq!(links.len(), 3);
        assert_eq!(links[0].kind, ResourceLinkKind::Share115);
        assert!(links[0].value.contains("password=7788"));
        assert_eq!(links[1].kind, ResourceLinkKind::Magnet);
        assert_eq!(links[2].kind, ResourceLinkKind::Ed2k);
    }

    #[test]
    fn deduplicates_equivalent_links() {
        let text = "https://115.com/s/swExample https://115cdn.com/s/swExample magnet:?xt=urn:btih:ABC magnet:?xt=urn:btih:abc";
        let links = extract_resource_links(text, &[text.to_string()]);
        assert_eq!(links.len(), 2);
    }

    #[test]
    fn rejects_spoofed_115_hosts() {
        assert!(classify_resource_link("https://evil115.com/s/example").is_none());
        assert!(classify_resource_link("https://115.com.evil.example/s/example").is_none());
        assert!(classify_resource_link("https://sub.115.com/s/example").is_some());
    }

    #[test]
    fn parses_public_channel_forms_and_rejects_invites() {
        assert_eq!(
            normalize_channel_token("@Movie_Channel").unwrap(),
            "Movie_Channel"
        );
        assert_eq!(
            normalize_channel_token("https://t.me/Movie_Channel/123").unwrap(),
            "Movie_Channel"
        );
        assert!(normalize_channel_token("https://t.me/+invite").is_err());
        assert!(normalize_channel_token("https://example.com/channel").is_err());
    }

    #[test]
    fn trims_only_terminal_punctuation() {
        assert_eq!(
            trim_link_punctuation("magnet:?xt=urn:btih:ABC）。"),
            "magnet:?xt=urn:btih:ABC"
        );
    }

    #[test]
    fn preserves_existing_115_password() {
        let value = append_115_password("https://115.com/s/example?password=1234", Some("5678"));
        assert!(value.contains("password=1234"));
        assert!(!value.contains("5678"));
    }

    #[test]
    fn offline_links_use_transfer_directory_and_schedule_flowlink() {
        for value in [
            "magnet:?xt=urn:btih:ABCDEF",
            "ed2k://|file|demo.mkv|1|HASH|/",
        ] {
            let link = classify_resource_link(value).unwrap();
            let payload = mediary_link_submit_payload(&link);
            assert_eq!(payload["link"], value);
            assert_eq!(payload["offline_target"], "transfer");
            assert_eq!(
                payload["flowlink_move_all_delay_seconds"],
                FLOWLINK_MOVE_ALL_DELAY_SECONDS
            );
        }
    }

    #[test]
    fn share_links_keep_the_flowlink_share_submission_contract() {
        let link = classify_resource_link("https://115.com/s/example").unwrap();
        let payload = mediary_link_submit_payload(&link);
        assert_eq!(payload["link"], "https://115.com/s/example");
        assert!(payload.get("offline_target").is_none());
        assert!(payload.get("flowlink_move_all_delay_seconds").is_none());
    }

    #[test]
    fn parses_gimy_and_regeng_media_hints() {
        let gimy = telegram_media_hint_from_text(
            "[剧集]：黑色丽人 (2024)\nS03E01-E08 · 完结\n🍿 TMDB ID：246246",
        )
        .unwrap();
        assert_eq!(gimy["tmdb_id"], 246246);
        assert_eq!(gimy["media_type"], "tv");
        assert_eq!(gimy["title"], "黑色丽人");
        assert_eq!(gimy["year"], 2024);

        let regeng = telegram_media_hint_from_text(
            "📺 剧集：不眠 (2021) S03E02\n⭐️ TMDB评分：https://www.themoviedb.org/tv/126167\nED2K: ed2k://|file|demo.{tmdbid-126167}.mkv|1|HASH|/",
        )
        .unwrap();
        assert_eq!(regeng["tmdb_id"], 126167);
        assert_eq!(regeng["season"], 3);
        assert_eq!(regeng["episodes"], serde_json::json!([2]));
    }

    #[test]
    fn ed2k_carries_exact_file_name_and_byte_size() {
        let link = "ed2k://|file|%E4%B8%8D%E7%9C%A0.2021.S03E02.1080p.mkv|1450891201|26b06170a0c58e02d54e3e2dc793bf88|/";
        let (name, size) = parse_ed2k_file(link).unwrap();
        assert_eq!(name, "不眠.2021.S03E02.1080p.mkv");
        assert_eq!(size, 1_450_891_201);
    }

    #[test]
    fn extracts_complete_ed2k_links_when_file_names_contain_spaces() {
        let link = "ed2k://|file|抓特务 (2026) - 2160p.WEB-DL.DoVi.HEVC.60fps.EAC3 5.1.{tmdb-1305672}.mp4|30465381435|d4b2c5098fdde7ecda64f93d62e08073|/";
        let links = extract_resource_links(link, &[link.to_string()]);
        assert_eq!(links.len(), 1);
        assert_eq!(links[0].value, link);
        let (name, size) = parse_ed2k_file(&links[0].value).unwrap();
        assert_eq!(
            name,
            "抓特务 (2026) - 2160p.WEB-DL.DoVi.HEVC.60fps.EAC3 5.1.{tmdb-1305672}.mp4"
        );
        assert_eq!(size, 30_465_381_435);
    }

    #[test]
    fn parses_bundle_size_quality_and_episode_range() {
        let text = "[剧集]：黑色丽人 (2024)\nS03E01-E08 · 完结\n4K Netflix WEB-DL DV.HDR10 EAC3 5.1\n文件：8 个 ｜ 大小：50.17 GB\nTMDB ID：246246";
        let metadata = telegram_media_metadata_from_text(text).unwrap();
        assert_eq!(metadata.title, "黑色丽人");
        assert_eq!(metadata.season, Some(3));
        assert_eq!(metadata.episode, Some(1));
        assert_eq!(metadata.end_episode, Some(8));
        assert_eq!(metadata.quality, "4K Netflix WEB-DL DV.HDR10 EAC3 5.1");
        assert_eq!(metadata.size, size_from_message(text).unwrap());
        assert!(metadata.size > 50 * 1024 * 1024 * 1024);
    }

    #[test]
    fn gimy_bundle_and_episode_become_distinct_cloudhub_resources() {
        let metadata = TelegramMediaMetadata {
            title: "黑色丽人".to_string(),
            tmdb_id: 246246,
            media_type: "tv".to_string(),
            year: Some(2024),
            season: Some(3),
            episode: Some(1),
            end_episode: Some(8),
            quality: "4K Netflix WEB-DL".to_string(),
            size: 50 * 1024 * 1024 * 1024,
        };
        let share = classify_resource_link("https://115cdn.com/s/example?password=1234").unwrap();
        let ed2k = classify_resource_link(
            "ed2k://|file|Black.Beauty.S03E01.2160p.mkv|6533701550|7fcf681faadc5a6a8c163881bb9cadce|/",
        )
        .unwrap();
        let bundle = direct_resource_payload(&metadata, &share, "gimy100", 1940).unwrap();
        let episode = direct_resource_payload(&metadata, &ed2k, "gimy100", 1940).unwrap();
        assert_eq!(bundle.payload["virtual_owner"], "share115");
        assert_eq!(bundle.payload["is_full_season"], true);
        assert_eq!(bundle.payload["episode"], 0);
        assert_eq!(episode.payload["virtual_owner"], "ed2k");
        assert_eq!(episode.payload["episode"], 1);
        assert_eq!(episode.payload["size"], 6_533_701_550_i64);
        assert_ne!(bundle.payload["sha1"], episode.payload["sha1"]);
    }

    #[test]
    fn realtime_reporter_accepts_only_configured_channel_peer_ids() {
        let configured = PeerId::channel(12345).unwrap();
        let other_channel = PeerId::channel(67890).unwrap();
        let private_chat = PeerId::user(12345).unwrap();
        let peers = HashMap::from([(configured, "gimy100".to_string())]);
        assert_eq!(report_channel_for_peer(&peers, configured), Some("gimy100"));
        assert_eq!(report_channel_for_peer(&peers, other_channel), None);
        assert_eq!(report_channel_for_peer(&peers, private_chat), None);
    }
}
