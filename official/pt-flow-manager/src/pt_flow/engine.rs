// SPDX-License-Identifier: GPL-3.0-only

use chrono::{Duration, Local, Utc};
use cron::Schedule;
use std::{
    collections::{BTreeMap, HashMap, HashSet},
    str::FromStr,
};
use uuid::Uuid;

use super::{
    client::{ControlRequest, MediaryClient},
    models::{
        DownloaderTorrent, ManagedTorrent, PluginSettings, RunReport, State, Task, TorrentCandidate,
    },
    rules::{
        candidate_allowed, deletion_reason, excluded_from_delete, in_active_time, number_range,
    },
};

const BASE_TAG: &str = "MediaryFlow";
const MAX_RUN_HISTORY: usize = 500;
const PENDING_GRACE_MINUTES: i64 = 10;

pub async fn tick(client: &MediaryClient, settings: &PluginSettings, state: &mut State) -> String {
    let now = Utc::now();
    let brush_ids = state
        .tasks
        .iter()
        .filter(|task| task.enabled && brush_due(task, now) && in_active_time(task))
        .map(|task| task.id.clone())
        .collect::<Vec<_>>();
    let check_ids = state
        .tasks
        .iter()
        .filter(|task| task.enabled && due(task.last_check_at, task.check_interval, now))
        .map(|task| task.id.clone())
        .collect::<Vec<_>>();

    let mut errors = 0;
    for id in brush_ids {
        if run_one(client, settings, state, &id).await.is_err() {
            errors += 1;
        }
    }
    for id in check_ids {
        if check_one(client, settings, state, &id).await.is_err() {
            errors += 1;
        }
    }
    trim_state(settings, state);
    if errors == 0 {
        "PT 流量管家定时检查完成".to_string()
    } else {
        format!("PT 流量管家定时检查完成，{errors} 个任务失败")
    }
}

pub async fn run_all(
    client: &MediaryClient,
    settings: &PluginSettings,
    state: &mut State,
) -> String {
    let ids = state
        .tasks
        .iter()
        .filter(|task| task.enabled)
        .map(|task| task.id.clone())
        .collect::<Vec<_>>();
    let mut accepted = 0;
    let mut errors = 0;
    for id in ids {
        match run_one(client, settings, state, &id).await {
            Ok(report) => accepted += report.accepted,
            Err(_) => errors += 1,
        }
    }
    format!("全部选种任务完成：添加 {accepted} 个，失败 {errors} 个")
}

pub async fn check_all(
    client: &MediaryClient,
    settings: &PluginSettings,
    state: &mut State,
) -> String {
    let ids = state
        .tasks
        .iter()
        .filter(|task| task.enabled)
        .map(|task| task.id.clone())
        .collect::<Vec<_>>();
    let mut deleted = 0;
    let mut errors = 0;
    for id in ids {
        match check_one(client, settings, state, &id).await {
            Ok(report) => deleted += report.deleted,
            Err(_) => errors += 1,
        }
    }
    format!("全部托管检查完成：删除 {deleted} 个，失败 {errors} 个")
}

