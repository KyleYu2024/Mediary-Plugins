// SPDX-License-Identifier: GPL-3.0-only

use chrono::{DateTime, Local, NaiveDateTime, NaiveTime, TimeZone, Utc};
use regex::Regex;

use super::models::{DownloaderTorrent, ManagedTorrent, Task, TorrentCandidate};

pub fn candidate_allowed(task: &Task, candidate: &TorrentCandidate) -> Result<(), String> {
    if candidate.site_id != task.site_id {
        return Err("站点不匹配".to_string());
    }
    if task.exclude_hr && candidate.hit_and_run {
        return Err("H&R 种子".to_string());
    }
    let is_free = candidate.downloadvolumefactor == Some(0.0);
    let is_double_free = is_free && candidate.uploadvolumefactor.unwrap_or(1.0) >= 2.0;
    if task.promotion == "free" && !is_free {
        return Err("不是免费种子".to_string());
    }
    if task.promotion == "2xfree" && !is_double_free {
        return Err("不是 2X 免费种子".to_string());
    }
    let content = format!(
        "{} {}",
        candidate.title,
        candidate.description.as_deref().unwrap_or_default()
    );
    if !task.include.trim().is_empty()
        && !Regex::new(&task.include)
            .map_err(|error| error.to_string())?
            .is_match(&content)
    {
        return Err("未命中包含规则".to_string());
    }
    if !task.exclude.trim().is_empty()
        && Regex::new(&task.exclude)
            .map_err(|error| error.to_string())?
            .is_match(&content)
    {
        return Err("命中排除规则".to_string());
    }
    let size_gb = candidate.size as f64 / 1024_f64.powi(3);
    if !size_range_matches(&task.size_gb, size_gb) {
        return Err("种子大小不符合规则".to_string());
    }
    if !number_range_matches(&task.seeders, candidate.seeders as f64) {
        return Err("做种人数不符合规则".to_string());
    }
    if !task.pubtime_minutes.trim().is_empty() {
        let age = publish_age_minutes(candidate.publish_time.as_deref())
            .ok_or_else(|| "无法解析发布时间".to_string())?;
        let adjusted_age = age as f64 - task.timezone_offset_hours * 60.0;
        if !number_range_matches(&task.pubtime_minutes, adjusted_age.max(0.0)) {
            return Err("发布时间不符合规则".to_string());
        }
    }
    Ok(())
}

pub fn in_active_time(task: &Task) -> bool {
    let value = task.active_time_range.trim();
    if value.is_empty() {
        return true;
    }
    let Some((start, end)) = value.split_once('-') else {
        return true;
    };
    let Ok(start) = NaiveTime::parse_from_str(start, "%H:%M") else {
        return true;
    };
    let Ok(end) = NaiveTime::parse_from_str(end, "%H:%M") else {
        return true;
    };
    let now = Local::now().time();
    if start <= end {
        now >= start && now <= end
    } else {
        now >= start || now <= end
    }
}

pub fn deletion_reason(
    task: &Task,
    managed: &ManagedTorrent,
    torrent: &DownloaderTorrent,
) -> Option<String> {
    let now = Utc::now();
    let age_hours = (now - managed.added_at).num_seconds().max(0) as f64 / 3600.0;
    let seed_hours = torrent.seeding_time.max(0) as f64 / 3600.0;
    let target_seed_hours = if managed.hit_and_run && task.hr_seed_time_hours > 0.0 {
        task.hr_seed_time_hours
    } else {
        task.seed_time_hours
    };
    if !torrent.is_completed
        && task.download_timeout_hours > 0.0
        && age_hours >= task.download_timeout_hours
    {
        return Some(format!("下载超过 {:.1} 小时", task.download_timeout_hours));
    }
    if torrent.is_completed && target_seed_hours > 0.0 && seed_hours >= target_seed_hours {
        return Some(format!("做种达到 {:.1} 小时", target_seed_hours));
    }
    if torrent.is_completed && task.seed_ratio > 0.0 && torrent.ratio >= task.seed_ratio {
        return Some(format!("分享率达到 {:.2}", task.seed_ratio));
    }
    let uploaded_gb = torrent.uploaded.max(0) as f64 / 1024_f64.powi(3);
    if torrent.is_completed && task.seed_upload_gb > 0.0 && uploaded_gb >= task.seed_upload_gb {
        return Some(format!("上传量达到 {:.1} GB", task.seed_upload_gb));
    }
    if torrent.is_completed
        && task.inactive_minutes > 0
        && torrent
            .last_active_at
            .and_then(|timestamp| DateTime::from_timestamp(timestamp, 0))
            .is_some_and(|last| (now - last).num_minutes() >= task.inactive_minutes)
    {
        return Some(format!("超过 {} 分钟无活动", task.inactive_minutes));
    }
    if torrent.is_completed
        && task.avg_upload_kbps > 0.0
        && age_hours >= 0.5
        && managed.average_upload_speed / 1024.0 < task.avg_upload_kbps
    {
        return Some(format!("平均上传速度低于 {:.0} KB/s", task.avg_upload_kbps));
    }
    None
}

