//! Drive REST API client (DRIVE_NEW_API_URL). Mirrors og/sdk auth + storage.

use anyhow::{anyhow, Result};
use reqwest::header::{HeaderMap, HeaderValue, AUTHORIZATION, CONTENT_TYPE};
use reqwest::Client;
use serde_json::{json, Value};

use crate::config;
use crate::models::DriveFileData;

pub struct DriveApi {
    client: Client,
    base: String,
}

fn base_headers() -> HeaderMap {
    let mut h = HeaderMap::new();
    h.insert(
        CONTENT_TYPE,
        HeaderValue::from_static("application/json; charset=utf-8"),
    );
    h.insert("internxt-version", HeaderValue::from_static(config::CLIENT_VERSION));
    h.insert("internxt-client", HeaderValue::from_static(config::CLIENT_NAME));
    if let Ok(v) = HeaderValue::from_str(&config::desktop_header()) {
        h.insert("x-internxt-desktop-header", v);
    }
    h
}

impl DriveApi {
    pub fn new() -> Self {
        DriveApi {
            client: Client::new(),
            base: config::drive_api_url(),
        }
    }

    fn url(&self, path: &str) -> String {
        format!("{}{}", self.base, path)
    }

    async fn check(resp: reqwest::Response, ctx: &str) -> Result<Value> {
        let status = resp.status();
        let text = resp.text().await.unwrap_or_default();
        if !status.is_success() {
            return Err(anyhow!("{ctx} failed: HTTP {status}: {text}"));
        }
        if text.is_empty() {
            return Ok(Value::Null);
        }
        Ok(serde_json::from_str(&text)?)
    }

    /// POST /auth/login -> (encrypted_salt (sKey), tfa_enabled)
    pub async fn security_details(&self, email: &str) -> Result<(String, bool)> {
        let resp = self
            .client
            .post(self.url("/auth/login"))
            .headers(base_headers())
            .json(&json!({ "email": email }))
            .send()
            .await?;
        let v = Self::check(resp, "securityDetails").await?;
        let skey = v["sKey"]
            .as_str()
            .ok_or_else(|| anyhow!("no sKey in response: {v}"))?
            .to_string();
        let tfa = v["tfa"].as_bool().unwrap_or(false) || v["tfa"].is_string();
        Ok((skey, tfa))
    }

    /// POST /auth/login/access (no keys) -> full response json (newToken, user, ...)
    pub async fn login_access(
        &self,
        email: &str,
        encrypted_password_hash: &str,
        tfa: Option<&str>,
    ) -> Result<Value> {
        let body = json!({
            "email": email,
            "password": encrypted_password_hash,
            "tfa": tfa,
        });
        let resp = self
            .client
            .post(self.url("/auth/login/access"))
            .headers(base_headers())
            .json(&body)
            .send()
            .await?;
        Self::check(resp, "loginAccess").await
    }

    /// GET /files/{uuid}/meta
    pub async fn get_file_meta(&self, token: &str, uuid: &str) -> Result<DriveFileData> {
        let mut headers = base_headers();
        headers.insert(
            AUTHORIZATION,
            HeaderValue::from_str(&format!("Bearer {token}"))?,
        );
        let resp = self
            .client
            .get(self.url(&format!("/files/{uuid}/meta")))
            .headers(headers)
            .send()
            .await?;
        let v = Self::check(resp, "getFileMeta").await?;
        Ok(serde_json::from_value(v)?)
    }

    /// POST /files  (createFileEntryByUuid)
    #[allow(clippy::too_many_arguments)]
    pub async fn create_file_entry(
        &self,
        token: &str,
        plain_name: &str,
        file_type: &str,
        size: u64,
        folder_uuid: &str,
        file_id: &str,
        bucket: &str,
        creation_time: &str,
        modification_time: &str,
    ) -> Result<DriveFileData> {
        let mut headers = base_headers();
        headers.insert(
            AUTHORIZATION,
            HeaderValue::from_str(&format!("Bearer {token}"))?,
        );
        let body = json!({
            "plainName": plain_name,
            "type": file_type,
            "size": size,
            "folderUuid": folder_uuid,
            "fileId": file_id,
            "bucket": bucket,
            "encryptVersion": "03-aes",
            "creationTime": creation_time,
            "modificationTime": modification_time,
        });
        let resp = self
            .client
            .post(self.url("/files"))
            .headers(headers)
            .json(&body)
            .send()
            .await?;
        let v = Self::check(resp, "createFileEntry").await?;
        Ok(serde_json::from_value(v)?)
    }
}