pub async fn run_one(
    client: &MediaryClient,
    settings: &PluginSettings,
    state: &mut State,
    task_id: &str,
) -> Result<RunReport, String> {
    let mut task = state
        .tasks
        .iter()
        .find(|task| task.id == task_id)
        .cloned()
        .ok_or_else(|| "未找到刷流任务".to_string())?;
    let started_at = Utc::now();
    let mut report = RunReport {
        task_id: task.id.clone(),
        kind: "brush".to_string(),
        started_at,
        finished_at: started_at,
        scanned: 0,
        accepted: 0,
        deleted: 0,
        skipped: 0,
        reasons: BTreeMap::new(),
        errors: Vec::new(),
        message: String::new(),
    };
    if !task.enabled {
        report.message = format!("{} 已停用", task.name);
        return finish_report(state, report);
    }
    if !in_active_time(&task) {
        report.message = format!("{} 当前不在开启时间段", task.name);
        return finish_report(state, report);
    }

    resolve_task_downloader(client, &mut task).await?;
    let snapshot = client.downloader_torrents(None).await?;
    let mut candidates = client.candidates(task.site_id, task.rss_support).await?;
    report.scanned = candidates.len();
    candidates.sort_by(|left, right| right.publish_time.cmp(&left.publish_time));
    let subscription_titles = if task.except_subscriptions {
        client
            .subscription_titles()
            .await?
            .into_iter()
            .map(|item| item.name)
            .filter(|name| !name.trim().is_empty())
            .collect::<Vec<_>>()
    } else {
        Vec::new()
    };
    let mut existing_sources = state
        .torrents
        .iter()
        .filter(|torrent| torrent.deleted_at.is_none())
        .map(|torrent| torrent.source_url.clone())
        .collect::<HashSet<_>>();
    let mut accepted_size = 0_i64;

    for candidate in candidates {
        if report.accepted >= task.max_add_per_run {
            break;
        }
        if existing_sources.contains(&candidate.download_url) {
            report.skipped += 1;
            record_reason(&mut report, "重复种子");
            continue;
        }
        if state.torrents.iter().any(|torrent| {
            torrent.deleted_at.is_none()
                && torrent.title.eq_ignore_ascii_case(&candidate.title)
                && torrent.site_id != candidate.site_id
        }) {
            report.skipped += 1;
            record_reason(&mut report, "其他站点存在同名托管种子");
            continue;
        }
        if matches_subscription(&candidate, &subscription_titles) {
            report.skipped += 1;
            record_reason(&mut report, "命中订阅内容");
            continue;
        }
        if let Err(reason) = candidate_allowed(&task, &candidate) {
            report.skipped += 1;
            record_reason(&mut report, &reason);
            continue;
        }
        if let Some(reason) = capacity_reason(
            settings,
            &task,
            state,
            &snapshot,
            accepted_size,
            candidate.size,
        ) {
            report.skipped += 1;
            record_reason(&mut report, reason);
            continue;
        }
        match add_candidate(client, &task, &candidate).await {
            Ok(hash) => {
                accepted_size = accepted_size.saturating_add(candidate.size);
                existing_sources.insert(candidate.download_url.clone());
                state.torrents.push(managed_record(&task, &candidate, hash));
                report.accepted += 1;
            }
            Err(error) => report.errors.push(format!("{}: {error}", candidate.title)),
        }
    }

    if let Some(task) = state.tasks.iter_mut().find(|item| item.id == task_id) {
        task.last_brush_at = Some(Utc::now());
    }
    report.message = format!(
        "{}：扫描 {}，添加 {}，跳过 {}",
        task.name, report.scanned, report.accepted, report.skipped
    );
    if task.notify && (report.accepted > 0 || !report.errors.is_empty()) {
        let _ = client
            .notify(
                "PT 流量管家选种",
                &format!("{}\n错误：{}", report.message, report.errors.len()),
            )
            .await;
    }
    finish_report(state, report)
}