pub fn number_range(value: &str) -> Option<(f64, f64)> {
    let value = value.trim();
    if value.is_empty() {
        return None;
    }
    if let Some((left, right)) = value.split_once('-') {
        let left = left.parse::<f64>().ok()?;
        let right = right.parse::<f64>().ok()?;
        Some((left.min(right), left.max(right)))
    } else {
        let number = value.parse::<f64>().ok()?;
        Some((0.0, number))
    }
}

pub fn excluded_from_delete(task: &Task, torrent: &DownloaderTorrent) -> bool {
    task.delete_except_tags
        .split(',')
        .map(str::trim)
        .filter(|tag| !tag.is_empty())
        .any(|excluded| {
            torrent
                .tags
                .iter()
                .any(|tag| tag.eq_ignore_ascii_case(excluded))
        })
}

fn number_range_matches(value: &str, number: f64) -> bool {
    number_range(value).is_none_or(|(minimum, maximum)| number >= minimum && number <= maximum)
}

fn size_range_matches(value: &str, number: f64) -> bool {
    let value = value.trim();
    if value.is_empty() {
        return true;
    }
    if value.contains('-') {
        return number_range_matches(value, number);
    }
    value
        .parse::<f64>()
        .ok()
        .is_none_or(|minimum| number >= minimum)
}

fn publish_age_minutes(value: Option<&str>) -> Option<i64> {
    let value = value?.trim();
    let date = DateTime::parse_from_rfc3339(value)
        .map(|date| date.with_timezone(&Utc))
        .ok()
        .or_else(|| {
            NaiveDateTime::parse_from_str(value, "%Y-%m-%d %H:%M:%S")
                .ok()
                .and_then(|date| Local.from_local_datetime(&date).single())
                .map(|date| date.with_timezone(&Utc))
        })?;
    Some((Utc::now() - date).num_minutes().max(0))
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Utc;

    use crate::pt_flow::models::{ManagedTorrent, TorrentCandidate};

    #[test]
    fn matches_single_value_as_upper_bound() {
        assert!(number_range_matches("5", 4.0));
        assert!(!number_range_matches("5", 6.0));
    }

    #[test]
    fn matches_closed_range() {
        assert!(number_range_matches("5-10", 7.0));
        assert!(!number_range_matches("5-10", 11.0));
    }

    #[test]
    fn size_single_value_is_a_minimum() {
        assert!(size_range_matches("5", 6.0));
        assert!(!size_range_matches("5", 4.0));
    }

    #[test]
    fn filters_promotions_hr_and_size() {
        let task = Task {
            site_id: 1,
            promotion: "free".to_string(),
            size_gb: "5".to_string(),
            pubtime_minutes: String::new(),
            ..Task::default()
        };
        let mut candidate = test_candidate();
        candidate.size = 6 * 1024_i64.pow(3);
        assert!(candidate_allowed(&task, &candidate).is_ok());

        candidate.hit_and_run = true;
        assert_eq!(
            candidate_allowed(&task, &candidate).unwrap_err(),
            "H&R 种子"
        );
        candidate.hit_and_run = false;
        candidate.downloadvolumefactor = Some(1.0);
        assert_eq!(
            candidate_allowed(&task, &candidate).unwrap_err(),
            "不是免费种子"
        );
    }

    #[test]
    fn deletes_completed_torrent_after_ratio_target() {
        let task = Task {
            seed_ratio: 2.0,
            ..Task::default()
        };
        let managed = ManagedTorrent {
            task_id: "task".to_string(),
            downloader: "qbittorrent".to_string(),
            hash: "hash".to_string(),
            title: "Example".to_string(),
            site_id: 1,
            site_name: "Test".to_string(),
            source_url: "https://example.test/download".to_string(),
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
        };
        let torrent = DownloaderTorrent {
            downloader: "qbittorrent".to_string(),
            hash: "hash".to_string(),
            name: "Example".to_string(),
            size: 1024,
            is_completed: true,
            is_paused: false,
            download_speed: 0,
            upload_speed: 0,
            downloaded: 1024,
            uploaded: 2048,
            ratio: 2.1,
            last_active_at: None,
            seeding_time: 60,
            tags: vec!["MediaryFlow".to_string()],
        };
        assert_eq!(
            deletion_reason(&task, &managed, &torrent).as_deref(),
            Some("分享率达到 2.00")
        );
    }

    fn test_candidate() -> TorrentCandidate {
        TorrentCandidate {
            title: "Example.2026.1080p".to_string(),
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
}
