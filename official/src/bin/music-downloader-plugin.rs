use reqwest::{Client, Response, Url, header};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};
use std::{
    env,
    fs::{self, File, OpenOptions},
    io::{Read, Write},
    path::{Path, PathBuf},
    process, thread,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

const DEFAULT_BASE_URL: &str = "https://flac.music.hi.cn";
const USER_AGENT: &str =
    "Mozilla/5.0 (X11; Linux x86_64) AppleWebKit/537.36 Chrome/137 Safari/537.36";

#[derive(Clone, Deserialize)]
struct Settings {
    #[serde(default = "default_base_url")]
    base_url: String,
    #[serde(default)]
    cookie: String,
    #[serde(default = "default_limit")]
    result_limit: usize,
    #[serde(default = "default_platform")]
    platform: String,
    #[serde(default = "default_quality")]
    quality: String,
    #[serde(default)]
    download_path: String,
    #[serde(default = "default_filename_format")]
    filename_format: String,
    #[serde(default)]
    overwrite: bool,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
struct Track {
    id: String,
    source: String,
    title: String,
    artist: String,
    album: String,
    #[serde(default)]
    url: String,
    #[serde(default)]
    time: u64,
    #[serde(default)]
    sign: String,
    #[serde(default)]
    minfo: Vec<MediaInfo>,
}

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
struct MediaInfo {
    #[serde(default)]
    format: String,
    #[serde(default)]
    bitrate: String,
    #[serde(default)]
    size: String,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
struct Job {
    id: String,
    track: Track,
    quality: String,
    state: String,
    created_at: u64,
    #[serde(default)]
    error: String,
    #[serde(default)]
    output: String,
    #[serde(default)]
    downloaded_bytes: u64,
    #[serde(default)]
    total_bytes: u64,
}

struct Context {
    settings: Settings,
    data_dir: PathBuf,
    client: Client,
}

#[tokio::main]
async fn main() {
    if let Err(error) = run().await {
        eprintln!("{error}");
        process::exit(1);
    }
}

async fn run() -> Result<(), String> {
    let context = Context::from_env()?;
    match env::var("MEDIARY_PLUGIN_ACTION").ok().as_deref() {
        Some("search") => print_json(search(&context, &read_payload()?).await?),
        Some("download") => print_json(enqueue(&context, &read_payload()?)?),
        Some("status") => print_json(job_status(&context, &read_payload()?)?),
        Some(action) if !action.is_empty() => Err(format!("不支持的音乐下载动作: {action}")),
        _ => worker(context).await,
    }
}

impl Context {
    fn from_env() -> Result<Self, String> {
        let settings = env::var("MEDIARY_PLUGIN_SETTINGS_JSON")
            .ok()
            .and_then(|raw| serde_json::from_str(&raw).ok())
            .unwrap_or_else(default_settings);
        validate_base_url(&settings.base_url)?;
        let data_dir = env::var("MEDIARY_PLUGIN_DATA_DIR")
            .map(PathBuf::from)
            .map_err(|_| "缺少 MEDIARY_PLUGIN_DATA_DIR".to_string())?;
        let client = Client::builder()
            .connect_timeout(Duration::from_secs(12))
            .timeout(Duration::from_secs(120))
            .user_agent(USER_AGENT)
            .redirect(reqwest::redirect::Policy::limited(8))
            .build()
            .map_err(|error| format!("创建 HTTP 客户端失败: {error}"))?;
        Ok(Self {
            settings,
            data_dir,
            client,
        })
    }
}

async fn search(context: &Context, payload: &Value) -> Result<Value, String> {
    let query = required_text(payload, "query")?;
    let limit = context.settings.result_limit.clamp(1, 100);
    let endpoint = endpoint(&context.settings.base_url, "search")?;
    let response = with_site_headers(
        context.client.post(endpoint).form(&[
            ("platform", context.settings.platform.as_str()),
            ("keyword", query.as_str()),
            ("page", "1"),
            ("size", &limit.to_string()),
        ]),
        context,
    )
    .send()
    .await
    .map_err(|error| format!("请求音乐站失败: {error}"))?;
    let value = json_response(response, "搜索音乐").await?;
    let tracks = normalize_tracks(&value, limit, &context.settings.platform);
    let items = tracks.into_iter().map(track_item).collect::<Vec<_>>();
    Ok(json!({"items": items}))
}

fn track_item(track: Track) -> Value {
    let title = track.title.clone();
    let mut metadata = Vec::new();
    if !track.artist.is_empty() {
        metadata.push(track.artist.clone());
    }
    if !track.album.is_empty() {
        metadata.push(track.album.clone());
    }
    if !track.source.is_empty() {
        metadata.push(track.source.clone());
    }
    let qualities = track
        .minfo
        .iter()
        .filter_map(|info| {
            let format = info.format.trim().to_ascii_uppercase();
            if format.is_empty() {
                return None;
            }
            if format == "FLAC" {
                Some("FLAC".to_string())
            } else if info.bitrate.trim().is_empty() {
                Some(format)
            } else {
                Some(format!("{} {}K", format, info.bitrate.trim()))
            }
        })
        .collect::<Vec<_>>();
    if !qualities.is_empty() {
        metadata.push(format!("音质：{}", qualities.join(" · ")));
    }
    let badge = track
        .minfo
        .iter()
        .find(|info| info.format.eq_ignore_ascii_case("flac"))
        .map(|_| "FLAC")
        .unwrap_or("音乐");
    let actions = download_actions(&track);
    json!({
        "key": format!("{}:{}", track.source, track.id),
        "title": title,
        "subtitle": track.artist,
        "badges": [{"label": badge, "tone": "success"}],
        "metadata": metadata,
        "actions": actions
    })
}

fn download_actions(track: &Track) -> Vec<Value> {
    let mut qualities = track
        .minfo
        .iter()
        .filter_map(quality_key)
        .collect::<Vec<_>>();
    qualities.dedup();
    if qualities.is_empty() {
        qualities.push("flac".into());
    }
    qualities
        .into_iter()
        .map(|quality| {
            json!({
                "type": "plugin_action",
                "action": "download",
                "label": format!("下载 {}", quality_label(&quality)),
                "pending_label": "提交中",
                "icon": "download",
                "tone": "success",
                "payload": {"track": track, "quality": quality},
                "success_message": "已加入后台下载队列。",
                "error_message": "提交下载失败。"
            })
        })
        .collect()
}

fn quality_key(info: &MediaInfo) -> Option<String> {
    let format = info.format.trim().to_ascii_lowercase();
    if format.is_empty() {
        None
    } else if format == "flac" {
        Some("flac".into())
    } else if info.bitrate.trim().is_empty() {
        Some(format)
    } else {
        Some(info.bitrate.trim().into())
    }
}

fn quality_label(quality: &str) -> String {
    if quality == "flac" {
        "FLAC".into()
    } else {
        format!("MP3 {}K", quality)
    }
}

fn enqueue(context: &Context, payload: &Value) -> Result<Value, String> {
    validate_download_path(&context.settings.download_path)?;
    let track_value = payload
        .get("track")
        .ok_or_else(|| "下载参数缺少 track".to_string())?;
    let track: Track = serde_json::from_value(track_value.clone())
        .map_err(|error| format!("无效的歌曲参数: {error}"))?;
    validate_track(&track)?;
    let quality = payload
        .get("quality")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .unwrap_or(&context.settings.quality)
        .to_ascii_lowercase();
    if !track.minfo.is_empty()
        && !track
            .minfo
            .iter()
            .filter_map(quality_key)
            .any(|value| value == quality)
    {
        return Err("所选音质不在该歌曲的可用列表中".into());
    }
    let jobs_dir = context.data_dir.join("jobs");
    fs::create_dir_all(&jobs_dir).map_err(|error| format!("创建下载队列目录失败: {error}"))?;
    let now = unix_time();
    let id = format!("{now}-{}-{}", process::id(), stable_suffix(&track));
    let job = Job {
        id: id.clone(),
        track,
        quality,
        state: "queued".into(),
        created_at: now,
        error: String::new(),
        output: String::new(),
        downloaded_bytes: 0,
        total_bytes: 0,
    };
    atomic_json(&jobs_dir.join(format!("{id}.json")), &job)?;
    Ok(job_status_response(
        &job,
        Some(format!("已加入后台下载队列：{}", job.track.title)),
    ))
}

fn job_status(context: &Context, payload: &Value) -> Result<Value, String> {
    let id = required_text(payload, "job_id")?;
    if !id.chars().all(|ch| ch.is_ascii_digit() || ch == '-') {
        return Err("无效的下载任务 ID".into());
    }
    let job = read_job(&context.data_dir.join("jobs").join(format!("{id}.json")))?;
    Ok(job_status_response(&job, None))
}

fn job_status_response(job: &Job, notice: Option<String>) -> Value {
    let (label, tone) = match job.state.as_str() {
        "completed" => ("已完成", "success"),
        "failed" => ("失败", "danger"),
        "downloading" => ("下载中", "info"),
        _ => ("排队中", "neutral"),
    };
    let progress = if job.total_bytes > 0 {
        format!(
            "{:.1}% ({}/{})",
            job.downloaded_bytes as f64 * 100.0 / job.total_bytes as f64,
            human_bytes(job.downloaded_bytes),
            human_bytes(job.total_bytes)
        )
    } else if job.downloaded_bytes > 0 {
        human_bytes(job.downloaded_bytes)
    } else {
        "-".into()
    };
    let mut metadata = vec![
        json!({"label": "歌手", "value": if job.track.artist.is_empty() { "-" } else { &job.track.artist }}),
        json!({"label": "进度", "value": progress}),
    ];
    if !job.output.is_empty() {
        metadata.push(json!({"label": "文件", "value": job.output}));
    }
    if !job.error.is_empty() {
        metadata.push(json!({"label": "错误", "value": job.error}));
    }
    json!({
        "notice": notice,
        "items": [{
            "key": format!("job:{}", job.id),
            "title": job.track.title,
            "subtitle": "下载任务",
            "badges": [{"label": label, "tone": tone}],
            "metadata": metadata,
            "actions": [{
                "type": "plugin_action",
                "action": "status",
                "label": "刷新状态",
                "icon": "refresh",
                "payload": {"job_id": job.id}
            }]
        }]
    })
}

async fn worker(context: Context) -> Result<(), String> {
    let jobs_dir = context.data_dir.join("jobs");
    fs::create_dir_all(&jobs_dir).map_err(|error| format!("创建队列目录失败: {error}"))?;
    recover_interrupted_jobs(&jobs_dir)?;
    loop {
        if let Some(path) = next_job(&jobs_dir)? {
            if let Err(error) = process_job(&context, &path).await {
                eprintln!("音乐下载任务失败: {error}");
            }
        } else {
            thread::sleep(Duration::from_secs(2));
        }
    }
}

fn recover_interrupted_jobs(jobs_dir: &Path) -> Result<(), String> {
    for entry in fs::read_dir(jobs_dir)
        .map_err(|error| format!("读取队列目录失败: {error}"))?
        .filter_map(Result::ok)
    {
        let path = entry.path();
        if path.extension().and_then(|value| value.to_str()) != Some("json") {
            continue;
        }
        let Ok(mut job) = read_job(&path) else {
            continue;
        };
        if job.state == "downloading" {
            job.state = "queued".into();
            job.error.clear();
            atomic_json(&path, &job)?;
        }
    }
    Ok(())
}

fn next_job(jobs_dir: &Path) -> Result<Option<PathBuf>, String> {
    let mut paths = fs::read_dir(jobs_dir)
        .map_err(|error| format!("读取队列目录失败: {error}"))?
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .filter(|path| path.extension().and_then(|value| value.to_str()) == Some("json"))
        .collect::<Vec<_>>();
    paths.sort();
    for path in paths {
        if read_job(&path).is_ok_and(|job| job.state == "queued") {
            return Ok(Some(path));
        }
    }
    Ok(None)
}

async fn process_job(context: &Context, path: &Path) -> Result<(), String> {
    let mut job = read_job(path)?;
    job.state = "downloading".into();
    atomic_json(path, &job)?;
    let result = download_track(context, &mut job, path).await;
    match result {
        Ok(output) => {
            job.state = "completed".into();
            job.output = output.to_string_lossy().into_owned();
        }
        Err(error) => {
            job.state = "failed".into();
            job.error = error.clone();
            atomic_json(path, &job)?;
            return Err(error);
        }
    }
    atomic_json(path, &job)
}

async fn download_track(
    context: &Context,
    job: &mut Job,
    job_path: &Path,
) -> Result<PathBuf, String> {
    let download_dir = validate_download_path(&context.settings.download_path)?;
    fs::create_dir_all(&download_dir).map_err(|error| format!("创建下载目录失败: {error}"))?;
    let (url, hinted_ext) = resolve_media(context, &job.track, &job.quality).await?;
    let response = with_site_headers(context.client.get(url.clone()), context)
        .send()
        .await
        .map_err(|error| format!("下载音频失败: {error}"))?;
    let status = response.status();
    if !status.is_success() {
        return Err(format!("下载音频失败: HTTP {}", status.as_u16()));
    }
    let content_type = response
        .headers()
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_string();
    job.total_bytes = response.content_length().unwrap_or(0);
    job.downloaded_bytes = 0;
    atomic_json(job_path, job)?;
    if content_type.contains("text/html") || content_type.contains("application/json") {
        return Err(format!(
            "站点未返回音频文件（Content-Type: {content_type}）"
        ));
    }
    let extension = media_extension(&url, &content_type, &hinted_ext, &job.quality);
    let stem = filename_stem(&job.track, &context.settings.filename_format);
    let final_path = available_path(&download_dir, &stem, &extension, context.settings.overwrite)?;
    let part_path = final_path.with_extension(format!("{extension}.part"));
    let mut file =
        File::create(&part_path).map_err(|error| format!("创建临时文件失败: {error}"))?;
    let mut response = response;
    let mut last_reported = 0;
    while let Some(chunk) = response
        .chunk()
        .await
        .map_err(|error| format!("读取音频数据失败: {error}"))?
    {
        file.write_all(&chunk)
            .map_err(|error| format!("写入 NAS 文件失败: {error}"))?;
        job.downloaded_bytes = job.downloaded_bytes.saturating_add(chunk.len() as u64);
        if job.downloaded_bytes.saturating_sub(last_reported) >= 1024 * 1024 {
            atomic_json(job_path, job)?;
            last_reported = job.downloaded_bytes;
        }
    }
    atomic_json(job_path, job)?;
    file.sync_all()
        .map_err(|error| format!("同步下载文件失败: {error}"))?;
    drop(file);
    fs::rename(&part_path, &final_path).map_err(|error| format!("完成文件改名失败: {error}"))?;
    Ok(final_path)
}

async fn resolve_media(
    context: &Context,
    track: &Track,
    quality: &str,
) -> Result<(Url, String), String> {
    let media = select_media_info(track, quality);
    let format = media
        .map(|value| value.format.as_str())
        .unwrap_or_else(|| if quality == "flac" { "flac" } else { "mp3" });
    let bitrate = media.map(|value| value.bitrate.as_str()).unwrap_or(quality);
    let time = track.time.to_string();
    let platform = if track.source.is_empty() {
        context.settings.platform.as_str()
    } else {
        track.source.as_str()
    };
    let endpoint = endpoint(&context.settings.base_url, "getUrl")?;
    let response = with_site_headers(
        context.client.post(endpoint).form(&[
            ("platform", platform),
            ("songid", track.id.as_str()),
            ("format", format),
            ("bitrate", bitrate),
            ("time", time.as_str()),
            ("sign", track.sign.as_str()),
        ]),
        context,
    )
    .send()
    .await
    .map_err(|error| format!("解析下载地址失败: {error}"))?;
    let value = json_response(response, "解析下载地址").await?;
    let url_text = find_recursive_text(&value, &["url", "link", "download_url", "src"])
        .ok_or_else(|| "站点响应中没有音频下载地址".to_string())?;
    let url = Url::parse(&url_text).map_err(|_| "站点返回了无效的下载地址".to_string())?;
    if !matches!(url.scheme(), "http" | "https") {
        return Err("只允许 HTTP/HTTPS 音频地址".into());
    }
    let ext = media
        .map(|value| value.format.clone())
        .or_else(|| find_recursive_text(&value, &["ext", "extension", "format"]))
        .unwrap_or_else(|| format.to_string());
    Ok((url, ext))
}

fn select_media_info<'a>(track: &'a Track, quality: &str) -> Option<&'a MediaInfo> {
    track
        .minfo
        .iter()
        .find(|info| {
            (quality == "flac" && info.format.eq_ignore_ascii_case("flac"))
                || (quality != "flac" && info.bitrate == quality)
        })
        .or_else(|| track.minfo.first())
}

fn normalize_tracks(value: &Value, limit: usize, platform: &str) -> Vec<Track> {
    let arrays = collect_candidate_arrays(value);
    let mut tracks = Vec::new();
    for array in arrays {
        for item in array {
            if tracks.len() >= limit {
                return tracks;
            }
            let Some(object) = item.as_object() else {
                continue;
            };
            let id = object_text(object, &["id", "songid", "song_id", "rid", "mid"]);
            let title = object_text(object, &["name", "title", "songname", "song_name"]);
            if id.is_empty() || title.is_empty() {
                continue;
            }
            tracks.push(Track {
                id,
                title,
                source: {
                    let source = object_text(object, &["source", "platform", "site", "type"]);
                    if source.is_empty() {
                        platform.to_string()
                    } else {
                        source
                    }
                },
                artist: object_text(object, &["artist", "author", "singer", "artist_name"]),
                album: object_text(object, &["album", "album_name"]),
                url: object_text(object, &["url", "download_url", "link", "src"]),
                time: object
                    .get("time")
                    .and_then(Value::as_u64)
                    .unwrap_or_default(),
                sign: object_text(object, &["sign"]),
                minfo: object
                    .get("minfo")
                    .cloned()
                    .and_then(|value| serde_json::from_value(value).ok())
                    .unwrap_or_default(),
            });
        }
    }
    tracks
}

fn collect_candidate_arrays(value: &Value) -> Vec<&Vec<Value>> {
    let mut found = Vec::new();
    fn walk<'a>(value: &'a Value, found: &mut Vec<&'a Vec<Value>>, depth: usize) {
        if depth > 4 {
            return;
        }
        match value {
            Value::Array(array) => {
                found.push(array);
            }
            Value::Object(object) => {
                for (key, child) in object {
                    if ["data", "result", "results", "list", "items", "songs"]
                        .contains(&key.as_str())
                    {
                        walk(child, found, depth + 1);
                    }
                }
            }
            _ => {}
        }
    }
    walk(value, &mut found, 0);
    found
}

