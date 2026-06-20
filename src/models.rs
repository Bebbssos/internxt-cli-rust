use serde::{Deserialize, Serialize};

/// Persisted credentials (our own format; stored AES-encrypted like the node CLI).
#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct Credentials {
    /// JWT used as Bearer for the drive API (the node CLI's `newToken`).
    pub token: String,
    pub user: UserInfo,
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct UserInfo {
    pub email: String,
    /// Plain (decrypted) mnemonic.
    pub mnemonic: String,
    pub bucket: String,
    pub bridge_user: String,
    pub user_id: String,
    pub root_folder_id: String,
}

// ---- Network (bridge) DTOs ----

#[derive(Deserialize, Debug)]
pub struct StartUploadResponse {
    pub uploads: Vec<UploadSlot>,
}

#[derive(Deserialize, Debug)]
pub struct UploadSlot {
    pub uuid: String,
    #[serde(default)]
    pub url: Option<String>,
    #[serde(default)]
    pub urls: Option<Vec<String>>,
    #[serde(rename = "UploadId", default)]
    pub upload_id: Option<String>,
}

#[derive(Deserialize, Debug)]
pub struct FinishUploadResponse {
    pub id: String,
}

#[derive(Deserialize, Debug)]
pub struct DownloadLinksResponse {
    pub index: String,
    pub shards: Vec<DownloadShard>,
    #[serde(default)]
    pub version: Option<u32>,
    pub size: u64,
}

#[derive(Deserialize, Debug, Clone)]
pub struct DownloadShard {
    pub index: i64,
    pub url: String,
}

// ---- Drive DTOs ----

#[derive(Deserialize, Debug)]
pub struct DriveFileData {
    pub uuid: String,
    #[serde(default)]
    pub bucket: String,
    #[serde(rename = "fileId", default)]
    pub file_id: Option<String>,
    #[serde(default)]
    pub size: SizeField,
    #[serde(rename = "plainName", default)]
    pub plain_name: Option<String>,
    #[serde(default)]
    pub name: Option<String>,
    #[serde(rename = "type", default)]
    pub file_type: Option<String>,
}

/// Size comes back as a number or a numeric string depending on endpoint.
#[derive(Debug, Default)]
pub struct SizeField(pub u64);

impl<'de> Deserialize<'de> for SizeField {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        use serde::de::Error;
        let v = serde_json::Value::deserialize(d)?;
        let n = match v {
            serde_json::Value::Number(n) => n.as_u64().unwrap_or(0),
            serde_json::Value::String(s) => s.parse().map_err(D::Error::custom)?,
            serde_json::Value::Null => 0,
            _ => return Err(D::Error::custom("invalid size")),
        };
        Ok(SizeField(n))
    }
}
