// SPDX-License-Identifier: GPL-3.0-only

use chrono::{DateTime, Utc};
use regex::Regex;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use uuid::Uuid;

#[derive(Clone, Debug, Deserialize)]
#[serde(default)]
pub struct PluginSettings {
    pub global_max_size_gb: f64,
    pub global_max_downloading: usize,
    pub global_max_upload_kbps: i64,
    pub global_max_download_kbps: i64,
    pub history_limit: usize,
}

impl Default for PluginSettings {
    fn default() -> Self {
        Self {
            global_max_size_gb: 0.0,
            global_max_downloading: 0,
            global_max_upload_kbps: 0,
            global_max_download_kbps: 0,
            history_limit: 2000,
        }
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(default)]
pub struct Task {
    pub id: String,
    pub name: String,
    pub site_id: i64,
    pub downloader: String,
    pub enabled: bool,
    pub notify: bool,
    pub brush_interval: i64,
    pub check_interval: i64,
    pub cron: String,
    pub active_time_range: String,
    pub rss_support: bool,
    pub except_subscriptions: bool,
    pub site_hr_active: bool,
    pub skip_download_tips: bool,
    pub promotion: String,
    pub exclude_hr: bool,
    pub include: String,
    pub exclude: String,
    pub size_gb: String,
    pub seeders: String,
    pub pubtime_minutes: String,
    pub timezone_offset_hours: f64,
    pub max_total_size_gb: f64,
    pub max_downloading: usize,
    pub max_upload_kbps: i64,
    pub max_download_kbps: i64,
    pub max_add_per_run: usize,
    pub seed_time_hours: f64,
    pub hr_seed_time_hours: f64,
    pub seed_ratio: f64,
    pub seed_upload_gb: f64,
    pub download_timeout_hours: f64,
    pub inactive_minutes: i64,
    pub avg_upload_kbps: f64,
    pub dynamic_delete: bool,
    pub delete_promotion_ended: bool,
    pub delete_size_gb: String,
    pub delete_files: bool,
    pub delete_except_tags: String,
    pub torrent_upload_limit_kbps: i64,
    pub torrent_download_limit_kbps: i64,
    pub save_path: String,
    pub category: String,
    pub extra_tags: String,
    pub auto_archive_days: i64,
    pub last_brush_at: Option<DateTime<Utc>>,
    pub last_check_at: Option<DateTime<Utc>>,
}

impl Default for Task {
    fn default() -> Self {
        Self {
            id: String::new(),
            name: String::new(),
            site_id: 0,
            downloader: String::new(),
            enabled: true,
            notify: true,
            brush_interval: 10,
            check_interval: 5,
            cron: String::new(),
            active_time_range: String::new(),
            rss_support: false,
            except_subscriptions: true,
            site_hr_active: false,
            skip_download_tips: false,
            promotion: "free".to_string(),
            exclude_hr: true,
            include: String::new(),
            exclude: String::new(),
            size_gb: String::new(),
            seeders: String::new(),
            pubtime_minutes: "0-30".to_string(),
            timezone_offset_hours: 0.0,
            max_total_size_gb: 0.0,
            max_downloading: 0,
            max_upload_kbps: 0,
            max_download_kbps: 0,
            max_add_per_run: 2,
            seed_time_hours: 0.0,
            hr_seed_time_hours: 0.0,
            seed_ratio: 0.0,
            seed_upload_gb: 0.0,
            download_timeout_hours: 0.0,
            inactive_minutes: 0,
            avg_upload_kbps: 0.0,
            dynamic_delete: false,
            delete_promotion_ended: false,
            delete_size_gb: String::new(),
            delete_files: true,
            delete_except_tags: "H&R".to_string(),
            torrent_upload_limit_kbps: 0,
            torrent_download_limit_kbps: 0,
            save_path: String::new(),
            category: "刷流".to_string(),
            extra_tags: String::new(),
            auto_archive_days: 30,
            last_brush_at: None,
            last_check_at: None,
        }
    }
}

impl Task {
    pub fn normalize_and_validate(&mut self) -> Result<(), String> {
        self.name = self.name.trim().to_string();
        if self.name.is_empty() || self.name.chars().count() > 80 {
            return Err("任务名称长度必须为 1 到 80 个字符".to_string());
        }
        if self.site_id <= 0 {
            return Err("站点 ID 必须大于 0".to_string());
        }
        if self.id.trim().is_empty() {
            self.id = Uuid::new_v4().simple().to_string();
        }
        self.brush_interval = self.brush_interval.clamp(1, 1440);
        self.check_interval = self.check_interval.clamp(1, 1440);
        self.max_add_per_run = self.max_add_per_run.clamp(1, 50);
        self.downloader = self.downloader.trim().to_ascii_lowercase();
        if !self.downloader.is_empty()
            && !matches!(self.downloader.as_str(), "qbittorrent" | "transmission")
        {
            return Err("下载器仅支持 qBittorrent 或 Transmission".to_string());
        }
        self.cron = self.cron.trim().to_string();
        if !self.cron.is_empty() {
            validate_cron(&self.cron)?;
        }
        if !matches!(self.promotion.as_str(), "all" | "free" | "2xfree") {
            return Err("促销类型无效".to_string());
        }
        if !self.include.trim().is_empty() {
            Regex::new(&self.include).map_err(|error| format!("包含规则无效: {error}"))?;
        }
        if !self.exclude.trim().is_empty() {
            Regex::new(&self.exclude).map_err(|error| format!("排除规则无效: {error}"))?;
        }
        for (label, value) in [
            ("种子大小", self.size_gb.as_str()),
            ("做种人数", self.seeders.as_str()),
            ("发布时间", self.pubtime_minutes.as_str()),
            ("动态删种阈值", self.delete_size_gb.as_str()),
        ] {
            if !value.trim().is_empty() && !valid_number_range(value) {
                return Err(format!("{label}必须是数字或范围"));
            }
        }
        if !self.active_time_range.trim().is_empty()
            && !Regex::new(r"^\d{2}:\d{2}-\d{2}:\d{2}$")
                .expect("valid time regex")
                .is_match(self.active_time_range.trim())
        {
            return Err("开启时间段格式应为 HH:MM-HH:MM".to_string());
        }
        Ok(())
    }