fn object_text(object: &Map<String, Value>, keys: &[&str]) -> String {
    for key in keys {
        if let Some(value) = object.get(*key) {
            if let Some(text) = scalar_text(value)
                && !text.trim().is_empty()
            {
                return text;
            }
            if let Some(array) = value.as_array() {
                let joined = array
                    .iter()
                    .filter_map(scalar_or_named_text)
                    .collect::<Vec<_>>()
                    .join(" / ");
                if !joined.is_empty() {
                    return joined;
                }
            }
        }
    }
    String::new()
}

fn scalar_or_named_text(value: &Value) -> Option<String> {
    scalar_text(value).or_else(|| {
        value.as_object().and_then(|obj| {
            ["name", "title"]
                .iter()
                .find_map(|key| obj.get(*key).and_then(scalar_text))
        })
    })
}

fn scalar_text(value: &Value) -> Option<String> {
    match value {
        Value::String(s) => Some(s.trim().to_string()),
        Value::Number(n) => Some(n.to_string()),
        _ => None,
    }
}

fn find_recursive_text(value: &Value, keys: &[&str]) -> Option<String> {
    fn walk(value: &Value, keys: &[&str], depth: usize) -> Option<String> {
        if depth > 5 {
            return None;
        }
        let object = value.as_object()?;
        for key in keys {
            if let Some(text) = object
                .get(*key)
                .and_then(scalar_text)
                .filter(|s| !s.is_empty())
            {
                return Some(text);
            }
        }
        for child in object.values() {
            if let Some(text) = walk(child, keys, depth + 1) {
                return Some(text);
            }
        }
        None
    }
    walk(value, keys, 0)
}

