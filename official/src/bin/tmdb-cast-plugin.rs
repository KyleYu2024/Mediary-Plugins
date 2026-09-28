use reqwest::Client;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{
    collections::HashMap,
    env,
    io::{self, Read},
    time::Duration,
};

const BATCH_SIZE: usize = 50;
const DEFAULT_BATCH_LIMIT: usize = 150;

#[derive(Deserialize)]
struct Settings {
    #[serde(default = "default_language")]
    language: String,
    #[serde(default = "default_cast_limit")]
    cast_limit: usize,
    #[serde(default = "default_batch_limit")]
    batch_limit: usize,
}
fn default_language() -> String {
    "zh-CN".into()
}
fn default_cast_limit() -> usize {
    15
}
fn default_batch_limit() -> usize {
    DEFAULT_BATCH_LIMIT
}

#[derive(Deserialize)]
struct PendingResponse {
    items: Vec<PendingItem>,
}
#[derive(Deserialize)]
struct PendingItem {
    id: String,
    item_type: String,
    tmdb_id: i32,
    admitted_at: String,
}
#[derive(Deserialize)]
struct CreditsResponse {
    #[serde(default)]
    cast: Vec<CastPerson>,
}
#[derive(Deserialize)]
struct CastPerson {
    id: i32,
    name: String,
    profile_path: Option<String>,
    character: Option<String>,
    #[serde(default)]
    roles: Vec<CastRole>,
    #[serde(default)]
    order: i32,
}
#[derive(Deserialize)]
struct CastRole {
    character: Option<String>,
}
#[derive(Clone, Deserialize)]
struct PersonDetails {
    #[serde(default)]
    biography: String,
    birthday: Option<String>,
    place_of_birth: Option<String>,
    profile_path: Option<String>,
}
#[derive(Serialize)]
struct CastMember {
    tmdb_id: i32,
    name: String,
    profile_path: Option<String>,
    biography: Option<String>,
    birthday: Option<String>,
    place_of_birth: Option<String>,
    character: Option<String>,
    order: i32,
}

#[tokio::main]
async fn main() {
    match run().await {
        Ok(report) => println!("{}", report),
        Err(error) => {
            eprintln!("{error}");
            std::process::exit(1);
        }
    }
}

async fn run() -> Result<Value, String> {
    if env::var("MEDIARY_PLUGIN_ACTION").as_deref() != Ok("sync") {
        return Err("不支持的动作".into());
    }
    let mut input = String::new();
    io::stdin()
        .read_to_string(&mut input)
        .map_err(|err| err.to_string())?;
    let settings: Settings = serde_json::from_str(&required_env("MEDIARY_PLUGIN_SETTINGS_JSON")?)
        .map_err(|err| format!("插件设置无效: {err}"))?;
    let api_url = required_env("MEDIARY_PLUGIN_API_URL")?;
    let token = required_env("MEDIARY_PLUGIN_TOKEN")?;
    let client = Client::builder()
        .timeout(Duration::from_secs(100))
        .build()
        .map_err(|err| err.to_string())?;
    let limit = settings.cast_limit.clamp(1, 30);
    let batch_limit = settings.batch_limit;
    let mut after_at = String::new();
    let mut after_id = String::new();
    let mut succeeded = 0;
    let mut failed = 0;
    let mut processed = 0;
    let mut people_cache = HashMap::new();
    while batch_limit == 0 || processed < batch_limit {
        let pending = client
            .get(format!("{api_url}/plugin/media-cast/pending"))
            .bearer_auth(&token)
            .query(&[
                ("after_at", after_at.as_str()),
                ("after_id", after_id.as_str()),
                ("limit", "50"),
            ])
            .send()
            .await
            .map_err(|err| format!("读取待同步作品失败: {err}"))?
            .error_for_status()
            .map_err(|_| "读取待同步作品失败：Mediary API 返回错误".to_string())?
            .json::<PendingResponse>()
            .await
            .map_err(|err| err.to_string())?;
        if pending.items.is_empty() {
            break;
        }
        let batch_len = pending.items.len();
        for item in pending.items {
            after_at = item.admitted_at.clone();
            after_id = item.id.clone();
            processed += 1;
            match sync_item(
                &client,
                &api_url,
                &token,
                &settings,
                limit,
                &item,
                &mut people_cache,
            )
            .await
            {
                Ok(()) => succeeded += 1,
                Err(error) => {
                    failed += 1;
                    eprintln!("作品 {} 演员同步失败: {error}", item.id);
                }
            }
            if batch_limit != 0 && processed >= batch_limit {
                break;
            }
        }
        if batch_len < BATCH_SIZE {
            break;
        }
    }
    Ok(
        json!({"notice": format!("演员同步完成：成功 {succeeded}，失败 {failed}"),
        "report": {"processed":processed,"succeeded":succeeded,"failed":failed}}),
    )
}

