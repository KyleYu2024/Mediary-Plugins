// SPDX-License-Identifier: GPL-3.0-only

use serde_json::{Value, json};

use super::models::{DownloaderInfo, Site, State, Task};

pub fn dashboard_response(
    state: &State,
    sites: &[Site],
    downloaders: &DownloaderInfo,
    notice: Option<&str>,
    reset_form: bool,
) -> Value {
    let configured = state
        .tasks
        .iter()
        .map(|task| task.site_id)
        .collect::<std::collections::HashSet<_>>();
    let mut items = task_items(state);
    items.extend(
        sites
            .iter()
            .filter(|site| !configured.contains(&site.id))
            .map(|site| {
                json!({
                    "key": format!("site:{}", site.id),
                    "section": "可添加站点",
                    "title": site.name,
                    "subtitle": site.domain,
                    "badges": [{"label": format!("ID {}", site.id), "tone": "neutral"}],
                    "actions": [{
                        "type": "plugin_action",
                        "action": "prepare_task",
                        "label": "创建任务",
                        "icon": "plus",
                        "tone": "success",
                        "payload": {"site_id": site.id, "site_name": site.name}
                    }]
                })
            }),
    );
    json!({
        "notice": notice,
        "items": items,
        "form_reset": reset_form,
        "form_open": false,
        "form_options": {
            "site_id": sites.iter().map(|site| json!({
                "label": site.name,
                "value": site.id
            })).collect::<Vec<_>>(),
            "downloader": downloaders.items.iter().map(|item| json!({
                "label": item.name,
                "value": item.id
            })).collect::<Vec<_>>()
        },
        "actions": [
            action("run_all", "全部选种", "play", "success", json!({})),
            action("check_all", "全部检查", "refresh", "info", json!({})),
            action("new_task", "新建任务", "plus", "neutral", json!({}))
        ]
    })
}

pub fn task_items(state: &State) -> Vec<Value> {
    state
        .tasks
        .iter()
        .map(|task| {
            let records = state
                .torrents
                .iter()
                .filter(|torrent| torrent.task_id == task.id)
                .collect::<Vec<_>>();
            let active = records
                .iter()
                .filter(|torrent| torrent.deleted_at.is_none())
                .count();
            let deleted = records.len().saturating_sub(active);
            json!({
                "key": task.id,
                "section": "刷流任务",
                "title": task.name,
                "subtitle": format!(
                    "站点 ID {} · {} · {} · 每 {} 分钟检查",
                    task.site_id,
                    downloader_label(&task.downloader),
                    if task.rss_support {
                        "RSS"
                    } else if task.cron.trim().is_empty() {
                        "定时选种"
                    } else {
                        "CRON 选种"
                    },
                    task.check_interval
                ),
                "badges": [
                    {
                        "label": if task.enabled { "运行中" } else { "已暂停" },
                        "tone": if task.enabled { "success" } else { "warning" }
                    },
                    {"label": format!("托管 {active}"), "tone": "info"},
                    {"label": format!("已删 {deleted}"), "tone": "neutral"}
                ],
                "metadata": [
                    {"label": "促销", "value": promotion_label(&task.promotion)},
                    {"label": "选种计划", "value": if task.cron.trim().is_empty() {
                        format!("每 {} 分钟", task.brush_interval)
                    } else {
                        task.cron.clone()
                    }},
                    {"label": "分类", "value": task.category},
                    {"label": "动态删种", "value": if task.dynamic_delete { "开启" } else { "关闭" }}
                ],
                "actions": [
                    action("edit_task", "编辑", "edit", "neutral", json!({"task_id": task.id})),
                    action("run_task", "立即选种", "play", "success", json!({"task_id": task.id})),
                    action("check_task", "立即检查", "refresh", "info", json!({"task_id": task.id})),
                    action(
                        "toggle_task",
                        if task.enabled { "暂停" } else { "启用" },
                        if task.enabled { "pause" } else { "play" },
                        "warning",
                        json!({"task_id": task.id})
                    ),
                    {
                        "type": "plugin_action",
                        "action": "clear_task",
                        "label": "清除数据",
                        "icon": "reset",
                        "tone": "warning",
                        "payload": {"task_id": task.id},
                        "confirm": {
                            "title": "清除任务数据",
                            "message": "托管记录、统计和运行诊断将被清除，下载器任务不会被删除。",
                            "confirm_text": "清除",
                            "danger": true
                        }
                    },
                    {
                        "type": "plugin_action",
                        "action": "delete_task",
                        "label": "删除任务",
                        "icon": "trash",
                        "tone": "danger",
                        "payload": {"task_id": task.id},
                        "confirm": {
                            "title": "删除刷流任务",
                            "message": "任务配置将被删除，已有托管记录和下载器任务会保留。",
                            "confirm_text": "删除",
                            "danger": true
                        }
                    }
                ]
            })
        })
        .collect()
}

pub fn edit_response(state: &State, task: &Task) -> Value {
    json!({
        "notice": format!("正在编辑 {}", task.name),
        "items": task_items(state),
        "form_values": serde_json::to_value(task).unwrap_or_else(|_| json!({})),
        "form_open": true
    })
}

pub fn prepare_response(
    state: &State,
    site_id: i64,
    site_name: &str,
    default_downloader: &str,
) -> Value {
    let task = Task {
        name: format!("{site_name} 刷流"),
        site_id,
        downloader: default_downloader.to_string(),
        ..Task::default()
    };
    json!({
        "notice": format!("已载入 {site_name} 的新任务配置"),
        "items": task_items(state),
        "form_values": serde_json::to_value(task).unwrap_or_else(|_| json!({})),
        "form_open": true
    })
}

pub fn new_task_response(state: &State, default_downloader: &str) -> Value {
    let task = Task {
        downloader: default_downloader.to_string(),
        ..Task::default()
    };
    json!({
        "notice": "已清空任务表单",
        "items": task_items(state),
        "form_values": serde_json::to_value(task).unwrap_or_else(|_| json!({})),
        "form_open": true
    })
}

fn action(action: &str, label: &str, icon: &str, tone: &str, payload: Value) -> Value {
    json!({
        "type": "plugin_action",
        "action": action,
        "label": label,
        "icon": icon,
        "tone": tone,
        "payload": payload
    })
}

fn promotion_label(value: &str) -> &str {
    match value {
        "free" => "免费",
        "2xfree" => "2X 免费",
        _ => "全部",
    }
}

fn downloader_label(value: &str) -> &str {
    match value {
        "qbittorrent" => "qBittorrent",
        "transmission" => "Transmission",
        _ => "默认下载器",
    }
}
