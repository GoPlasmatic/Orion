use anyhow::{Context, Result, bail};
use reqwest::Client;
use serde::de::DeserializeOwned;
use serde_json::Value;

#[derive(Debug, Clone)]
pub struct OrionClient {
    base_url: String,
    http: Client,
    api_key: Option<String>,
    api_key_header: Option<String>,
}

impl OrionClient {
    pub fn new(base_url: &str) -> Result<Self> {
        let base_url = base_url.trim_end_matches('/').to_string();
        let http = Client::builder()
            .timeout(std::time::Duration::from_secs(30))
            .build()
            .context("Failed to create HTTP client")?;
        Ok(Self {
            base_url,
            http,
            api_key: None,
            api_key_header: None,
        })
    }

    pub fn base_url(&self) -> &str {
        &self.base_url
    }

    pub fn with_api_key(mut self, api_key: String, header: Option<String>) -> Self {
        self.api_key = Some(api_key);
        self.api_key_header = header;
        self
    }

    fn apply_auth(&self, req: reqwest::RequestBuilder) -> reqwest::RequestBuilder {
        match (&self.api_key, &self.api_key_header) {
            (Some(key), Some(header)) if !header.eq_ignore_ascii_case("authorization") => {
                req.header(header, key)
            }
            (Some(key), _) => req.header("Authorization", format!("Bearer {key}")),
            _ => req,
        }
    }

    pub async fn get<T: DeserializeOwned>(&self, path: &str) -> Result<T> {
        let url = format!("{}{path}", self.base_url);
        let resp = self
            .apply_auth(self.http.get(&url))
            .send()
            .await
            .with_context(|| Self::connection_hint(&url))?;
        self.handle_response(resp).await
    }

    pub async fn get_text(&self, path: &str) -> Result<String> {
        let url = format!("{}{path}", self.base_url);
        let resp = self
            .apply_auth(self.http.get(&url))
            .send()
            .await
            .with_context(|| Self::connection_hint(&url))?;
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
            .apply_auth(self.http.post(&url))
            .json(body)
            .send()
            .await
            .with_context(|| Self::connection_hint(&url))?;
        self.handle_response(resp).await
    }

    pub async fn post_empty<T: DeserializeOwned>(&self, path: &str) -> Result<T> {
        let url = format!("{}{path}", self.base_url);
        let resp = self
            .apply_auth(self.http.post(&url))
            .send()
            .await
            .with_context(|| Self::connection_hint(&url))?;
        self.handle_response(resp).await
    }

    pub async fn put<T: DeserializeOwned>(&self, path: &str, body: &Value) -> Result<T> {
        let url = format!("{}{path}", self.base_url);
        let resp = self
            .apply_auth(self.http.put(&url))
            .json(body)
            .send()
            .await
            .with_context(|| Self::connection_hint(&url))?;
        self.handle_response(resp).await
    }

    pub async fn patch<T: DeserializeOwned>(&self, path: &str, body: &Value) -> Result<T> {
        let url = format!("{}{path}", self.base_url);
        let resp = self
            .apply_auth(self.http.patch(&url))
            .json(body)
            .send()
            .await
            .with_context(|| Self::connection_hint(&url))?;
        self.handle_response(resp).await
    }

    pub async fn delete_request(&self, path: &str) -> Result<()> {
        let url = format!("{}{path}", self.base_url);
        let resp = self
            .apply_auth(self.http.delete(&url))
            .send()
            .await
            .with_context(|| Self::connection_hint(&url))?;
        Self::check_error(resp).await?;
        Ok(())
    }

    async fn handle_response<T: DeserializeOwned>(&self, resp: reqwest::Response) -> Result<T> {
        let resp = Self::check_error(resp).await?;
        resp.json::<T>()
            .await
            .context("Failed to parse response JSON")
    }

    fn connection_hint(url: &str) -> String {
        format!(
            "Failed to connect to {url}\n  \
             Hint: Is the server running? Check with 'orion-cli config show' or use '--server <url>'"
        )
    }

    fn error_hint(code: &str) -> &'static str {
        match code {
            "AUTH_FAILED" | "UNAUTHORIZED" => {
                "\n  Hint: Check your --api-key or ORION_API_KEY environment variable"
            }
            "NOT_FOUND" => {
                "\n  Hint: Verify the resource ID exists with the corresponding 'list' command"
            }
            "VALIDATION_ERROR" | "INVALID_INPUT" => {
                "\n  Hint: Check the JSON structure. Use 'orion-cli workflows validate -f <file>' to validate"
            }
            "CONFLICT" | "ALREADY_EXISTS" => {
                "\n  Hint: A resource with this ID or name may already exist"
            }
            _ => "",
        }
    }

    async fn check_error(resp: reqwest::Response) -> Result<reqwest::Response> {
        if !resp.status().is_success() {
            let status = resp.status();
            let body: Value = resp.json().await.unwrap_or_default();
            if let Some(err) = body.get("error") {
                let code = err["code"].as_str().unwrap_or("UNKNOWN");
                let msg = err["message"].as_str().unwrap_or("Unknown error");
                let hint = Self::error_hint(code);
                bail!("[{code}] {msg}{hint}");
            }
            if status == reqwest::StatusCode::UNAUTHORIZED {
                bail!(
                    "Unauthorized ({status})\n  Hint: Check your --api-key or ORION_API_KEY environment variable"
                );
            }
            bail!("Request failed ({status})");
        }
        Ok(resp)
    }
}