pub async fn check_one(
    client: &MediaryClient,
    settings: &PluginSettings,
    state: &mut State,
    task_id: &str,
) -> Result<RunReport, String> {
    let mut task = state
        .tasks
        .iter()
        .find(|task| task.id == task_id)
        .cloned()
        .ok_or_else(|| "未找到刷流任务".to_string())?;
    let started_at = Utc::now();
    let mut report = RunReport {
        task_id: task.id.clone(),
        kind: "check".to_string(),
        started_at,
        finished_at: started_at,
        scanned: 0,
        accepted: 0,
        deleted: 0,
        skipped: 0,
        reasons: BTreeMap::new(),
        errors: Vec::new(),
        message: String::new(),
    };
    resolve_task_downloader(client, &mut task).await?;
    let snapshot = client.downloader_torrents(Some(&task.downloader)).await?;
    let by_hash = snapshot
        .iter()
        .map(|torrent| (torrent.hash.to_ascii_lowercase(), torrent))
        .collect::<HashMap<_, _>>();
    reconcile_pending(&task, &snapshot, state);
    let now = Utc::now();
    let mut planned = Vec::<(String, String)>::new();

    for managed in state
        .torrents
        .iter_mut()
        .filter(|torrent| torrent.task_id == task.id && torrent.deleted_at.is_none())
    {
        report.scanned += 1;
        let Some(torrent) = by_hash.get(&managed.hash.to_ascii_lowercase()) else {
            if now - managed.added_at >= Duration::minutes(PENDING_GRACE_MINUTES) {
                managed.deleted_at = Some(now);
                managed.delete_reason = Some("下载器中已不存在".to_string());
            }
            continue;
        };
        update_average(managed, torrent, now);
        if excluded_from_delete(&task, torrent) {
            continue;
        }
        if let Some(reason) = deletion_reason(&task, managed, torrent)
            .or_else(|| promotion_expired_reason(&task, managed, torrent))
        {
            planned.push((torrent.hash.clone(), reason));
        }
    }

    planned.extend(dynamic_deletions(&task, state, &snapshot, &planned));
    planned.sort_by(|left, right| left.0.cmp(&right.0));
    planned.dedup_by(|left, right| left.0.eq_ignore_ascii_case(&right.0));
    if !planned.is_empty() {
        let hashes = planned
            .iter()
            .map(|(hash, _)| hash.clone())
            .collect::<Vec<_>>();
        let _ = client
            .control(ControlRequest {
                action: "reannounce",
                hashes: hashes.clone(),
                upload_limit_kbps: None,
                download_limit_kbps: None,
                tags: None,
                delete_files: false,
                downloader: Some(&task.downloader),
            })
            .await;
        match client
            .control(ControlRequest {
                action: "delete",
                hashes,
                upload_limit_kbps: None,
                download_limit_kbps: None,
                tags: None,
                delete_files: task.delete_files,
                downloader: Some(&task.downloader),
            })
            .await
        {
            Ok(()) => {
                for (hash, reason) in &planned {
                    if let Some(managed) = state.torrents.iter_mut().find(|torrent| {
                        torrent.deleted_at.is_none() && torrent.hash.eq_ignore_ascii_case(hash)
                    }) {
                        managed.deleted_at = Some(now);
                        managed.delete_reason = Some(reason.clone());
                        report.deleted += 1;
                    }
                }
            }
            Err(error) => report.errors.push(error),
        }
    }

    archive_old(&task, state);
    if let Some(task) = state.tasks.iter_mut().find(|item| item.id == task_id) {
        task.last_check_at = Some(now);
    }
    report.message = format!(
        "{}：检查 {}，删除 {}",
        task.name, report.scanned, report.deleted
    );
    if task.notify && (report.deleted > 0 || !report.errors.is_empty()) {
        let _ = client
            .notify(
                "PT 流量管家托管检查",
                &format!("{}\n错误：{}", report.message, report.errors.len()),
            )
            .await;
    }
    let report = finish_report(state, report)?;
    trim_state(settings, state);
    Ok(report)
}

