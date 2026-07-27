// SPDX-License-Identifier: GPL-3.0-only

mod client;
mod engine;
mod models;
mod rules;
mod store;
mod view;

use serde_json::{Value, json};
use std::{env, io::Read, path::PathBuf};

use client::MediaryClient;
use engine::{check_all, run_all, tick};
use models::{PluginSettings, State, Task};
use store::StateStore;
use view::{dashboard_response, edit_response};

pub async fn run() -> Result<(), String> {
    let action = required_env("MEDIARY_PLUGIN_ACTION")?;
    let data_dir = PathBuf::from(required_env("MEDIARY_PLUGIN_DATA_DIR")?);
    let settings = env::var("MEDIARY_PLUGIN_SETTINGS_JSON")
        .ok()
        .and_then(|value| serde_json::from_str::<PluginSettings>(&value).ok())
        .unwrap_or_default();
    let payload = read_payload()?;
    let client = MediaryClient::from_env()?;
    let store = StateStore::open(data_dir)?;
    let mut guard = store.lock()?;
    let mut state = guard.load()?;

    let response = match action.as_str() {
        "dashboard" => {
            let saved = save_task_from_payload(&mut state, &payload)?;
            guard.save(&mut state)?;
            let sites = client.sites().await.unwrap_or_default();
            let downloaders = client.downloader_info().await.unwrap_or_default();
            dashboard_response(
                &state,
                &sites,
                &downloaders,
                saved.then_some("任务已保存"),
                saved,
            )
        }
        "edit_task" => edit_task(&state, &payload)?,
        "prepare_task" => {
            let site_id = payload
                .get("site_id")
                .and_then(Value::as_i64)
                .ok_or_else(|| "缺少站点 ID".to_string())?;
            let site_name = payload
                .get("site_name")
                .and_then(Value::as_str)
                .unwrap_or("PT 站点");
            let downloaders = client.downloader_info().await.unwrap_or_default();
            view::prepare_response(&state, site_id, site_name, &downloaders.default)
        }
        "new_task" => {
            let downloaders = client.downloader_info().await.unwrap_or_default();
            view::new_task_response(&state, &downloaders.default)
        }
        "toggle_task" => {
            let id = payload_id(&payload)?;
            let task = state
                .tasks
                .iter_mut()
                .find(|task| task.id == id)
                .ok_or_else(|| "未找到刷流任务".to_string())?;
            task.enabled = !task.enabled;
            guard.save(&mut state)?;
            let sites = client.sites().await.unwrap_or_default();
            let downloaders = client.downloader_info().await.unwrap_or_default();
            dashboard_response(&state, &sites, &downloaders, Some("任务状态已更新"), false)
        }
        "delete_task" => {
            let id = payload_id(&payload)?;
            state.tasks.retain(|task| task.id != id);
            guard.save(&mut state)?;
            let sites = client.sites().await.unwrap_or_default();
            let downloaders = client.downloader_info().await.unwrap_or_default();
            dashboard_response(
                &state,
                &sites,
                &downloaders,
                Some("任务已删除，托管记录予以保留"),
                false,
            )
        }
        "clear_task" => {
            let id = payload_id(&payload)?;
            state.torrents.retain(|torrent| torrent.task_id != id);
            state.runs.retain(|run| run.task_id != id);
            guard.save(&mut state)?;
            let sites = client.sites().await.unwrap_or_default();
            let downloaders = client.downloader_info().await.unwrap_or_default();
            dashboard_response(
                &state,
                &sites,
                &downloaders,
                Some("任务统计与托管记录已清除"),
                false,
            )
        }
        "run_task" => {
            let id = payload_id(&payload)?;
            let report = engine::run_one(&client, &settings, &mut state, &id).await?;
            guard.save(&mut state)?;
            json!({"notice": report.message, "items": view::task_items(&state)})
        }
        "check_task" => {
            let id = payload_id(&payload)?;
            let report = engine::check_one(&client, &settings, &mut state, &id).await?;
            guard.save(&mut state)?;
            json!({"notice": report.message, "items": view::task_items(&state)})
        }
        "run_all" => {
            let notice = run_all(&client, &settings, &mut state).await;
            guard.save(&mut state)?;
            json!({"notice": notice, "items": view::task_items(&state)})
        }
        "check_all" => {
            let notice = check_all(&client, &settings, &mut state).await;
            guard.save(&mut state)?;
            json!({"notice": notice, "items": view::task_items(&state)})
        }
        "tick" => {
            let notice = tick(&client, &settings, &mut state).await;
            guard.save(&mut state)?;
            json!({"notice": notice})
        }
        _ => return Err(format!("不支持的 PT 流量管家动作: {action}")),
    };
    guard.write_dashboard(&state)?;
    println!("{response}");
    Ok(())
}

fn save_task_from_payload(state: &mut State, payload: &Value) -> Result<bool, String> {
    if payload
        .get("name")
        .and_then(Value::as_str)
        .is_none_or(|name| name.trim().is_empty())
    {
        return Ok(false);
    }
    let mut task: Task = serde_json::from_value(payload.clone())
        .map_err(|error| format!("任务配置无效: {error}"))?;
    task.normalize_and_validate()?;
    if let Some(existing) = state.tasks.iter_mut().find(|item| item.id == task.id) {
        *existing = task;
    } else {
        state.tasks.push(task);
    }
    Ok(true)
}

fn edit_task(state: &State, payload: &Value) -> Result<Value, String> {
    let id = payload_id(payload)?;
    let task = state
        .tasks
        .iter()
        .find(|task| task.id == id)
        .ok_or_else(|| "未找到刷流任务".to_string())?;
    Ok(edit_response(state, task))
}

fn payload_id(payload: &Value) -> Result<String, String> {
    payload
        .get("task_id")
        .or_else(|| payload.get("id"))
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_string)
        .ok_or_else(|| "缺少任务 ID".to_string())
}

fn read_payload() -> Result<Value, String> {
    let mut input = String::new();
    std::io::stdin()
        .read_to_string(&mut input)
        .map_err(|error| error.to_string())?;
    if input.trim().is_empty() {
        Ok(json!({}))
    } else {
        serde_json::from_str(&input).map_err(|error| format!("动作参数不是有效 JSON: {error}"))
    }
}

fn required_env(name: &str) -> Result<String, String> {
    env::var(name)
        .ok()
        .filter(|value| !value.trim().is_empty())
        .ok_or_else(|| format!("缺少环境变量 {name}"))
}
