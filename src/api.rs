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

    fn auth_headers(token: &str) -> Result<HeaderMap> {
        let mut h = base_headers();
        h.insert(AUTHORIZATION, HeaderValue::from_str(&format!("Bearer {token}"))?);
        Ok(h)
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

    /// GET /auth/logout (best effort; invalidates the session token server-side).
    pub async fn logout(&self, token: &str) -> Result<()> {
        let resp = self
            .client
            .get(self.url("/auth/logout"))
            .headers(Self::auth_headers(token)?)
            .send()
            .await?;
        Self::check(resp, "logout").await?;
        Ok(())
    }

    /// GET /folders/{uuid}/meta
    pub async fn get_folder_meta(&self, token: &str, uuid: &str) -> Result<Value> {
        let resp = self
            .client
            .get(self.url(&format!("/folders/{uuid}/meta")))
            .headers(Self::auth_headers(token)?)
            .send()
            .await?;
        Self::check(resp, "getFolderMeta").await
    }

    /// GET /folders/content/{uuid}/folders/ — one page (returns `.folders`).
    pub async fn get_folder_subfolders(
        &self,
        token: &str,
        uuid: &str,
        offset: u32,
    ) -> Result<Value> {
        let path = format!(
            "/folders/content/{uuid}/folders/?offset={offset}&limit=50&sort=plainName&order=ASC"
        );
        let resp = self
            .client
            .get(self.url(&path))
            .headers(Self::auth_headers(token)?)
            .send()
            .await?;
        Self::check(resp, "getFolderFolders").await
    }

    /// GET /folders/content/{uuid}/files/ — one page (returns `.files`).
    pub async fn get_folder_subfiles(
        &self,
        token: &str,
        uuid: &str,
        offset: u32,
    ) -> Result<Value> {
        let path = format!(
            "/folders/content/{uuid}/files/?offset={offset}&limit=50&sort=plainName&order=ASC"
        );
        let resp = self
            .client
            .get(self.url(&path))
            .headers(Self::auth_headers(token)?)
            .send()
            .await?;
        Self::check(resp, "getFolderFiles").await
    }

    /// POST /folders — create folder by parent uuid.
    pub async fn create_folder(
        &self,
        token: &str,
        plain_name: &str,
        parent_folder_uuid: &str,
    ) -> Result<Value> {
        let body = json!({ "plainName": plain_name, "parentFolderUuid": parent_folder_uuid });
        let resp = self
            .client
            .post(self.url("/folders"))
            .headers(Self::auth_headers(token)?)
            .json(&body)
            .send()
            .await?;
        Self::check(resp, "createFolder").await
    }

    /// PATCH /folders/{uuid} — move folder into a destination folder.
    pub async fn move_folder(&self, token: &str, uuid: &str, destination: &str) -> Result<Value> {
        let resp = self
            .client
            .patch(self.url(&format!("/folders/{uuid}")))
            .headers(Self::auth_headers(token)?)
            .json(&json!({ "destinationFolder": destination }))
            .send()
            .await?;
        Self::check(resp, "moveFolder").await
    }

    /// PATCH /files/{uuid} — move file into a destination folder.
    pub async fn move_file(&self, token: &str, uuid: &str, destination: &str) -> Result<Value> {
        let resp = self
            .client
            .patch(self.url(&format!("/files/{uuid}")))
            .headers(Self::auth_headers(token)?)
            .json(&json!({ "destinationFolder": destination }))
            .send()
            .await?;
        Self::check(resp, "moveFile").await
    }

    /// PUT /folders/{uuid}/meta — rename folder.
    pub async fn rename_folder(&self, token: &str, uuid: &str, plain_name: &str) -> Result<()> {
        let resp = self
            .client
            .put(self.url(&format!("/folders/{uuid}/meta")))
            .headers(Self::auth_headers(token)?)
            .json(&json!({ "plainName": plain_name }))
            .send()
            .await?;
        Self::check(resp, "renameFolder").await?;
        Ok(())
    }

    /// PUT /files/{uuid}/meta — rename file (plainName + type).
    pub async fn rename_file(
        &self,
        token: &str,
        uuid: &str,
        plain_name: &str,
        file_type: &str,
    ) -> Result<()> {
        let resp = self
            .client
            .put(self.url(&format!("/files/{uuid}/meta")))
            .headers(Self::auth_headers(token)?)
            .json(&json!({ "plainName": plain_name, "type": file_type }))
            .send()
            .await?;
        Self::check(resp, "renameFile").await?;
        Ok(())
    }

    /// POST /storage/trash/add — move items to trash. `items` = [{uuid,type}].
    pub async fn trash_items(&self, token: &str, items: Value) -> Result<()> {
        let resp = self
            .client
            .post(self.url("/storage/trash/add"))
            .headers(Self::auth_headers(token)?)
            .json(&json!({ "items": items }))
            .send()
            .await?;
        Self::check(resp, "trashItems").await?;
        Ok(())
    }

    /// GET /storage/trash/paginated — one page of trash; `kind` is "files" or "folders".
    pub async fn trash_paginated(&self, token: &str, kind: &str, offset: u32) -> Result<Value> {
        let path =
            format!("/storage/trash/paginated?limit=50&offset={offset}&type={kind}&root=true");
        let resp = self
            .client
            .get(self.url(&path))
            .headers(Self::auth_headers(token)?)
            .send()
            .await?;
        Self::check(resp, "getTrashPaginated").await
    }

    /// DELETE /storage/trash/all — empty the trash permanently.
    pub async fn clear_trash(&self, token: &str) -> Result<()> {
        let resp = self
            .client
            .delete(self.url("/storage/trash/all"))
            .headers(Self::auth_headers(token)?)
            .send()
            .await?;
        Self::check(resp, "clearTrash").await?;
        Ok(())
    }

    /// DELETE /files/{uuid} — permanently delete a file.
    pub async fn delete_file(&self, token: &str, uuid: &str) -> Result<()> {
        let resp = self
            .client
            .delete(self.url(&format!("/files/{uuid}")))
            .headers(Self::auth_headers(token)?)
            .send()
            .await?;
        Self::check(resp, "deleteFile").await?;
        Ok(())
    }

    /// DELETE /folders/{uuid} — permanently delete a folder.
    pub async fn delete_folder(&self, token: &str, uuid: &str) -> Result<()> {
        let resp = self
            .client
            .delete(self.url(&format!("/folders/{uuid}")))
            .headers(Self::auth_headers(token)?)
            .send()
            .await?;
        Self::check(resp, "deleteFolder").await?;
        Ok(())
    }
}