async fn add_candidate(
    client: &MediaryClient,
    task: &Task,
    candidate: &TorrentCandidate,
) -> Result<String, String> {
    let mut tags = vec![
        "Mediary".to_string(),
        BASE_TAG.to_string(),
        task.unique_tag(),
    ];
    tags.extend(
        task.extra_tags
            .split(',')
            .map(str::trim)
            .filter(|tag| !tag.is_empty())
            .map(str::to_string),
    );
    let response = client
        .add_download(
            candidate,
            non_empty(&task.save_path),
            non_empty(&task.category),
            tags.clone(),
            Some(&task.downloader),
            task.skip_download_tips,
        )
        .await?;
    let hash = response
        .hash
        .unwrap_or_else(|| format!("pending:{}", Uuid::new_v4().simple()));
    if !hash.starts_with("pending:") {
        if task.torrent_upload_limit_kbps > 0 || task.torrent_download_limit_kbps > 0 {
            client
                .control(ControlRequest {
                    action: "limits",
                    hashes: vec![hash.clone()],
                    upload_limit_kbps: (task.torrent_upload_limit_kbps > 0)
                        .then_some(task.torrent_upload_limit_kbps),
                    download_limit_kbps: (task.torrent_download_limit_kbps > 0)
                        .then_some(task.torrent_download_limit_kbps),
                    tags: None,
                    delete_files: false,
                    downloader: Some(&task.downloader),
                })
                .await?;
        }
        client
            .control(ControlRequest {
                action: "tags",
                hashes: vec![hash.clone()],
                upload_limit_kbps: None,
                download_limit_kbps: None,
                tags: Some(tags),
                delete_files: false,
                downloader: Some(&task.downloader),
            })
            .await?;
    }
    Ok(hash)
}

fn managed_record(task: &Task, candidate: &TorrentCandidate, hash: String) -> ManagedTorrent {
    ManagedTorrent {
        task_id: task.id.clone(),
        downloader: task.downloader.clone(),
        hash,
        title: candidate.title.clone(),
        site_id: candidate.site_id,
        site_name: candidate.site_name.clone(),
        source_url: candidate.download_url.clone(),
        size: candidate.size,
        hit_and_run: candidate.hit_and_run || task.site_hr_active,
        promotion: candidate
            .volume_factor
            .clone()
            .unwrap_or_else(|| "普通".to_string()),
        free_until: candidate.freedate.clone(),
        added_at: Utc::now(),
        deleted_at: None,
        delete_reason: None,
        last_uploaded: 0,
        last_downloaded: 0,
        last_sample_at: None,
        average_upload_speed: 0.0,
    }
}

fn capacity_reason(
    settings: &PluginSettings,
    task: &Task,
    state: &State,
    snapshot: &[DownloaderTorrent],
    accepted_size: i64,
    candidate_size: i64,
) -> Option<&'static str> {
    let managed_keys = state
        .torrents
        .iter()
        .filter(|torrent| torrent.deleted_at.is_none())
        .map(|torrent| {
            (
                torrent.downloader.to_ascii_lowercase(),
                torrent.hash.to_ascii_lowercase(),
            )
        })
        .collect::<HashSet<_>>();
    let task_keys = state
        .torrents
        .iter()
        .filter(|torrent| torrent.task_id == task.id && torrent.deleted_at.is_none())
        .map(|torrent| {
            (
                torrent.downloader.to_ascii_lowercase(),
                torrent.hash.to_ascii_lowercase(),
            )
        })
        .collect::<HashSet<_>>();
    let managed = snapshot
        .iter()
        .filter(|torrent| {
            managed_keys.contains(&(
                torrent.downloader.to_ascii_lowercase(),
                torrent.hash.to_ascii_lowercase(),
            ))
        })
        .collect::<Vec<_>>();
    let task_torrents = snapshot
        .iter()
        .filter(|torrent| {
            task_keys.contains(&(
                torrent.downloader.to_ascii_lowercase(),
                torrent.hash.to_ascii_lowercase(),
            ))
        })
        .collect::<Vec<_>>();
    let global_size = managed.iter().map(|torrent| torrent.size).sum::<i64>() + accepted_size;
    let task_size = task_torrents
        .iter()
        .map(|torrent| torrent.size)
        .sum::<i64>()
        + accepted_size;
    let downloading = managed
        .iter()
        .filter(|torrent| !torrent.is_completed && !torrent.is_paused)
        .count();
    let task_downloading = task_torrents
        .iter()
        .filter(|torrent| !torrent.is_completed && !torrent.is_paused)
        .count();
    let upload = snapshot
        .iter()
        .map(|torrent| torrent.upload_speed)
        .sum::<i64>()
        / 1024;
    let download = snapshot
        .iter()
        .map(|torrent| torrent.download_speed)
        .sum::<i64>()
        / 1024;
    let task_upload = task_torrents
        .iter()
        .map(|torrent| torrent.upload_speed)
        .sum::<i64>()
        / 1024;
    let task_download = task_torrents
        .iter()
        .map(|torrent| torrent.download_speed)
        .sum::<i64>()
        / 1024;
    if exceeds_size(settings.global_max_size_gb, global_size, candidate_size)
        || exceeds_size(task.max_total_size_gb, task_size, candidate_size)
    {
        return Some("保种体积达到限制");
    }
    if (settings.global_max_downloading > 0 && downloading >= settings.global_max_downloading)
        || (task.max_downloading > 0 && task_downloading >= task.max_downloading)
    {
        return Some("同时下载数量达到限制");
    }
    if (settings.global_max_upload_kbps > 0 && upload >= settings.global_max_upload_kbps)
        || (task.max_upload_kbps > 0 && task_upload >= task.max_upload_kbps)
    {
        return Some("上传带宽达到限制");
    }
    if (settings.global_max_download_kbps > 0 && download >= settings.global_max_download_kbps)
        || (task.max_download_kbps > 0 && task_download >= task.max_download_kbps)
    {
        return Some("下载带宽达到限制");
    }
    None
}