fn with_site_headers(
    builder: reqwest::RequestBuilder,
    context: &Context,
) -> reqwest::RequestBuilder {
    let builder = builder.header("X-Requested-With", "XMLHttpRequest").header(
        header::REFERER,
        format!("{}/", context.settings.base_url.trim_end_matches('/')),
    );
    if context.settings.cookie.trim().is_empty() {
        builder
    } else {
        builder.header(header::COOKIE, context.settings.cookie.trim())
    }
}

async fn json_response(response: Response, action: &str) -> Result<Value, String> {
    let status = response.status();
    let content_type = response
        .headers()
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_string();
    let body = response
        .text()
        .await
        .map_err(|error| format!("读取{action}响应失败: {error}"))?;
    if content_type.contains("text/html") || body.trim_start().starts_with('<') {
        return Err(format!("{action}被站点防护拦截，请在插件设置中更新 Cookie"));
    }
    if !status.is_success() {
        return Err(format!("{action}失败: HTTP {}", status.as_u16()));
    }
    serde_json::from_str(&body).map_err(|error| format!("解析{action}响应失败: {error}"))
}

fn endpoint(base: &str, action: &str) -> Result<Url, String> {
    let mut url = Url::parse(&format!("{}/ajax.php", base.trim_end_matches('/')))
        .map_err(|_| "音乐站地址无效".to_string())?;
    url.query_pairs_mut().append_pair("act", action);
    Ok(url)
}

