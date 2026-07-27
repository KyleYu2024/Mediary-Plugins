// SPDX-License-Identifier: GPL-3.0-only

use chrono::Utc;
use fs2::FileExt;
use serde_json::json;
use std::{
    fs::{self, File, OpenOptions},
    io,
    path::{Path, PathBuf},
};

use super::models::State;

pub struct StateStore {
    data_dir: PathBuf,
}

pub struct StateGuard {
    lock: File,
    state_path: PathBuf,
    dashboard_path: PathBuf,
}

impl StateStore {
    pub fn open(data_dir: PathBuf) -> Result<Self, String> {
        fs::create_dir_all(&data_dir).map_err(|error| error.to_string())?;
        Ok(Self { data_dir })
    }

    pub fn lock(&self) -> Result<StateGuard, String> {
        let lock_path = self.data_dir.join("state.lock");
        let lock = OpenOptions::new()
            .create(true)
            .read(true)
            .write(true)
            .truncate(false)
            .open(lock_path)
            .map_err(|error| error.to_string())?;
        lock.lock_exclusive()
            .map_err(|error| format!("获取插件状态锁失败: {error}"))?;
        Ok(StateGuard {
            lock,
            state_path: self.data_dir.join("state.json"),
            dashboard_path: self.data_dir.join("dashboard.json"),
        })
    }
}

impl StateGuard {
    pub fn load(&mut self) -> Result<State, String> {
        match fs::read_to_string(&self.state_path) {
            Ok(raw) => {
                serde_json::from_str(&raw).map_err(|error| format!("插件状态文件无效: {error}"))
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(State::default()),
            Err(error) => Err(error.to_string()),
        }
    }

    pub fn save(&mut self, state: &mut State) -> Result<(), String> {
        state.updated_at = Some(Utc::now());
        write_json_atomic(&self.state_path, state)
    }

    pub fn write_dashboard(&self, state: &State) -> Result<(), String> {
        let active = state
            .torrents
            .iter()
            .filter(|torrent| torrent.deleted_at.is_none())
            .count();
        let deleted = state
            .torrents
            .iter()
            .filter(|torrent| torrent.deleted_at.is_some())
            .count();
        let items = state
            .runs
            .iter()
            .rev()
            .take(200)
            .map(|run| {
                let task_name = state
                    .tasks
                    .iter()
                    .find(|task| task.id == run.task_id)
                    .map(|task| task.name.as_str())
                    .unwrap_or("已删除任务");
                json!({
                    "task": task_name,
                    "kind": if run.kind == "brush" { "选种" } else { "检查" },
                    "message": run.message,
                    "scanned": run.scanned,
                    "accepted": run.accepted,
                    "deleted": run.deleted,
                    "reasons": run.reasons.iter()
                        .map(|(reason, count)| format!("{reason}: {count}"))
                        .collect::<Vec<_>>()
                        .join("；"),
                    "finished_at": run.finished_at,
                })
            })
            .collect::<Vec<_>>();
        write_json_atomic(
            &self.dashboard_path,
            &json!({
                "updated_at": state.updated_at,
                "summary": {
                    "tasks": state.tasks.len(),
                    "enabled": state.tasks.iter().filter(|task| task.enabled).count(),
                    "active": active,
                    "deleted": deleted,
                },
                "items": items,
            }),
        )
    }
}

impl Drop for StateGuard {
    fn drop(&mut self) {
        let _ = self.lock.unlock();
    }
}

fn write_json_atomic(path: &Path, value: &impl serde::Serialize) -> Result<(), String> {
    let encoded = serde_json::to_string_pretty(value).map_err(|error| error.to_string())?;
    let temporary = path.with_extension("json.tmp");
    fs::write(&temporary, format!("{encoded}\n")).map_err(|error| error.to_string())?;
    fs::rename(temporary, path).map_err(|error| error.to_string())
}