fn exceeds_size(limit_gb: f64, current: i64, candidate: i64) -> bool {
    limit_gb > 0.0 && (current.saturating_add(candidate)) as f64 > limit_gb * 1024_f64.powi(3)
}

fn dynamic_deletions(
    task: &Task,
    state: &State,
    snapshot: &[DownloaderTorrent],
    already_planned: &[(String, String)],
) -> Vec<(String, String)> {
    if !task.dynamic_delete || task.delete_size_gb.trim().is_empty() {
        return Vec::new();
    }
    let (target_gb, trigger_gb) = dynamic_threshold(&task.delete_size_gb);
    let planned = already_planned
        .iter()
        .map(|(hash, _)| hash.to_ascii_lowercase())
        .collect::<HashSet<_>>();
    let managed_by_hash = state
        .torrents
        .iter()
        .filter(|torrent| torrent.task_id == task.id && torrent.deleted_at.is_none())
        .map(|torrent| (torrent.hash.to_ascii_lowercase(), torrent))
        .collect::<HashMap<_, _>>();
    let mut candidates = snapshot
        .iter()
        .filter_map(|torrent| {
            let managed = managed_by_hash.get(&torrent.hash.to_ascii_lowercase())?;
            (torrent.is_completed
                && !managed.hit_and_run
                && !planned.contains(&torrent.hash.to_ascii_lowercase())
                && !excluded_from_delete(task, torrent))
            .then_some((torrent, *managed))
        })
        .collect::<Vec<_>>();
    let mut total = snapshot
        .iter()
        .filter(|torrent| managed_by_hash.contains_key(&torrent.hash.to_ascii_lowercase()))
        .map(|torrent| torrent.size)
        .sum::<i64>();
    if total as f64 <= trigger_gb * 1024_f64.powi(3) {
        return Vec::new();
    }
    candidates.sort_by_key(|(torrent, _)| std::cmp::Reverse(torrent.seeding_time));
    let target = (target_gb * 1024_f64.powi(3)) as i64;
    let mut result = Vec::new();
    for (torrent, _) in candidates {
        if total <= target {
            break;
        }
        total = total.saturating_sub(torrent.size);
        result.push((torrent.hash.clone(), "动态保种体积回收".to_string()));
    }
    result
}

fn dynamic_threshold(value: &str) -> (f64, f64) {
    if value.contains('-') {
        number_range(value).unwrap_or((0.0, 0.0))
    } else {
        let value = value.trim().parse::<f64>().unwrap_or(0.0);
        (value, value)
    }
}