fn filename_stem(track: &Track, format: &str) -> String {
    let title = primary_song_title(&track.title);
    let raw = if format == "artist-title" && !track.artist.trim().is_empty() {
        format!("{} - {}", track.artist, title)
    } else {
        title
    };
    sanitize_filename(&raw)
}

fn primary_song_title(value: &str) -> String {
    let value = value.trim();
    if let Some(bracket) = value.find('《') {
        let prefix = value[..bracket].trim_end_matches([' ', '-', '_']);
        if !prefix.is_empty() && prefix.len() < bracket {
            return prefix.to_string();
        }
    }
    value.to_string()
}

fn sanitize_filename(value: &str) -> String {
    let sanitized = value
        .chars()
        .map(|ch| match ch {
            '/' | '\\' | ':' | '*' | '?' | '"' | '<' | '>' | '|' | '\0' => '_',
            ch if ch.is_control() => '_',
            _ => ch,
        })
        .collect::<String>();
    let sanitized = sanitized
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .trim_matches([' ', '.'])
        .to_string();
    if sanitized.is_empty() {
        "未命名歌曲".into()
    } else {
        sanitized.chars().take(180).collect()
    }
}

fn media_extension(url: &Url, content_type: &str, hint: &str, quality: &str) -> String {
    let hint = hint.trim().trim_start_matches('.').to_ascii_lowercase();
    if ["flac", "mp3", "m4a", "aac", "wav", "ogg", "opus"].contains(&hint.as_str()) {
        return hint;
    }
    let path_ext = Path::new(url.path())
        .extension()
        .and_then(|v| v.to_str())
        .unwrap_or("")
        .to_ascii_lowercase();
    if ["flac", "mp3", "m4a", "aac", "wav", "ogg", "opus"].contains(&path_ext.as_str()) {
        return path_ext;
    }
    if content_type.contains("flac") {
        "flac".into()
    } else if content_type.contains("mp4") {
        "m4a".into()
    } else if content_type.contains("wav") {
        "wav".into()
    } else if quality.eq_ignore_ascii_case("flac") {
        "flac".into()
    } else {
        "mp3".into()
    }
}

