//! Drive REST API client (DRIVE_NEW_API_URL). Mirrors og/sdk auth + storage.

use anyhow::{anyhow, Result};
use reqwest::header::{HeaderMap, HeaderValue, AUTHORIZATION, CONTENT_TYPE};
use reqwest::Client;
use serde_json::{json, Value};

use crate::config;
use crate::models::{Credentials, DriveFileData};

pub struct DriveApi {
    client: Client,
    base: String,
    /// Active workspace as (uuid, token). When set, requests carry the
    /// `x-internxt-workspace` header and folder/trash/file-entry calls route to
    /// the `/workspaces/{id}/...` endpoints.
    workspace: Option<(String, String)>,
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
            workspace: None,
        }
    }

    /// Build a client scoped to the credentials' active workspace (if any), so
    /// every request carries the workspace header and routes appropriately.
    pub fn for_credentials(creds: &Credentials) -> Self {
        let mut api = Self::new();
        if let Some(w) = &creds.workspace {
            api.workspace = Some((w.id.clone(), w.token.clone()));
        }
        api
    }

    fn url(&self, path: &str) -> String {
        format!("{}{}", self.base, path)
    }

    /// Authenticated headers, including `x-internxt-workspace` when a workspace
    /// is active (mirrors node SdkManager.init with a workspaceToken).
    fn auth_headers(&self, token: &str) -> Result<HeaderMap> {
        let mut h = base_headers();
        h.insert(AUTHORIZATION, HeaderValue::from_str(&format!("Bearer {token}"))?);
        if let Some((_, ws_token)) = &self.workspace {
            h.insert("x-internxt-workspace", HeaderValue::from_str(ws_token)?);
        }
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

    /// GET /users/cli/refresh -> new session token (refreshUserCredentials).
    /// Returns the `newToken`; the rest of the user identity is unchanged.
    pub async fn refresh_user_token(&self, token: &str) -> Result<String> {
        let resp = self
            .client
            .get(self.url("/users/cli/refresh"))
            .headers(self.auth_headers(token)?)
            .send()
            .await?;
        let v = Self::check(resp, "refreshUserCredentials").await?;
        v["newToken"]
            .as_str()
            .map(|s| s.to_string())
            .ok_or_else(|| anyhow!("no newToken in refresh response: {v}"))
    }

    /// GET /files/{uuid}/meta
    pub async fn get_file_meta(&self, token: &str, uuid: &str) -> Result<DriveFileData> {
        let resp = self
            .client
            .get(self.url(&format!("/files/{uuid}/meta")))
            .headers(self.auth_headers(token)?)
            .send()
            .await?;
        let v = Self::check(resp, "getFileMeta").await?;
        Ok(serde_json::from_value(v)?)
    }

    /// POST /files (createFileEntryByUuid), or POST /workspaces/{id}/files when a
    /// workspace is active. The workspace variant omits `creationTime` and adds a
    /// `date` field (mirrors og workspaceClient.createFileEntry).
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
        let (path, body) = match &self.workspace {
            Some((id, _)) => (
                format!("/workspaces/{id}/files"),
                json!({
                    "name": plain_name,
                    "plainName": plain_name,
                    "type": file_type,
                    "size": size,
                    "folderUuid": folder_uuid,
                    "fileId": file_id,
                    "bucket": bucket,
                    "encryptVersion": "03-aes",
                    "modificationTime": modification_time,
                    "date": modification_time,
                }),
            ),
            None => (
                "/files".to_string(),
                json!({
                    "plainName": plain_name,
                    "type": file_type,
                    "size": size,
                    "folderUuid": folder_uuid,
                    "fileId": file_id,
                    "bucket": bucket,
                    "encryptVersion": "03-aes",
                    "creationTime": creation_time,
                    "modificationTime": modification_time,
                }),
            ),
        };
        let resp = self
            .client
            .post(self.url(&path))
            .headers(self.auth_headers(token)?)
            .json(&body)
            .send()
            .await?;
        let v = Self::check(resp, "createFileEntry").await?;
        Ok(serde_json::from_value(v)?)
    }

    /// GET /workspaces/ — available + pending workspaces (WorkspacesResponse).
    pub async fn get_workspaces(&self, token: &str) -> Result<Value> {
        let resp = self
            .client
            .get(self.url("/workspaces/"))
            .headers(self.auth_headers(token)?)
            .send()
            .await?;
        Self::check(resp, "getWorkspaces").await
    }

    /// GET /workspaces/{id}/credentials — network creds + token header for a workspace.
    pub async fn get_workspace_credentials(&self, token: &str, workspace_id: &str) -> Result<Value> {
        let resp = self
            .client
            .get(self.url(&format!("/workspaces/{workspace_id}/credentials")))
            .headers(self.auth_headers(token)?)
            .send()
            .await?;
        Self::check(resp, "getWorkspaceCredentials").await
    }

    /// GET /auth/logout (best effort; invalidates the session token server-side).
    pub async fn logout(&self, token: &str) -> Result<()> {
        let resp = self
            .client
            .get(self.url("/auth/logout"))
            .headers(self.auth_headers(token)?)
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
            .headers(self.auth_headers(token)?)
            .send()
            .await?;
        Self::check(resp, "getFolderMeta").await
    }

    /// One page of subfolders (returns `.folders`/`.result`). Personal endpoint is
    /// `/folders/content/{uuid}/folders/`; workspace is `/workspaces/{id}/folders/{uuid}/folders/`.
    pub async fn get_folder_subfolders(
        &self,
        token: &str,
        uuid: &str,
        offset: u32,
    ) -> Result<Value> {
        let path = match &self.workspace {
            Some((id, _)) => {
                format!("/workspaces/{id}/folders/{uuid}/folders/?offset={offset}&limit=50")
            }
            None => format!(
                "/folders/content/{uuid}/folders/?offset={offset}&limit=50&sort=plainName&order=ASC"
            ),
        };
        let resp = self
            .client
            .get(self.url(&path))
            .headers(self.auth_headers(token)?)
            .send()
            .await?;
        Self::check(resp, "getFolderFolders").await
    }

    /// One page of files (returns `.files`/`.result`). Personal endpoint is
    /// `/folders/content/{uuid}/files/`; workspace is `/workspaces/{id}/folders/{uuid}/files/`.
    pub async fn get_folder_subfiles(
        &self,
        token: &str,
        uuid: &str,
        offset: u32,
    ) -> Result<Value> {
        let path = match &self.workspace {
            Some((id, _)) => {
                format!("/workspaces/{id}/folders/{uuid}/files/?offset={offset}&limit=50")
            }
            None => format!(
                "/folders/content/{uuid}/files/?offset={offset}&limit=50&sort=plainName&order=ASC"
            ),
        };
        let resp = self
            .client
            .get(self.url(&path))
            .headers(self.auth_headers(token)?)
            .send()
            .await?;
        Self::check(resp, "getFolderFiles").await
    }

    /// Create a folder by parent uuid. Routes to `/workspaces/{id}/folders` when
    /// a workspace is active (payload uses `name` instead of `plainName`).
    pub async fn create_folder(
        &self,
        token: &str,
        plain_name: &str,
        parent_folder_uuid: &str,
    ) -> Result<Value> {
        let (path, body) = match &self.workspace {
            Some((id, _)) => (
                format!("/workspaces/{id}/folders"),
                json!({ "name": plain_name, "parentFolderUuid": parent_folder_uuid }),
            ),
            None => (
                "/folders".to_string(),
                json!({ "plainName": plain_name, "parentFolderUuid": parent_folder_uuid }),
            ),
        };
        let resp = self
            .client
            .post(self.url(&path))
            .headers(self.auth_headers(token)?)
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
            .headers(self.auth_headers(token)?)
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
            .headers(self.auth_headers(token)?)
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
            .headers(self.auth_headers(token)?)
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
            .headers(self.auth_headers(token)?)
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
            .headers(self.auth_headers(token)?)
            .json(&json!({ "items": items }))
            .send()
            .await?;
        Self::check(resp, "trashItems").await?;
        Ok(())
    }

    /// One page of trash; `kind` is "files" or "folders". Personal uses
    /// `/storage/trash/paginated`; workspace uses `/workspaces/{id}/trash` with a
    /// singular `type` (`file`/`folder`).
    pub async fn trash_paginated(&self, token: &str, kind: &str, offset: u32) -> Result<Value> {
        let path = match &self.workspace {
            Some((id, _)) => {
                let ws_type = if kind == "folders" { "folder" } else { "file" };
                format!("/workspaces/{id}/trash?offset={offset}&limit=50&type={ws_type}")
            }
            None => {
                format!("/storage/trash/paginated?limit=50&offset={offset}&type={kind}&root=true")
            }
        };
        let resp = self
            .client
            .get(self.url(&path))
            .headers(self.auth_headers(token)?)
            .send()
            .await?;
        Self::check(resp, "getTrashPaginated").await
    }

    /// Empty the trash permanently. Personal: DELETE /storage/trash/all;
    /// workspace: DELETE /workspaces/{id}/trash.
    pub async fn clear_trash(&self, token: &str) -> Result<()> {
        let path = match &self.workspace {
            Some((id, _)) => format!("/workspaces/{id}/trash"),
            None => "/storage/trash/all".to_string(),
        };
        let resp = self
            .client
            .delete(self.url(&path))
            .headers(self.auth_headers(token)?)
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
            .headers(self.auth_headers(token)?)
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
            .headers(self.auth_headers(token)?)
            .send()
            .await?;
        Self::check(resp, "deleteFolder").await?;
        Ok(())
    }
}
