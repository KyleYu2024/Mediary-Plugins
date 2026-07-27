// SPDX-License-Identifier: GPL-3.0-only

use reqwest::{Client, Method, Response};
use serde::Deserialize;
use serde_json::{Value, json};
use std::{env, time::Duration};

use super::models::{DownloaderInfo, DownloaderTorrent, Site, SubscriptionTitle, TorrentCandidate};

#[derive(Clone)]
pub struct MediaryClient {
    base_url: String,
    token: String,
    client: Client,
}

#[derive(Deserialize)]
struct ListResponse<T> {
    items: Vec<T>,
}

#[derive(Deserialize)]
pub struct AddDownloadResponse {
    pub hash: Option<String>,
}

pub struct ControlRequest<'a> {
    pub action: &'a str,
    pub hashes: Vec<String>,
    pub upload_limit_kbps: Option<i64>,
    pub download_limit_kbps: Option<i64>,
    pub tags: Option<Vec<String>>,
    pub delete_files: bool,
    pub downloader: Option<&'a str>,
}

impl MediaryClient {
    pub fn from_env() -> Result<Self, String> {
        let base_url = env::var("MEDIARY_PLUGIN_API_URL")
            .map_err(|_| "缺少 MEDIARY_PLUGIN_API_URL".to_string())?;
        let token = env::var("MEDIARY_PLUGIN_TOKEN")
            .map_err(|_| "缺少 MEDIARY_PLUGIN_TOKEN".to_string())?;
        let client = Client::builder()
            .connect_timeout(Duration::from_secs(10))
            .timeout(Duration::from_secs(90))
            .build()
            .map_err(|error| error.to_string())?;
        Ok(Self {
            base_url: base_url.trim_end_matches('/').to_string(),
            token,
            client,
        })
    }

    pub async fn sites(&self) -> Result<Vec<Site>, String> {
        let response = self.get("/plugin/sites").await?;
        parse_list(response).await
    }

    pub async fn candidates(
        &self,
        site_id: i64,
        rss_support: bool,
    ) -> Result<Vec<TorrentCandidate>, String> {
        let source = if rss_support { "rss" } else { "browse" };
        let path = format!("/plugin/torrents?site_ids={site_id}&limit=1000&source={source}");
        let response = self.get(&path).await?;
        parse_list(response).await
    }

    pub async fn downloader_info(&self) -> Result<DownloaderInfo, String> {
        let response = self.get("/plugin/downloader").await?;
        parse_json(response).await
    }

    pub async fn downloader_torrents(
        &self,
        downloader: Option<&str>,
    ) -> Result<Vec<DownloaderTorrent>, String> {
        let path = downloader
            .map(|value| format!("/plugin/downloader/torrents?downloader={value}"))
            .unwrap_or_else(|| "/plugin/downloader/torrents".to_string());
        let response = self.get(&path).await?;
        parse_list(response).await
    }

    pub async fn subscription_titles(&self) -> Result<Vec<SubscriptionTitle>, String> {
        let response = self.get("/plugin/subscriptions/titles").await?;
        parse_list(response).await
    }

    pub async fn add_download(
        &self,
        candidate: &TorrentCandidate,
        save_path: Option<&str>,
        category: Option<&str>,
        tags: Vec<String>,
        downloader: Option<&str>,
        skip_download_tips: bool,
    ) -> Result<AddDownloadResponse, String> {
        let response = self
            .request(
                Method::POST,
                "/plugin/downloads",
                Some(json!({
                    "torrent_url": candidate.download_url,
                    "site_id": candidate.site_id,
                    "site_name": candidate.site_name,
                    "downloader": downloader,
                    "save_path": save_path,
                    "category": category,
                    "tags": tags,
                    "paused": false,
                    "skip_download_tips": skip_download_tips,
                })),
            )
            .await?;
        parse_json(response).await
    }

    pub async fn control(&self, payload: ControlRequest<'_>) -> Result<(), String> {
        self.request(
            Method::POST,
            "/plugin/downloader/torrents/control",
            Some(json!({
                "action": payload.action,
                "downloader": payload.downloader,
                "hashes": payload.hashes,
                "upload_limit_kbps": payload.upload_limit_kbps,
                "download_limit_kbps": payload.download_limit_kbps,
                "tags": payload.tags.unwrap_or_default(),
                "delete_files": payload.delete_files,
            })),
        )
        .await?;
        Ok(())
    }

    pub async fn notify(&self, title: &str, content: &str) -> Result<(), String> {
        self.request(
            Method::POST,
            "/plugin/notifications",
            Some(json!({"title": title, "content": content})),
        )
        .await?;
        Ok(())
    }

    async fn get(&self, path: &str) -> Result<Response, String> {
        self.request(Method::GET, path, None).await
    }

    async fn request(
        &self,
        method: Method,
        path: &str,
        body: Option<Value>,
    ) -> Result<Response, String> {
        let mut request = self
            .client
            .request(method, format!("{}{}", self.base_url, path))
            .bearer_auth(&self.token);
        if let Some(body) = body {
            request = request.json(&body);
        }
        let response = request.send().await.map_err(|error| error.to_string())?;
        if response.status().is_success() {
            Ok(response)
        } else {
            let status = response.status();
            let body = response.text().await.unwrap_or_default();
            let message = serde_json::from_str::<Value>(&body)
                .ok()
                .and_then(|value| {
                    value
                        .get("error")
                        .and_then(Value::as_str)
                        .map(str::to_string)
                })
                .unwrap_or(body);
            Err(format!("Mediary API {status}: {message}"))
        }
    }
}

async fn parse_list<T>(response: Response) -> Result<Vec<T>, String>
where
    T: for<'de> Deserialize<'de>,
{
    response
        .json::<ListResponse<T>>()
        .await
        .map(|response| response.items)
        .map_err(|error| format!("Mediary 列表响应无效: {error}"))
}

async fn parse_json<T>(response: Response) -> Result<T, String>
where
    T: for<'de> Deserialize<'de>,
{
    response
        .json::<T>()
        .await
        .map_err(|error| format!("Mediary 响应无效: {error}"))
}
