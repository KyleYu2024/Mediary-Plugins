mod runtime;
mod store;

use serde_json::{Value, json};
use std::io::{self, Read};

fn main() {
    match run() {
        Ok(value) => println!("{}", value),
        Err(error) => {
            eprintln!("{}", error);
            std::process::exit(1)
        }
    }
}

fn run() -> Result<Value, String> {
    let mut input = String::new();
    io::stdin()
        .take(2 * 1024 * 1024 + 1)
        .read_to_string(&mut input)
        .map_err(|_| "读取输入失败")?;
    if input.len() > 2 * 1024 * 1024 {
        return Err("输入超过 2 MB".into());
    }
    let input: Value = serde_json::from_str(&input).map_err(|_| "输入必须为 JSON")?;
    let action = std::env::var("MEDIARY_PLUGIN_ACTION").unwrap_or_default();
    let mut store = store::Store::open()?;
    match action.as_str() {
        "site-dispatch" => {
            if input["contract_version"] != 1 {
                return Err("不支持的站点契约版本".into());
            }
            let operation = input["operation"].as_str().ok_or("缺少 operation")?;
            let site = &input["site"];
            let host = url::Url::parse(site["url"].as_str().ok_or("缺少站点 URL")?)
                .map_err(|_| "站点 URL 无效")?
                .host_str()
                .ok_or("站点缺少域名")?
                .to_ascii_lowercase();
            let selected = input["input"]["adapter_id"].as_str();
            let matches = store
                .entries
                .values()
                .filter(|entry| {
                    entry.enabled
                        && entry.package.domains.iter().any(|domain| domain == &host)
                        && entry
                            .package
                            .operations
                            .iter()
                            .any(|value| value == operation)
                        && selected.is_none_or(|id| id == entry.package.id)
                })
                .cloned()
                .collect::<Vec<_>>();
            if matches.len() > 1 {
                return Err("多个启用的适配包匹配同一站点和操作".into());
            }
            let Some(entry) = matches.first() else {
                return Ok(json!({"handled":false}));
            };
            // Release the file lock before any network request; unrelated sites remain concurrent.
            drop(store);
            let result = runtime::execute(entry, operation, site, &input["input"])?;
            let mut result = result.as_object().cloned().ok_or("适配脚本必须返回对象")?;
            result.insert("handled".into(), json!(true));
            result.insert("adapter_id".into(), json!(entry.package.id));
            Ok(Value::Object(result))
        }
        "list" => Ok(json!({"packages":store.entries.values().map(|entry| json!({
            "id":entry.package.id,"name":entry.package.name,"version":entry.package.version,
            "description":entry.package.description,"domains":entry.package.domains,
            "operations":entry.package.operations,"enabled":entry.enabled,
            "credential_domains":entry.package.credential_domains,
            "network_domains":entry.package.network_domains,
            "settings_fields":entry.package.settings_fields,
            "has_previous":entry.previous.is_some()
        })).collect::<Vec<_>>()})),
        "import" => {
            let package: store::Package = serde_json::from_value(input["package"].clone())
                .map_err(|e| format!("适配包结构无效: {e}"))?;
            store.import(package, input["replace"].as_bool().unwrap_or(false))?;
            Ok(json!({"notice":"适配包已导入。请检查规则后启用；替换已有包会保留本机配置。"}))
        }
        "export" => {
            let id = input["id"].as_str().ok_or("缺少适配包 ID")?;
            let entry = store.entries.get(id).ok_or("适配包不存在")?;
            Ok(json!({"package":entry.package}))
        }
        "toggle" => {
            let id = input["id"].as_str().ok_or("缺少适配包 ID")?;
            let enabled = input["enabled"].as_bool().ok_or("缺少 enabled")?;
            if enabled {
                store.check_conflict(id)?;
            }
            store.entries.get_mut(id).ok_or("适配包不存在")?.enabled = enabled;
            store.save()?;
            Ok(
                json!({"notice":if enabled {"适配已启用，下次站点请求生效"} else {"适配已停用，恢复内置处理"}}),
            )
        }
        "settings" => {
            let id = input["id"].as_str().ok_or("缺少适配包 ID")?;
            let entry = store.entries.get_mut(id).ok_or("适配包不存在")?;
            let settings = input["settings"]
                .as_object()
                .ok_or("配置必须为 JSON 对象")?;
            if settings.keys().any(|key| {
                !entry
                    .package
                    .settings_fields
                    .iter()
                    .any(|field| &field.key == key)
            }) {
                return Err("配置包含适配包未声明的字段".into());
            }
            // Merge: omitted fields preserve local secrets; explicit null deletes a field.
            for (key, value) in settings {
                if value.is_null() {
                    entry.settings.remove(key);
                } else {
                    entry.settings.insert(key.clone(), value.clone());
                }
            }
            store.save()?;
            Ok(json!({"notice":"本机配置已保存，不会随适配包导出"}))
        }
        "rollback" => {
            let id = input["id"].as_str().ok_or("缺少适配包 ID")?;
            let entry = store.entries.get_mut(id).ok_or("适配包不存在")?;
            let previous = entry.previous.take().ok_or("没有可回退版本")?;
            entry.previous = Some(std::mem::replace(&mut entry.package, previous));
            entry.enabled = false;
            store.save()?;
            Ok(json!({"notice":"已恢复上一份适配，请检查后重新启用"}))
        }
        "remove" => {
            let id = input["id"].as_str().ok_or("缺少适配包 ID")?;
            store.entries.remove(id).ok_or("适配包不存在")?;
            store.save()?;
            Ok(json!({"notice":"适配包及其本机配置已移除"}))
        }
        _ => Err("不支持的插件动作".into()),
    }
}