fn available_path(dir: &Path, stem: &str, ext: &str, overwrite: bool) -> Result<PathBuf, String> {
    let first = dir.join(format!("{stem}.{ext}"));
    if overwrite || !first.exists() {
        return Ok(first);
    }
    for index in 2..=9999 {
        let candidate = dir.join(format!("{stem} ({index}).{ext}"));
        if !candidate.exists() {
            return Ok(candidate);
        }
    }
    Err("同名文件过多，无法生成新文件名".into())
}

fn validate_download_path(value: &str) -> Result<PathBuf, String> {
    let path = PathBuf::from(value.trim());
    if value.trim().is_empty() {
        return Err("请先配置 NAS 下载目录".into());
    }
    if !path.is_absolute() {
        return Err("NAS 下载目录必须是容器内的绝对路径".into());
    }
    if path == Path::new("/") {
        return Err("不允许将根目录作为下载目录".into());
    }
    Ok(path)
}

fn validate_base_url(value: &str) -> Result<(), String> {
    let url = Url::parse(value.trim()).map_err(|_| "音乐站地址无效".to_string())?;
    if !matches!(url.scheme(), "http" | "https") || url.host_str().is_none() {
        return Err("音乐站地址必须是 HTTP/HTTPS URL".into());
    }
    Ok(())
}