fn reconcile_pending(task: &Task, snapshot: &[DownloaderTorrent], state: &mut State) {
    for managed in state
        .torrents
        .iter_mut()
        .filter(|torrent| torrent.task_id == task.id && torrent.hash.starts_with("pending:"))
    {
        if let Some(torrent) = snapshot.iter().find(|torrent| {
            torrent
                .tags
                .iter()
                .any(|tag| tag.eq_ignore_ascii_case(&task.unique_tag()))
                && torrent.name == managed.title
        }) {
            managed.hash = torrent.hash.clone();
        }
    }
}

fn update_average(
    managed: &mut ManagedTorrent,
    torrent: &DownloaderTorrent,
    sampled_at: chrono::DateTime<Utc>,
) {
    if let Some(previous_at) = managed.last_sample_at {
        let seconds = (sampled_at - previous_at).num_seconds();
        let bytes = torrent.uploaded.saturating_sub(managed.last_uploaded);
        if seconds > 0 && bytes >= 0 {
            let speed = bytes as f64 / seconds as f64;
            managed.average_upload_speed = if managed.average_upload_speed <= 0.0 {
                speed
            } else {
                managed.average_upload_speed * 0.7 + speed * 0.3
            };
        }
    }
    managed.last_uploaded = torrent.uploaded;
    managed.last_downloaded = torrent.downloaded;
    managed.last_sample_at = Some(sampled_at);
}

fn promotion_expired_reason(
    task: &Task,
    managed: &ManagedTorrent,
    torrent: &DownloaderTorrent,
) -> Option<String> {
    if !task.delete_promotion_ended || torrent.is_completed || managed.free_until.is_none() {
        return None;
    }
    let value = managed.free_until.as_deref()?.trim();
    let expired = chrono::DateTime::parse_from_rfc3339(value)
        .map(|date| date.with_timezone(&Utc) <= Utc::now())
        .unwrap_or(false);
    expired.then_some("促销已经结束".to_string())
}

fn archive_old(task: &Task, state: &mut State) {
    if task.auto_archive_days <= 0 {
        return;
    }
    let threshold = Utc::now() - Duration::days(task.auto_archive_days);
    state.torrents.retain(|torrent| {
        torrent.task_id != task.id
            || torrent
                .deleted_at
                .is_none_or(|deleted| deleted >= threshold)
    });
}

fn finish_report(state: &mut State, mut report: RunReport) -> Result<RunReport, String> {
    report.finished_at = Utc::now();
    if report.message.is_empty() {
        report.message = "任务已完成".to_string();
    }
    state.runs.push(report.clone());
    if state.runs.len() > MAX_RUN_HISTORY {
        state.runs.drain(..state.runs.len() - MAX_RUN_HISTORY);
    }
    Ok(report)
}

fn trim_state(settings: &PluginSettings, state: &mut State) {
    let limit = settings.history_limit.clamp(100, 20_000);
    if state.torrents.len() > limit {
        let remove = state.torrents.len() - limit;
        let mut deleted_indexes = state
            .torrents
            .iter()
            .enumerate()
            .filter_map(|(index, torrent)| torrent.deleted_at.map(|_| index))
            .take(remove)
            .collect::<Vec<_>>();
        deleted_indexes.reverse();
        for index in deleted_indexes {
            state.torrents.remove(index);
        }
    }
}

fn due(
    last: Option<chrono::DateTime<Utc>>,
    interval_minutes: i64,
    now: chrono::DateTime<Utc>,
) -> bool {
    last.is_none_or(|last| now - last >= Duration::minutes(interval_minutes.max(1)))
}

