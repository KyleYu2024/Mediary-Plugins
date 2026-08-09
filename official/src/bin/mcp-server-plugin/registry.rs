use serde::Serialize;
use serde_json::{Map, Value, json};

#[derive(Debug, Serialize)]
pub(crate) struct ToolDefinition {
    pub(crate) name: String,
    description: String,
    #[serde(rename = "inputSchema")]
    pub(crate) input_schema: Value,
}

fn tool(name: &str, description: &str, properties: Value, required: &[&str]) -> ToolDefinition {
    let mut schema = Map::from_iter([
        ("type".to_string(), json!("object")),
        ("properties".to_string(), properties),
        ("additionalProperties".to_string(), json!(false)),
    ]);
    if !required.is_empty() {
        schema.insert("required".to_string(), json!(required));
    }
    ToolDefinition {
        name: name.to_string(),
        description: description.to_string(),
        input_schema: Value::Object(schema),
    }
}

pub(crate) fn define_tools() -> Vec<ToolDefinition> {
    vec![
        tool(
            "catalog_search",
            "搜索 Mediary 媒体目录，根据关键词查找电影或电视剧。",
            json!({
                "query": {"type": "string", "description": "搜索关键词"},
                "media_type": {
                    "type": "string",
                    "description": "媒体类型",
                    "enum": ["multi", "movie", "tv"],
                    "default": "multi"
                },
                "limit": {
                    "type": "integer",
                    "description": "返回结果数量上限",
                    "minimum": 1,
                    "maximum": 100,
                    "default": 10
                }
            }),
            &["query"],
        ),
        tool(
            "subscriptions_list",
            "列出 Mediary 中的媒体订阅，可按 TMDB ID、媒体类型和季号筛选。",
            json!({
                "tmdb_id": {"type": "integer", "minimum": 1, "description": "TMDB ID"},
                "media_type": {"type": "string", "enum": ["movie", "tv"], "description": "媒体类型"},
                "season": {"type": "integer", "minimum": 1, "description": "季号"},
                "limit": {"type": "integer", "minimum": 1, "maximum": 1000, "default": 20}
            }),
            &[],
        ),
        tool(
            "downloads_list",
            "列出 Mediary 管理的下载任务及其进度。",
            json!({
                "status": {
                    "type": "string",
                    "description": "任务状态",
                    "enum": ["all", "downloading", "paused", "completed", "failed", "organizing", "removed"],
                    "default": "all"
                },
                "limit": {"type": "integer", "minimum": 1, "maximum": 200, "default": 20}
            }),
            &[],
        ),
        tool(
            "downloads_create",
            "向默认下载器提交磁力链接或种子下载链接。",
            json!({
                "url": {"type": "string", "description": "magnet: 链接或种子下载链接"},
                "save_path": {"type": "string", "description": "可选保存路径"},
                "category": {"type": "string", "description": "可选下载器分类"}
            }),
            &["url"],
        ),
        tool(
            "subscriptions_create",
            "创建电影或电视剧订阅；电视剧必须提供季号。",
            json!({
                "title": {"type": "string", "description": "媒体标题"},
                "media_type": {"type": "string", "enum": ["movie", "tv"], "default": "movie"},
                "tmdb_id": {"type": "integer", "minimum": 1, "description": "TMDB ID"},
                "year": {"type": "integer", "minimum": 1800, "maximum": 3000, "description": "发行年份"},
                "season": {"type": "integer", "minimum": 1, "description": "电视剧季号"}
            }),
            &["title", "tmdb_id"],
        ),
        tool(
            "subscriptions_delete",
            "永久删除指定的订阅记录。调用前应确认订阅 ID。",
            json!({"id": {"type": "integer", "minimum": 1, "description": "订阅 ID"}}),
            &["id"],
        ),
        tool(
            "sites_list",
            "列出已启用 PT 站点的脱敏状态信息，不返回 Cookie 或 API 密钥。",
            json!({}),
            &[],
        ),
        tool(
            "downloader_status",
            "获取已配置下载器及其支持的操作能力。",
            json!({}),
            &[],
        ),
        tool(
            "torrents_list",
            "列出 qBittorrent 或 Transmission 中当前存在的种子任务。",
            json!({
                "downloader": {
                    "type": "string",
                    "description": "可选下载器筛选",
                    "enum": ["qbittorrent", "transmission"]
                },
                "limit": {"type": "integer", "minimum": 1, "maximum": 200, "default": 20}
            }),
            &[],
        ),
        tool(
            "downloads_delete",
            "从下载器永久删除指定种子；默认同时删除数据文件。调用前必须向用户确认。",
            json!({
                "hash": {"type": "string", "description": "torrent hash"},
                "delete_files": {"type": "boolean", "description": "是否同时删除数据文件", "default": true}
            }),
            &["hash"],
        ),
        tool(
            "downloads_pause",
            "暂停指定的下载器种子任务。",
            json!({"hash": {"type": "string", "description": "torrent hash"}}),
            &["hash"],
        ),
        tool(
            "downloads_resume",
            "恢复指定的下载器种子任务。",
            json!({"hash": {"type": "string", "description": "torrent hash"}}),
            &["hash"],
        ),
        tool(
            "system_logs",
            "读取 Mediary 近期运行日志，用于状态检查和故障排查。",
            json!({
                "limit": {"type": "integer", "minimum": 1, "maximum": 800, "default": 50},
                "scope": {
                    "type": "string",
                    "enum": ["general", "cloudhub_broadcast", "pt_scheduled_fetch", "plugin", "all"],
                    "default": "all"
                }
            }),
            &[],
        ),
        tool(
            "filters_list",
            "列出 Mediary 当前的过滤规则配置。",
            json!({}),
            &[],
        ),
        tool(
            "filters_create",
            "创建自定义过滤规则。",
            filter_properties(),
            &["id", "name"],
        ),
        tool(
            "filters_update",
            "按 ID 更新自定义过滤规则，仅修改明确提供的字段。",
            filter_properties(),
            &["id"],
        ),
        tool(
            "filters_delete",
            "永久删除指定的自定义过滤规则。调用前应确认规则 ID。",
            json!({"id": {"type": "string", "description": "规则 ID"}}),
            &["id"],
        ),
        tool(
            "send_notification",
            "通过 Mediary 已配置的通知渠道发送消息。",
            json!({
                "title": {"type": "string", "minLength": 1, "maxLength": 120, "description": "通知标题"},
                "message": {"type": "string", "minLength": 1, "maxLength": 4000, "description": "通知正文"}
            }),
            &["title", "message"],
        ),
    ]
}