    pub fn unique_tag(&self) -> String {
        format!("MediaryFlow:{}", self.id)
    }
}

fn validate_cron(value: &str) -> Result<(), String> {
    use std::str::FromStr;

    if value.split_whitespace().count() != 5 {
        return Err("CRON 必须使用 5 段表达式".to_string());
    }
    cron::Schedule::from_str(&format!("0 {value}"))
        .map(|_| ())
        .map_err(|error| format!("CRON 表达式无效: {error}"))
}

fn valid_number_range(value: &str) -> bool {
    Regex::new(r"^\d+(?:\.\d+)?(?:-\d+(?:\.\d+)?)?$")
        .expect("valid number range regex")
        .is_match(value.trim())
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct State {
    #[serde(default = "state_version")]
    pub version: u32,
    #[serde(default)]
    pub tasks: Vec<Task>,
    #[serde(default)]
    pub torrents: Vec<ManagedTorrent>,
    #[serde(default)]
    pub runs: Vec<RunReport>,
    pub updated_at: Option<DateTime<Utc>>,
}

fn state_version() -> u32 {
    1
}

impl Default for State {
    fn default() -> Self {
        Self {
            version: state_version(),
            tasks: Vec::new(),
            torrents: Vec::new(),
            runs: Vec::new(),
            updated_at: None,
        }
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct ManagedTorrent {
    pub task_id: String,
    #[serde(default)]
    pub downloader: String,
    pub hash: String,
    pub title: String,
    pub site_id: i64,
    pub site_name: String,
    pub source_url: String,
    pub size: i64,
    pub hit_and_run: bool,
    pub promotion: String,
    pub free_until: Option<String>,
    pub added_at: DateTime<Utc>,
    pub deleted_at: Option<DateTime<Utc>>,
    pub delete_reason: Option<String>,
    pub last_uploaded: i64,
    pub last_downloaded: i64,
    pub last_sample_at: Option<DateTime<Utc>>,
    pub average_upload_speed: f64,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct RunReport {
    pub task_id: String,
    pub kind: String,
    pub started_at: DateTime<Utc>,
    pub finished_at: DateTime<Utc>,
    pub scanned: usize,
    pub accepted: usize,
    pub deleted: usize,
    pub skipped: usize,
    #[serde(default)]
    pub reasons: BTreeMap<String, usize>,
    pub errors: Vec<String>,
    pub message: String,
}

#[derive(Clone, Debug, Default, Deserialize)]
pub struct Site {
    pub id: i64,
    pub name: String,
    pub domain: String,
}

#[derive(Clone, Debug, Deserialize)]
pub struct TorrentCandidate {
    pub title: String,
    pub site_name: String,
    pub site_id: i64,
    pub size: i64,
    pub download_url: String,
    pub description: Option<String>,
    pub seeders: i32,
    pub publish_time: Option<String>,
    pub uploadvolumefactor: Option<f64>,
    pub downloadvolumefactor: Option<f64>,
    pub volume_factor: Option<String>,
    pub freedate: Option<String>,
    #[serde(default)]
    pub hit_and_run: bool,
}

#[derive(Clone, Debug, Deserialize)]
pub struct DownloaderTorrent {
    pub downloader: String,
    pub hash: String,
    pub name: String,
    pub size: i64,
    pub is_completed: bool,
    pub is_paused: bool,
    pub download_speed: i64,
    pub upload_speed: i64,
    pub downloaded: i64,
    pub uploaded: i64,
    pub ratio: f64,
    pub last_active_at: Option<i64>,
    pub seeding_time: i64,
    #[serde(default)]
    pub tags: Vec<String>,
}

#[derive(Clone, Debug, Default, Deserialize)]
pub struct DownloaderInfo {
    pub default: String,
    #[serde(default)]
    pub items: Vec<DownloaderInfoItem>,
}

#[derive(Clone, Debug, Default, Deserialize)]
pub struct DownloaderInfoItem {
    pub id: String,
    pub name: String,
}

#[derive(Clone, Debug, Default, Deserialize)]
pub struct SubscriptionTitle {
    pub name: String,
}