fn brush_due(task: &Task, now: chrono::DateTime<Utc>) -> bool {
    if task.cron.trim().is_empty() {
        return due(task.last_brush_at, task.brush_interval, now);
    }
    let Ok(schedule) = Schedule::from_str(&format!("0 {}", task.cron.trim())) else {
        return false;
    };
    let now_local = now.with_timezone(&Local);
    let after = task
        .last_brush_at
        .map(|value| value.with_timezone(&Local))
        .unwrap_or_else(|| now_local - Duration::minutes(1));
    schedule
        .after(&after)
        .next()
        .is_some_and(|next| next <= now_local)
}

async fn resolve_task_downloader(client: &MediaryClient, task: &mut Task) -> Result<(), String> {
    let info = client.downloader_info().await?;
    if task.downloader.trim().is_empty() {
        task.downloader = info.default.trim().to_ascii_lowercase();
    }
    if !info
        .items
        .iter()
        .any(|item| item.id.eq_ignore_ascii_case(&task.downloader))
    {
        return Err(format!("任务下载器 {} 未配置或不可用", task.downloader));
    }
    Ok(())
}

fn matches_subscription(candidate: &TorrentCandidate, names: &[String]) -> bool {
    if names.is_empty() {
        return false;
    }
    let content = normalize_search_text(&format!(
        "{} {}",
        candidate.title,
        candidate.description.as_deref().unwrap_or_default()
    ));
    names.iter().any(|name| {
        let name = normalize_search_text(name);
        !name.is_empty() && content.contains(&name)
    })
}