fn validate_track(track: &Track) -> Result<(), String> {
    if track.id.trim().is_empty() || track.title.trim().is_empty() {
        return Err("歌曲 ID 和歌名不能为空".into());
    }
    if track.id.len() > 512 || track.title.len() > 512 || track.source.len() > 128 {
        return Err("歌曲参数过长".into());
    }
    Ok(())
}

fn atomic_json(path: &Path, value: &impl Serialize) -> Result<(), String> {
    let temp = path.with_extension("json.tmp");
    let bytes =
        serde_json::to_vec_pretty(value).map_err(|error| format!("序列化任务失败: {error}"))?;
    let mut file = OpenOptions::new()
        .create(true)
        .truncate(true)
        .write(true)
        .open(&temp)
        .map_err(|error| format!("写入任务失败: {error}"))?;
    file.write_all(&bytes)
        .and_then(|_| file.sync_all())
        .map_err(|error| format!("写入任务失败: {error}"))?;
    fs::rename(temp, path).map_err(|error| format!("保存任务失败: {error}"))
}

fn read_job(path: &Path) -> Result<Job, String> {
    let mut raw = String::new();
    File::open(path)
        .and_then(|mut f| f.read_to_string(&mut raw))
        .map_err(|error| format!("读取任务失败: {error}"))?;
    serde_json::from_str(&raw).map_err(|error| format!("解析任务失败: {error}"))
}

