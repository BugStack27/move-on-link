use crate::model::MigrationRecord;
use std::fs;
use std::path::{Path, PathBuf};

pub fn records_path(app_data_dir: &Path) -> PathBuf {
    app_data_dir.join("records.json")
}

pub fn load(app_data_dir: &Path) -> Vec<MigrationRecord> {
    let path = records_path(app_data_dir);
    match fs::read_to_string(&path) {
        Ok(content) => serde_json::from_str(&content).unwrap_or_default(),
        Err(_) => Vec::new(),
    }
}

pub fn save(app_data_dir: &Path, records: &[MigrationRecord]) -> Result<(), String> {
    fs::create_dir_all(app_data_dir)
        .map_err(|e| format!("创建记录目录失败：{e}"))?;
    let path = records_path(app_data_dir);
    let content = serde_json::to_string_pretty(records)
        .map_err(|e| format!("序列化迁移记录失败：{e}"))?;
    let tmp = app_data_dir.join("records.json.tmp");
    fs::write(&tmp, content).map_err(|e| format!("写入迁移记录失败：{e}"))?;
    // Windows 上 rename 不能覆盖已有文件，先删除旧文件再替换。
    if path.exists() {
        let _ = fs::remove_file(&path);
    }
    fs::rename(&tmp, &path).map_err(|e| format!("保存迁移记录失败：{e}"))?;
    Ok(())
}