fn normalize_search_text(value: &str) -> String {
    value
        .to_lowercase()
        .chars()
        .map(|character| {
            if character.is_alphanumeric() {
                character
            } else {
                ' '
            }
        })
        .collect::<String>()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

fn record_reason(report: &mut RunReport, reason: &str) {
    *report.reasons.entry(reason.to_string()).or_default() += 1;
}

fn non_empty(value: &str) -> Option<&str> {
    let value = value.trim();
    (!value.is_empty()).then_some(value)
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    #[test]
    fn cron_runs_once_when_a_scheduled_minute_is_due() {
        let now = Utc.with_ymd_and_hms(2026, 7, 27, 12, 10, 0).unwrap();
        let mut task = Task {
            cron: "*/5 * * * *".to_string(),
            last_brush_at: Some(now - Duration::minutes(6)),
            ..Task::default()
        };

        assert!(brush_due(&task, now));
        task.last_brush_at = Some(now);
        assert!(!brush_due(&task, now));
    }

    #[test]
    fn subscription_titles_match_title_or_description_case_insensitively() {
        let title_candidate = candidate("Some.Show.S01.1080p");
        assert!(matches_subscription(
            &title_candidate,
            &["some show".to_string()]
        ));

        let mut described = candidate("Unrelated.Release");
        described.description = Some("Includes The Example Movie extras".to_string());
        assert!(matches_subscription(
            &described,
            &["the example movie".to_string()]
        ));
        assert!(!matches_subscription(
            &described,
            &["other title".to_string()]
        ));
    }

    #[test]
    fn task_bandwidth_limit_is_isolated_from_other_downloaders() {
        let task = Task {
            id: "qb-task".to_string(),
            downloader: "qbittorrent".to_string(),
            max_upload_kbps: 100,
            ..Task::default()
        };
        let state = State {
            torrents: vec![
                managed("qb-task", "qbittorrent", "qb"),
                managed("tr-task", "transmission", "tr"),
            ],
            ..State::default()
        };
        let snapshot = vec![
            downloader_torrent("qbittorrent", "qb", 50, 0, 10),
            downloader_torrent("transmission", "tr", 500, 0, 20),
        ];

        assert_eq!(
            capacity_reason(&PluginSettings::default(), &task, &state, &snapshot, 0, 1),
            None
        );
        let settings = PluginSettings {
            global_max_upload_kbps: 500,
            ..PluginSettings::default()
        };
        assert_eq!(
            capacity_reason(&settings, &task, &state, &snapshot, 0, 1),
            Some("上传带宽达到限制")
        );
    }

    #[test]
    fn promotion_expiry_only_deletes_incomplete_torrents() {
        let task = Task {
            delete_promotion_ended: true,
            ..Task::default()
        };
        let mut managed = managed("task", "qbittorrent", "hash");
        managed.free_until = Some((Utc::now() - Duration::minutes(1)).to_rfc3339());
        let incomplete = downloader_torrent("qbittorrent", "hash", 0, 0, 0);
        assert_eq!(
            promotion_expired_reason(&task, &managed, &incomplete).as_deref(),
            Some("促销已经结束")
        );

        let completed = DownloaderTorrent {
            is_completed: true,
            ..incomplete
        };
        assert_eq!(promotion_expired_reason(&task, &managed, &completed), None);
    }

    #[test]
    fn dynamic_cleanup_prefers_longest_seeding_completed_torrents() {
        let task = Task {
            id: "task".to_string(),
            downloader: "qbittorrent".to_string(),
            dynamic_delete: true,
            delete_size_gb: "1-3".to_string(),
            ..Task::default()
        };
        let state = State {
            torrents: vec![
                managed("task", "qbittorrent", "old"),
                managed("task", "qbittorrent", "new"),
            ],
            ..State::default()
        };
        let gib = 1024_i64.pow(3);
        let snapshot = vec![
            DownloaderTorrent {
                is_completed: true,
                size: 2 * gib,
                ..downloader_torrent("qbittorrent", "old", 0, 0, 10_000)
            },
            DownloaderTorrent {
                is_completed: true,
                size: 2 * gib,
                ..downloader_torrent("qbittorrent", "new", 0, 0, 100)
            },
        ];

        assert_eq!(
            dynamic_deletions(&task, &state, &snapshot, &[]),
            vec![
                ("old".to_string(), "动态保种体积回收".to_string()),
                ("new".to_string(), "动态保种体积回收".to_string())
            ]
        );
    }

    #[test]
    fn site_hr_mode_marks_new_records_as_protected() {
        let task = Task {
            site_hr_active: true,
            ..Task::default()
        };
        assert!(managed_record(&task, &candidate("Example"), "hash".to_string()).hit_and_run);
    }

    fn candidate(title: &str) -> TorrentCandidate {
        TorrentCandidate {
            title: title.to_string(),
            site_name: "Test".to_string(),
            site_id: 1,
            size: 1024,
            download_url: "https://example.test/download".to_string(),
            description: None,
            seeders: 1,
            publish_time: None,
            uploadvolumefactor: Some(1.0),
            downloadvolumefactor: Some(0.0),
            volume_factor: Some("免费".to_string()),
            freedate: None,
            hit_and_run: false,
        }
    }

    fn managed(task_id: &str, downloader: &str, hash: &str) -> ManagedTorrent {
        ManagedTorrent {
            task_id: task_id.to_string(),
            downloader: downloader.to_string(),
            hash: hash.to_string(),
            title: hash.to_string(),
            site_id: 1,
            site_name: "Test".to_string(),
            source_url: format!("https://example.test/{hash}"),
            size: 1024,
            hit_and_run: false,
            promotion: "免费".to_string(),
            free_until: None,
            added_at: Utc::now(),
            deleted_at: None,
            delete_reason: None,
            last_uploaded: 0,
            last_downloaded: 0,
            last_sample_at: None,
            average_upload_speed: 0.0,
        }
    }

    fn downloader_torrent(
        downloader: &str,
        hash: &str,
        upload_kbps: i64,
        download_kbps: i64,
        seeding_time: i64,
    ) -> DownloaderTorrent {
        DownloaderTorrent {
            downloader: downloader.to_string(),
            hash: hash.to_string(),
            name: hash.to_string(),
            size: 1024,
            is_completed: false,
            is_paused: false,
            download_speed: download_kbps * 1024,
            upload_speed: upload_kbps * 1024,
            downloaded: 0,
            uploaded: 0,
            ratio: 0.0,
            last_active_at: None,
            seeding_time,
            tags: Vec::new(),
        }
    }
}
