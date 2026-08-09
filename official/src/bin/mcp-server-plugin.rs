#[path = "mcp-server-plugin/api.rs"]
mod api;
#[path = "mcp-server-plugin/protocol.rs"]
mod protocol;
#[path = "mcp-server-plugin/registry.rs"]
mod registry;
#[path = "mcp-server-plugin/tools.rs"]
mod tools;

use api::PluginContext;
use serde_json::{Value, json};
use std::env;

fn base_url<'a>(api_url: &'a str, public_url: Option<&'a str>) -> &'a str {
    public_url
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(|value| value.trim_end_matches('/'))
        .unwrap_or_else(|| {
            api_url
                .trim_end_matches('/')
                .strip_suffix("/api")
                .unwrap_or_else(|| api_url.trim_end_matches('/'))
        })
}

fn status_result(base_url: &str, trigger: &str) -> Value {
    let endpoint = format!("{}/mcp", base_url.trim_end_matches('/'));
    let openclaw_template = serde_json::to_string_pretty(&json!({
        "mcp": {
            "servers": {
                "mediary": {
                    "url": endpoint,
                    "transport": "streamable-http",
                    "headers": {"Authorization": "Bearer <Mediary API Token>"}
                }
            }
        }
    }))
    .unwrap_or_default();
    let hermes_template = format!(
        "mcp_servers:\n  mediary:\n    url: \"{endpoint}\"\n    headers:\n      Authorization: \"Bearer <Mediary API Token>\""
    );
    let generic_template = serde_json::to_string_pretty(&json!({
        "mcpServers": {
            "mediary": {
                "url": endpoint,
                "headers": {"Authorization": "Bearer <Mediary API Token>"}
            }
        }
    }))
    .unwrap_or_default();

    json!({
        "notice": format!("MCP 服务已启用，端点: {endpoint}"),
        "items": [
            {
                "title": "连接信息",
                "subtitle": format!("端点: {endpoint}\n鉴权: Authorization: Bearer <Mediary API Token>\nToken 在 Mediary 设置 -> API Token 中获取"),
                "metadata": [],
                "actions": []
            },
            {
                "title": "OpenClaw 配置",
                "subtitle": openclaw_template,
                "metadata": [],
                "actions": []
            },
            {
                "title": "Hermes 配置",
                "subtitle": hermes_template,
                "metadata": [],
                "actions": []
            },
            {
                "title": "通用 MCP 客户端配置",
                "subtitle": generic_template,
                "metadata": [],
                "actions": []
            }
        ],
        "report": {
            "tools": registry::define_tools().len(),
            "protocol_versions": protocol::SUPPORTED_PROTOCOL_VERSIONS,
            "trigger": trigger
        }
    })
}

async fn run() -> Result<(), String> {
    let api_url = env::var("MEDIARY_PLUGIN_API_URL")
        .map_err(|_| "缺少 MEDIARY_PLUGIN_API_URL 环境变量".to_string())?;
    let api_token = env::var("MEDIARY_PLUGIN_TOKEN")
        .map_err(|_| "缺少 MEDIARY_PLUGIN_TOKEN 环境变量".to_string())?;
    let action = env::var("MEDIARY_PLUGIN_ACTION").unwrap_or_default();
    let trigger = env::var("MEDIARY_PLUGIN_TRIGGER").unwrap_or_default();

    let settings_json = env::var("MEDIARY_PLUGIN_SETTINGS_JSON").unwrap_or_default();
    let settings: Value = serde_json::from_str(&settings_json).unwrap_or(Value::Null);
    let public_url = settings.get("public_url").and_then(Value::as_str);

    if action == "status" {
        println!(
            "{}",
            status_result(base_url(&api_url, public_url), &trigger)
        );
        return Ok(());
    }

    if action != "mcp" {
        return Err(format!("不支持的插件动作: {action}"));
    }

    let ctx = PluginContext::new(api_url, api_token)?;
    let input = std::io::read_to_string(std::io::stdin())
        .map_err(|error| format!("读取请求失败: {error}"))?;
    if let Some(response) = protocol::handle_input(&ctx, &input).await {
        println!(
            "{}",
            serde_json::to_string(&response).map_err(|error| format!("序列化响应失败: {error}"))?
        );
    }
    Ok(())
}

#[tokio::main]
async fn main() {
    if let Err(error) = run().await {
        eprintln!("{error}");
        std::process::exit(1);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn derives_public_base_url_without_truncating_other_api_text() {
        assert_eq!(
            base_url("http://127.0.0.1:8118/api", None),
            "http://127.0.0.1:8118"
        );
        assert_eq!(
            base_url("http://example.test/capistrano", None),
            "http://example.test/capistrano"
        );
        assert_eq!(
            base_url("http://internal/api", Some(" https://media.example/m/ ")),
            "https://media.example/m"
        );
    }

    #[test]
    fn generates_native_openclaw_and_hermes_configuration() {
        let result = status_result("https://media.example", "manual");
        let items = result["items"].as_array().unwrap();
        assert!(
            items[1]["subtitle"]
                .as_str()
                .unwrap()
                .contains("\"transport\": \"streamable-http\"")
        );
        assert!(items[1]["subtitle"].as_str().unwrap().contains("\"mcp\""));
        assert!(
            items[2]["subtitle"]
                .as_str()
                .unwrap()
                .starts_with("mcp_servers:")
        );
        assert_eq!(result["report"]["tools"], 18);
    }

    #[test]
    fn manifest_and_protocol_report_the_same_plugin_version() {
        let manifest: Value =
            serde_json::from_str(include_str!("../../mcp-server/plugin.json")).unwrap();
        assert_eq!(manifest["version"], protocol::SERVER_VERSION);
    }
}