fn read_payload() -> Result<Value, String> {
    let mut raw = String::new();
    std::io::stdin()
        .read_to_string(&mut raw)
        .map_err(|error| format!("读取动作参数失败: {error}"))?;
    serde_json::from_str(&raw).map_err(|error| format!("动作参数不是有效 JSON: {error}"))
}

fn required_text(value: &Value, key: &str) -> Result<String, String> {
    value
        .get(key)
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .ok_or_else(|| format!("{key} 不能为空"))
}

fn print_json(value: Value) -> Result<(), String> {
    println!("{value}");
    Ok(())
}
fn unix_time() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}
fn stable_suffix(track: &Track) -> u64 {
    track
        .id
        .bytes()
        .chain(track.source.bytes())
        .fold(1469598103934665603u64, |hash, byte| {
            (hash ^ u64::from(byte)).wrapping_mul(1099511628211)
        })
}
fn default_base_url() -> String {
    DEFAULT_BASE_URL.into()
}
fn default_limit() -> usize {
    30
}
fn default_platform() -> String {
    "kuwo".into()
}
fn default_quality() -> String {
    "flac".into()
}
fn default_filename_format() -> String {
    "title".into()
}
fn default_settings() -> Settings {
    Settings {
        base_url: default_base_url(),
        cookie: String::new(),
        result_limit: default_limit(),
        platform: default_platform(),
        quality: default_quality(),
        download_path: String::new(),
        filename_format: default_filename_format(),
        overwrite: false,
    }
}

