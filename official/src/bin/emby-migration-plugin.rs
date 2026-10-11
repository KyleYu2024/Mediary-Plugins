//! Thin interactive client: the host owns background reads and transactional imports.
use reqwest::Client;
use serde_json::{Value, json};
use std::{env, time::Duration};

#[tokio::main]
async fn main() {
    if let Err(error) = run().await {
        eprintln!("{error}");
        std::process::exit(1);
    }
}
async fn run() -> Result<(), String> {
    let action = env::var("MEDIARY_PLUGIN_ACTION").unwrap_or_default();
    if action != "manage" {
        return Err("不支持的迁移动作".into());
    }
    let input = std::io::read_to_string(std::io::stdin()).map_err(|_| "读取动作参数失败")?;
    let payload: Value = if input.trim().is_empty() {
        json!({})
    } else {
        serde_json::from_str(&input).map_err(|_| "动作参数格式无效")?
    };
    let settings: Value = serde_json::from_str(
        &env::var("MEDIARY_PLUGIN_SETTINGS_JSON").unwrap_or_else(|_| "{}".into()),
    )
    .map_err(|_| "插件设置格式无效")?;
    let api = env::var("MEDIARY_PLUGIN_API_URL").map_err(|_| "缺少插件 API 地址")?;
    let token = env::var("MEDIARY_PLUGIN_TOKEN").map_err(|_| "缺少插件令牌")?;
    let client = Client::builder()
        .timeout(Duration::from_secs(120))
        .build()
        .map_err(|_| "无法建立连接")?;
    let operation = payload["operation"].as_str().unwrap_or("status");
    let supplied_id = payload["job_id"].as_str().unwrap_or("").trim();
    let job = if supplied_id.is_empty() {
        "latest"
    } else {
        supplied_id
    };
    let base = format!("{}/plugin/emby-migration/jobs", api.trim_end_matches('/'));
    if operation == "discover" {
        let discovery = request_json(
            client
                .post(format!(
                    "{}/plugin/emby-migration/discover",
                    api.trim_end_matches('/')
                ))
                .bearer_auth(&token)
                .json(&json!({"base_url":settings["base_url"],"api_key":settings["api_key"]})),
        )
        .await?;
        println!("{}", json!({"discovery":discovery}));
        return Ok(());
    }
    if operation == "raw_status" {
        let job = request_json(
            client
                .get(format!("{}/{}", base, url_component(job)))
                .bearer_auth(&token),
        )
        .await?;
        println!("{}", json!({"job":job}));
        return Ok(());
    }
    let value = match operation {
        "preview" => {
            let request = migration_request(&settings, &payload)?;
            request_json(client.post(&base).bearer_auth(&token).json(&request)).await?
        }
        "import" => {
            if supplied_id.is_empty() {
                return Err("执行迁移必须填写已核对的预览任务 ID".into());
            }
            if payload["confirm"].as_bool() != Some(true) {
                return Err("请先核对预览报告并勾选确认迁移".into());
            }
            request_json(client.post(format!("{}/{}/import",base,url_component(job))).bearer_auth(&token)
                .json(&json!({"allow_unmatched":payload["allow_unmatched"].as_bool().unwrap_or(false),
                    "replace_unchanged":payload["replace_unchanged"].as_bool().unwrap_or(false)}))).await?
        }
        "status" | "report" => {
            let result = request_json(
                client
                    .get(format!("{}/{}", base, url_component(job)))
                    .bearer_auth(&token)
                    .query(&[("full", operation == "report")]),
            )
            .await?;
            if operation == "report" {
                let dir = std::path::PathBuf::from(
                    env::var("MEDIARY_PLUGIN_DATA_DIR").map_err(|_| "缺少插件数据目录")?,
                );
                std::fs::create_dir_all(&dir).map_err(|_| "无法创建报告目录")?;
                let temporary = dir.join("migration-report.json.tmp");
                std::fs::write(
                    &temporary,
                    serde_json::to_vec_pretty(&result).map_err(|_| "无法序列化报告")?,
                )
                .map_err(|_| "保存迁移报告失败")?;
                std::fs::rename(temporary, dir.join("migration-report.json"))
                    .map_err(|_| "保存迁移报告失败")?;
                println!(
                    "{}",
                    json!({"notice":"完整报告已保存到插件数据目录 migration-report.json"})
                );
                return Ok(());
            }
            result
        }
        _ => return Err("不支持的迁移操作".into()),
    };
    println!("{}", render(&value));
    Ok(())
}
fn migration_request(settings: &Value, payload: &Value) -> Result<Value, String> {
    let mut request = json!({"base_url":settings["base_url"],"api_key":settings["api_key"]});
    for key in ["library_map", "user_map", "path_map", "username_map"] {
        request[key] = if let Some(value) = payload.get(key) {
            if !value.is_object() {
                return Err("迁移映射格式无效".into());
            }
            value.clone()
        } else {
            parse_mapping(settings, key)?
        };
    }
    request["user_ids"] = if let Some(ids) = payload.get("user_ids") {
        ids.clone()
    } else {
        parse_user_ids(settings)?
    };
    Ok(request)
}
fn parse_user_ids(settings: &Value) -> Result<Value, String> {
    let raw = settings["user_ids"].as_str().unwrap_or("").trim();
    if raw.is_empty() {
        return Ok(json!([]));
    }
    let ids: Vec<String> =
        serde_json::from_str(raw).map_err(|_| "所选用户必须为 ID 字符串组成的 JSON 数组")?;
    Ok(json!(ids))
}
fn parse_mapping(settings: &Value, key: &str) -> Result<Value, String> {
    match &settings[key] {
        Value::Null => Ok(json!({})),
        Value::String(raw) if raw.trim().is_empty() => Ok(json!({})),
        Value::String(raw) => {
            let value: Value =
                serde_json::from_str(raw).map_err(|_| format!("{key} 必须为有效 JSON 对象"))?;
            if !value.is_object() {
                return Err(format!("{key} 必须为 JSON 对象"));
            }
            Ok(value)
        }
        value if value.is_object() => Ok(value.clone()),
        _ => Err(format!("{key} 必须为 JSON 对象")),
    }
}
fn url_component(value: &str) -> String {
    // Job identifiers only; never an arbitrary URL/path supplied to the host.
    value
        .bytes()
        .map(|byte| {
            if byte.is_ascii_alphanumeric() || byte == b'-' {
                (byte as char).to_string()
            } else {
                format!("%{byte:02X}")
            }
        })
        .collect()
}
async fn request_json(request: reqwest::RequestBuilder) -> Result<Value, String> {
    let response = request
        .send()
        .await
        .map_err(|_| "无法连接 Mediary 迁移接口")?;
    let status = response.status();
    let value: Value = response
        .json()
        .await
        .map_err(|_| "主程序未提供迁移接口，请先更新 Mediary")?;
    if !status.is_success() {
        return Err(value["error"]
            .as_str()
            .unwrap_or("迁移请求失败，请检查插件权限")
            .into());
    }
    Ok(value)
}
fn render(value: &Value) -> Value {
    let status = value["status"].as_str().unwrap_or("unknown");
    let label = match status {
        "empty" => "尚未创建预览",
        "reading" => "正在读取",
        "ready" => "预览完成，待导入",
        "importing" => "正在导入",
        "completed" => "迁移完成",
        "failed" => "读取失败",
        _ => "状态未知",
    };
    let id = value["id"].as_str().unwrap_or("");
    let report = &value["report"];
    let summary = &report["summary"];
    let mut items = vec![
        json!({"id":"job","title":format!("迁移任务：{label}"),"subtitle":if id.is_empty() {"请先在插件配置中填写 Emby 连接，再创建迁移预览。".to_string()} else {format!("任务 ID：{id}")},"badges":[label]}),
    ];
    if let Some(error) = report["error"].as_str() {
        items.push(json!({"id":"error","title":error}));
    }
    if let Some(error) = report["import_error"].as_str() {
        items.push(json!({"id":"import-error","title":format!("导入未执行成功：{error}"),"subtitle":"失败事务已回滚；请修正映射或导入选项后重试。"}));
    }
    if !summary.is_null() {
        items.push(json!({"id":"summary","title":format!("用户 {} · 已匹配状态 {} · 未匹配 {} · 已有状态 {}",summary["users"],summary["matched"],summary["unmatched"],summary["existing_states"]),
            "subtitle":format!("演员收藏 {} / {}；存在冲突的用户 {}。完整报告可通过“保存完整报告”获取。",summary["matched_person_favorites"],summary["person_favorites"],summary["blocked_users"])}));
    }
    if let Some(folders) = report["folders"].as_array() {
        for folder in folders {
            let source = folder["ItemId"].as_str().unwrap_or("");
            items.push(json!({"id":format!("folder-{source}"),"title":format!("Emby 库：{}",folder["Name"].as_str().unwrap_or("")),
                "subtitle":format!("源 ID：{source}；目标 ID：{}",report["library_map"][source])}));
        }
    }
    if let Some(libraries) = report["local_libraries"].as_array() {
        for library in libraries {
            items.push(json!({"id":format!("library-{}",library["id"]),"title":format!("Mediary 库：{}",library["name"].as_str().unwrap_or("")),"subtitle":format!("目标 ID：{}",library["id"])}));
        }
    }
    if let Some(users) = report["local_users"].as_array() {
        for user in users {
            items.push(json!({"id":format!("local-user-{}",user["id"]),"title":format!("Mediary 现有用户：{}",user["username"].as_str().unwrap_or("")),"subtitle":format!("用户映射目标数字 ID：{}",user["id"])}));
        }
    }
    if let Some(users) = report["users"].as_array() {
        for user in users {
            let name = user["username"].as_str().unwrap_or("");
            items.push(json!({"id":format!("user-{}",user["source_id"]),"title":name,
                "subtitle":format!("源 ID：{}；目标：{}；{}",user["source_id"],user["target_id"],user["error"].as_str().unwrap_or("可导入；新账号首次登录使用原 Emby 密码"))}));
            if let Some(warnings) = user["warnings"].as_array() {
                for (i, warning) in warnings.iter().enumerate() {
                    items.push(json!({"id":format!("warning-{name}-{i}"),"title":format!("{name}：{}",warning.as_str().unwrap_or(""))}));
                }
            }
            if let Some(states) = user["states"].as_array() {
                for state in states.iter().filter(|state| state["target_id"].is_null()) {
                    items.push(json!({"id":format!("unmatched-{name}-{}",state["source_id"]),"title":format!("未匹配：{}",state["name"].as_str().unwrap_or("")),"subtitle":state["reason"]}));
                }
            }
        }
    }
    if !report["result"].is_null() {
        items.push(
            json!({"id":"result","title":"迁移结果","subtitle":report["result"].to_string()}),
        );
    }
    if items.len() > 1000 {
        items.truncate(1000);
        items
            .push(json!({"id":"truncated","title":"界面详情已截断，请保存完整报告查看全部记录。"}));
    }
    json!({"job":value,"items":items,"refreshing":matches!(status,"reading"|"importing"),"refresh_after_ms":2000,
        "form_values":{"operation":"status","job_id":id,"confirm":false},"notice":if status == "reading" {"正在后台读取，请选择刷新状态；尚未写入用户数据。"} else if status == "importing" {"正在后台事务导入，请刷新状态查看结果。"} else {"预览不修改用户；导入须填写任务 ID 并确认。重复导入不会重复创建账号。"}})
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn maps_require_json_objects() {
        assert_eq!(
            parse_mapping(&json!({"x":"{\"a\":\"b\"}"}), "x").unwrap(),
            json!({"a":"b"})
        );
        assert!(parse_mapping(&json!({"x":"[]"}), "x").is_err());
        assert!(parse_mapping(&json!({"x":"bad"}), "x").is_err());
    }
    #[test]
    fn reports_show_ids_and_errors_without_credentials() {
        let value = render(&json!({"id":"task-1","status":"failed","report":{"error":"认证失败"}}));
        assert!(value.to_string().contains("task-1"));
        assert!(value.to_string().contains("认证失败"));
    }
}