fn filter_properties() -> Value {
    json!({
        "id": {"type": "string", "description": "唯一规则 ID"},
        "name": {"type": "string", "description": "规则名称"},
        "include": {"type": ["string", "null"], "description": "必须包含的关键词表达式"},
        "exclude": {"type": ["string", "null"], "description": "必须排除的关键词表达式"},
        "size_range": {"type": ["string", "null"], "description": "体积范围表达式"},
        "seeders": {"type": ["string", "null"], "description": "做种人数表达式"}
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

    #[test]
    fn exposes_unique_strict_tool_schemas() {
        let tools = define_tools();
        assert_eq!(tools.len(), 18);
        assert_eq!(
            tools
                .iter()
                .map(|tool| tool.name.as_str())
                .collect::<HashSet<_>>()
                .len(),
            18
        );
        assert!(
            tools
                .iter()
                .all(|tool| tool.input_schema["type"] == "object")
        );
        assert!(
            tools
                .iter()
                .all(|tool| tool.input_schema["additionalProperties"] == false)
        );
    }

    #[test]
    fn documents_safe_sites_and_real_downloader_filter() {
        let tools = define_tools();
        let sites = tools.iter().find(|tool| tool.name == "sites_list").unwrap();
        let torrents = tools
            .iter()
            .find(|tool| tool.name == "torrents_list")
            .unwrap();
        assert!(sites.description.contains("不返回 Cookie"));
        assert!(
            torrents.input_schema["properties"]
                .get("downloader")
                .is_some()
        );
    }
}