fn human_bytes(bytes: u64) -> String {
    const UNITS: [&str; 4] = ["B", "KB", "MB", "GB"];
    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit < UNITS.len() - 1 {
        value /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{bytes} B")
    } else {
        format!("{value:.1} {}", UNITS[unit])
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::TcpListener;

    #[test]
    fn normalizes_wrapped_search_results() {
        let value = json!({"code": 0, "data": {"list": [{"id": 7, "name": "夜曲", "artist": ["周杰伦"], "album": "十一月的萧邦", "source": "netease"}]}});
        let tracks = normalize_tracks(&value, 30, "kuwo");
        assert_eq!(tracks.len(), 1);
        assert_eq!(tracks[0].id, "7");
        assert_eq!(tracks[0].title, "夜曲");
        assert_eq!(tracks[0].artist, "周杰伦");
    }

    #[test]
    fn sanitizes_nas_filename_and_keeps_song_name() {
        assert_eq!(sanitize_filename("  A/B: C?  "), "A_B_ C_");
        let track = Track {
            id: "1".into(),
            source: "x".into(),
            title: "Song".into(),
            artist: "Artist".into(),
            album: String::new(),
            url: String::new(),
            time: 1,
            sign: "sign".into(),
            minfo: Vec::new(),
        };
        assert_eq!(filename_stem(&track, "title"), "Song");
        assert_eq!(filename_stem(&track, "artist-title"), "Artist - Song");
        assert_eq!(
            primary_song_title("十年-《明年今日》国语版_《隐婚男女》电影插曲"),
            "十年"
        );
    }

    #[test]
    fn rejects_relative_and_root_download_paths() {
        assert!(validate_download_path("music").is_err());
        assert!(validate_download_path("/").is_err());
        assert_eq!(
            validate_download_path("/media/music").unwrap(),
            PathBuf::from("/media/music")
        );
    }

    #[test]
    fn selects_extension_from_hint_url_or_content_type() {
        let url = Url::parse("https://cdn.test/a.mp3?token=x").unwrap();
        assert_eq!(media_extension(&url, "audio/mpeg", "", "flac"), "mp3");
        assert_eq!(media_extension(&url, "audio/mpeg", "flac", "320"), "flac");
    }

    #[tokio::test]
    async fn downloads_to_part_then_renames_by_song_title() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let server = thread::spawn(move || {
            let (mut resolver, _) = listener.accept().unwrap();
            let mut request = [0u8; 2048];
            let _ = resolver.read(&mut request).unwrap();
            let body = format!("{{\"code\":0,\"data\":{{\"url\":\"http://{address}/audio\"}}}}");
            resolver
                .write_all(format!("HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len()).as_bytes())
                .unwrap();
            let (mut media, _) = listener.accept().unwrap();
            let _ = media.read(&mut request).unwrap();
            media.write_all(b"HTTP/1.1 200 OK\r\nContent-Type: audio/flac\r\nContent-Length: 8\r\nConnection: close\r\n\r\nfLaCTest").unwrap();
        });
        let test_root = env::temp_dir().join(format!(
            "mediary-music-downloader-test-{}-{}",
            process::id(),
            unix_time()
        ));
        let download_dir = test_root.join("music");
        let settings = Settings {
            base_url: format!("http://{address}"),
            download_path: download_dir.to_string_lossy().into_owned(),
            ..default_settings()
        };
        let context = Context {
            settings,
            data_dir: test_root.join("data"),
            client: Client::builder().build().unwrap(),
        };
        let mut job = Job {
            id: "test".into(),
            track: Track {
                id: "1".into(),
                source: "test".into(),
                title: "A/B Song".into(),
                artist: "Artist".into(),
                album: String::new(),
                url: String::new(),
                time: 1,
                sign: "signed".into(),
                minfo: vec![MediaInfo {
                    format: "flac".into(),
                    bitrate: "2000".into(),
                    size: "8 B".into(),
                }],
            },
            quality: "flac".into(),
            state: "queued".into(),
            created_at: unix_time(),
            error: String::new(),
            output: String::new(),
            downloaded_bytes: 0,
            total_bytes: 0,
        };
        fs::create_dir_all(&context.data_dir).unwrap();
        let job_path = context.data_dir.join("job.json");
        atomic_json(&job_path, &job).unwrap();
        let output = download_track(&context, &mut job, &job_path).await.unwrap();
        server.join().unwrap();
        assert_eq!(output.file_name().unwrap(), "A_B Song.flac");
        assert_eq!(fs::read(&output).unwrap(), b"fLaCTest");
        assert!(!output.with_extension("flac.part").exists());
        fs::remove_dir_all(test_root).unwrap();
    }
}
