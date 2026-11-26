use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Issue {
    pub code: String,
    pub message: String,
    pub detail: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct LockedFile {
    pub path: String,
    pub reason: String,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PreflightReport {
    pub source: String,
    pub target: String,
    pub link_type: String,
    pub total_bytes: u64,
    pub file_count: u64,
    pub dir_count: u64,
    pub target_free_bytes: u64,
    pub blocked: bool,
    pub blocking_issues: Vec<Issue>,
    pub warnings: Vec<Issue>,
    pub locked_files: Vec<LockedFile>,
    pub locked_count: u64,
    pub elevated: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MigrationRecord {
    pub id: String,
    pub created_at: String,
    pub source: String,
    pub target: String,
    pub link_type: String,
    pub size_bytes: u64,
    pub file_count: u64,
    #[serde(default)]
    pub dir_count: u64,
    pub status: String,
    #[serde(default)]
    pub note: Option<String>,
    #[serde(default)]
    pub revoked_at: Option<String>,
    #[serde(default)]
    pub backup_path: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RecoveryPath {
    pub label: String,
    pub path: String,
    pub is_dir: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RecoveryNotice {
    pub kind: String,
    pub title: String,
    pub message: String,
    #[serde(default)]
    pub paths: Vec<RecoveryPath>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ProgressEvent {
    pub stage: String,
    pub phase_label: String,
    pub percent: f64,
    pub bytes_done: u64,
    pub bytes_total: u64,
    pub files_done: u64,
    pub files_total: u64,
    pub current_file: Option<String>,
    pub message: Option<String>,
}

pub const STATUS_COMPLETED: &str = "completed";
pub const STATUS_WARNING: &str = "completed_with_warning";
pub const STATUS_REVOKED: &str = "revoked";

pub const NOTICE_INFO: &str = "info";
pub const NOTICE_WARNING: &str = "warning";