async fn sync_item(
    client: &Client,
    api_url: &str,
    token: &str,
    settings: &Settings,
    limit: usize,
    item: &PendingItem,
    people_cache: &mut HashMap<i32, PersonDetails>,
) -> Result<(), String> {
    let media_type = match item.item_type.as_str() {
        "movie" => "movie",
        "series" => "tv",
        _ => return Err("不支持的媒体类型".into()),
    };
    let response = client
        .get(format!(
            "{api_url}/plugin/tmdb/credits/{media_type}/{}",
            item.tmdb_id
        ))
        .bearer_auth(token)
        .query(&[("language", settings.language.as_str())])
        .send()
        .await
        .map_err(|_| "主程序 TMDB 演员请求失败".to_string())?;
    let response = host_response(response, "获取演员数据")
        .await?
        .json::<CreditsResponse>()
        .await
        .map_err(|_| "TMDB 演员数据解析失败".to_string())?;
    let mut cast = Vec::new();
    for person in response
        .cast
        .into_iter()
        .filter(|person| person.id > 0 && !person.name.trim().is_empty())
        .take(limit)
    {
        // Fetch biographies for leading cast; other cast still get portrait and local works.
        let details = if cast.len() < 5 && !people_cache.contains_key(&person.id) {
            client
                .get(format!("{api_url}/plugin/tmdb/person/{}", person.id))
                .bearer_auth(token)
                .query(&[("language", settings.language.as_str())])
                .send()
                .await
                .ok()
                .and_then(|response| response.error_for_status().ok())
        } else {
            None
        };
        let details = match details {
            Some(response) => response.json::<PersonDetails>().await.ok(),
            None => None,
        };
        if let Some(details) = details {
            people_cache.insert(person.id, details);
        }
        let details = people_cache.get(&person.id);
        cast.push(CastMember {
            tmdb_id: person.id,
            name: person.name,
            profile_path: details
                .as_ref()
                .and_then(|value| value.profile_path.clone())
                .or(person.profile_path),
            biography: details
                .as_ref()
                .map(|value| value.biography.clone())
                .filter(|value| !value.is_empty()),
            birthday: details.as_ref().and_then(|value| value.birthday.clone()),
            place_of_birth: details
                .as_ref()
                .and_then(|value| value.place_of_birth.clone()),
            character: person
                .character
                .or_else(|| person.roles.into_iter().find_map(|role| role.character)),
            order: person.order,
        });
    }
    client
        .put(format!("{api_url}/plugin/media-cast/{}", item.id))
        .bearer_auth(token)
        .json(&json!({"tmdb_id":item.tmdb_id,"cast":cast}))
        .send()
        .await
        .map_err(|_| "写入演员资料失败".to_string())?
        .error_for_status()
        .map_err(|_| "Mediary 拒绝写入演员资料".to_string())?;
    Ok(())
}

async fn host_response(
    response: reqwest::Response,
    operation: &str,
) -> Result<reqwest::Response, String> {
    if response.status().is_success() {
        return Ok(response);
    }
    let message = response
        .json::<Value>()
        .await
        .ok()
        .and_then(|value| {
            value
                .get("error")
                .and_then(Value::as_str)
                .map(str::to_string)
        })
        .unwrap_or_else(|| "主程序接口返回错误".to_string());
    Err(format!("{operation}失败：{message}"))
}

fn required_env(key: &str) -> Result<String, String> {
    env::var(key).map_err(|_| format!("缺少运行环境变量 {key}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tv_aggregate_roles_are_parsed() {
        let response: CreditsResponse = serde_json::from_str(
            r#"{"cast":[{"id":7,"name":"Actor","profile_path":"/actor.jpg","roles":[{"character":"Lead"}],"order":0}]}"#,
        )
        .unwrap();
        let actor = response.cast.into_iter().next().unwrap();
        assert_eq!(actor.roles[0].character.as_deref(), Some("Lead"));
        assert_eq!(actor.character, None);
    }
}
