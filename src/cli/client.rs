use anyhow::{Context, Result, bail};
use reqwest::Client;
use serde::de::DeserializeOwned;
use serde_json::Value;

pub struct OrionClient {
    base_url: String,
    http: Client,
}

impl OrionClient {
    pub fn new(base_url: &str) -> Result<Self> {
        let base_url = base_url.trim_end_matches('/').to_string();
        let http = Client::builder()
            .timeout(std::time::Duration::from_secs(30))
            .build()
            .context("Failed to create HTTP client")?;
        Ok(Self { base_url, http })
    }

    pub async fn get<T: DeserializeOwned>(&self, path: &str) -> Result<T> {
        let url = format!("{}{path}", self.base_url);
        let resp = self
            .http
            .get(&url)
            .send()
            .await
            .with_context(|| format!("Failed to connect to {url}"))?;
        self.handle_response(resp).await
    }

    pub async fn get_text(&self, path: &str) -> Result<String> {
        let url = format!("{}{path}", self.base_url);
        let resp = self
            .http
            .get(&url)
            .send()
            .await
            .with_context(|| format!("Failed to connect to {url}"))?;
        if !resp.status().is_success() {
            let status = resp.status();
            let body = resp.text().await.unwrap_or_default();
            bail!("Request failed ({status}): {body}");
        }
        resp.text().await.context("Failed to read response body")
    }

    pub async fn post<T: DeserializeOwned>(&self, path: &str, body: &Value) -> Result<T> {
        let url = format!("{}{path}", self.base_url);
        let resp = self
            .http
            .post(&url)
            .json(body)
            .send()
            .await
            .with_context(|| format!("Failed to connect to {url}"))?;
        self.handle_response(resp).await
    }

    pub async fn post_empty<T: DeserializeOwned>(&self, path: &str) -> Result<T> {
        let url = format!("{}{path}", self.base_url);
        let resp = self
            .http
            .post(&url)
            .send()
            .await
            .with_context(|| format!("Failed to connect to {url}"))?;
        self.handle_response(resp).await
    }

    pub async fn put<T: DeserializeOwned>(&self, path: &str, body: &Value) -> Result<T> {
        let url = format!("{}{path}", self.base_url);
        let resp = self
            .http
            .put(&url)
            .json(body)
            .send()
            .await
            .with_context(|| format!("Failed to connect to {url}"))?;
        self.handle_response(resp).await
    }

    pub async fn patch<T: DeserializeOwned>(&self, path: &str, body: &Value) -> Result<T> {
        let url = format!("{}{path}", self.base_url);
        let resp = self
            .http
            .patch(&url)
            .json(body)
            .send()
            .await
            .with_context(|| format!("Failed to connect to {url}"))?;
        self.handle_response(resp).await
    }

    pub async fn delete_request(&self, path: &str) -> Result<()> {
        let url = format!("{}{path}", self.base_url);
        let resp = self
            .http
            .delete(&url)
            .send()
            .await
            .with_context(|| format!("Failed to connect to {url}"))?;
        if !resp.status().is_success() {
            let status = resp.status();
            let body: Value = resp.json().await.unwrap_or_default();
            if let Some(err) = body.get("error") {
                let code = err["code"].as_str().unwrap_or("UNKNOWN");
                let msg = err["message"].as_str().unwrap_or("Unknown error");
                bail!("[{code}] {msg}");
            }
            bail!("Request failed ({status})");
        }
        Ok(())
    }

    async fn handle_response<T: DeserializeOwned>(&self, resp: reqwest::Response) -> Result<T> {
        let status = resp.status();
        if !status.is_success() {
            let body: Value = resp.json().await.unwrap_or_default();
            if let Some(err) = body.get("error") {
                let code = err["code"].as_str().unwrap_or("UNKNOWN");
                let msg = err["message"].as_str().unwrap_or("Unknown error");
                bail!("[{code}] {msg}");
            }
            bail!("Request failed ({status})");
        }
        resp.json::<T>()
            .await
            .context("Failed to parse response JSON")
    }
}
