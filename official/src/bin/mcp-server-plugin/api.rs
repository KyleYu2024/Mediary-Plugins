use reqwest::{Client, Response};
use serde_json::{Value, json};
use std::time::Duration;

const CONNECT_TIMEOUT: Duration = Duration::from_secs(5);
const REQUEST_TIMEOUT: Duration = Duration::from_secs(45);
const MAX_RESPONSE_BYTES: u64 = 2 * 1024 * 1024;

pub(crate) struct PluginContext {
    api_url: String,
    api_token: String,
    http_client: Client,
}

impl PluginContext {
    pub(crate) fn new(api_url: String, api_token: String) -> Result<Self, String> {
        let api_url = api_url.trim().trim_end_matches('/').to_string();
        if api_url.is_empty() {
            return Err("MEDIARY_PLUGIN_API_URL 不能为空".to_string());
        }
        if api_token.trim().is_empty() {
            return Err("MEDIARY_PLUGIN_TOKEN 不能为空".to_string());
        }
        let http_client = Client::builder()
            .connect_timeout(CONNECT_TIMEOUT)
            .timeout(REQUEST_TIMEOUT)
            .build()
            .map_err(|error| format!("创建 HTTP 客户端失败: {error}"))?;
        Ok(Self {
            api_url,
            api_token,
            http_client,
        })
    }

    pub(crate) async fn get(&self, path: &str) -> Result<Value, String> {
        let response = self
            .http_client
            .get(self.url(path))
            .bearer_auth(&self.api_token)
            .send()
            .await
            .map_err(request_error)?;
        parse_response(response).await
    }

    pub(crate) async fn get_query(
        &self,
        path: &str,
        query: &[(&str, String)],
    ) -> Result<Value, String> {
        let response = self
            .http_client
            .get(self.url(path))
            .query(query)
            .bearer_auth(&self.api_token)
            .send()
            .await
            .map_err(request_error)?;
        parse_response(response).await
    }

    pub(crate) async fn post(&self, path: &str, payload: Value) -> Result<Value, String> {
        let response = self
            .http_client
            .post(self.url(path))
            .bearer_auth(&self.api_token)
            .json(&payload)
            .send()
            .await
            .map_err(request_error)?;
        parse_response(response).await
    }

    pub(crate) async fn delete(&self, path: &str) -> Result<Value, String> {
        let response = self
            .http_client
            .delete(self.url(path))
            .bearer_auth(&self.api_token)
            .send()
            .await
            .map_err(request_error)?;
        parse_response(response).await
    }

    fn url(&self, path: &str) -> String {
        format!("{}{}", self.api_url, path)
    }
}

fn request_error(error: reqwest::Error) -> String {
    if error.is_timeout() {
        "Mediary API 请求超时".to_string()
    } else if error.is_connect() {
        format!("无法连接 Mediary API: {error}")
    } else {
        format!("Mediary API 请求失败: {error}")
    }
}

async fn parse_response(response: Response) -> Result<Value, String> {
    let status = response.status();
    if response
        .content_length()
        .is_some_and(|length| length > MAX_RESPONSE_BYTES)
    {
        return Err("Mediary API 响应超过 2 MB 上限".to_string());
    }
    let bytes = response
        .bytes()
        .await
        .map_err(|error| format!("读取 Mediary API 响应失败: {error}"))?;
    if bytes.len() as u64 > MAX_RESPONSE_BYTES {
        return Err("Mediary API 响应超过 2 MB 上限".to_string());
    }
    let body = if bytes.iter().all(u8::is_ascii_whitespace) {
        json!({})
    } else {
        serde_json::from_slice(&bytes)
            .map_err(|error| format!("Mediary API 返回了无效 JSON: {error}"))?
    };
    if status.is_success() {
        return Ok(body);
    }
    let message = body
        .get("error")
        .or_else(|| body.get("message"))
        .and_then(Value::as_str)
        .unwrap_or("未知错误");
    Err(format!("Mediary API 错误 ({}): {message}", status.as_u16()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_empty_context_values() {
        assert!(PluginContext::new("".into(), "token".into()).is_err());
        assert!(PluginContext::new("http://localhost/api".into(), " ".into()).is_err());
    }
}
