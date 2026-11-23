mod engine;
mod model;
mod pathutil;
mod protect;
mod records;
mod scan;

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use tauri::{AppHandle, Emitter, Manager, State};

#[derive(Default)]
struct AppState {
    cancel: Arc<AtomicBool>,
    busy: Arc<AtomicBool>,
    recovery: Arc<std::sync::Mutex<Option<String>>>,
}

struct BusyGuard(Arc<AtomicBool>);

impl Drop for BusyGuard {
    fn drop(&mut self) {
        self.0.store(false, Ordering::SeqCst);
    }
}

fn acquire_busy(busy: &Arc<AtomicBool>) -> Result<BusyGuard, String> {
    if busy.swap(true, Ordering::SeqCst) {
        return Err("已有任务正在进行，请稍候".to_string());
    }
    Ok(BusyGuard(busy.clone()))
}

fn data_dir(app: &AppHandle) -> Result<PathBuf, String> {
    let dir = app
        .path()
        .app_data_dir()
        .map_err(|err| format!("无法获取应用数据目录：{err}"))?;
    std::fs::create_dir_all(&dir).map_err(|err| format!("创建应用数据目录失败：{err}"))?;
    Ok(dir)
}

fn join_note(existing: Option<String>, extra: String) -> String {
    match existing {
        Some(note) => format!("{note}；{extra}"),
        None => extra,
    }
}

#[tauri::command]
fn is_elevated() -> bool {
    pathutil::is_elevated()
}

#[tauri::command]
fn path_kind(path: String) -> &'static str {
    let path = Path::new(&path);
    if path.is_dir() {
        "dir"
    } else if path.is_file() {
        "file"
    } else {
        "missing"
    }
}

#[tauri::command]
fn check_protected_dir(path: String) -> Option<protect::ProtectedDir> {
    protect::check(Path::new(&path))
}

#[derive(serde::Serialize)]
#[serde(rename_all = "camelCase")]
struct DiskInfo {
    free_bytes: u64,
    total_bytes: u64,
}

fn drive_root(path: &str) -> Option<String> {
    let bytes = path.as_bytes();
    if bytes.len() >= 2 && bytes[1] == b':' {
        Some(format!("{}:\\", (bytes[0] as char).to_ascii_uppercase()))
    } else {
        None
    }
}

#[tauri::command]
fn disk_info(path: String) -> Result<DiskInfo, String> {
    let root = drive_root(&path)
        .ok_or_else(|| "无法识别磁盘盘符（仅支持本地盘）".to_string())?;

    use std::os::windows::ffi::OsStrExt;
    let wide: Vec<u16> = std::ffi::OsStr::new(&root)
        .encode_wide()
        .chain(std::iter::once(0))
        .collect();
    let mut free = 0u64;
    let mut total = 0u64;
    let mut total_free = 0u64;
    let ok = unsafe {
        windows_sys::Win32::Storage::FileSystem::GetDiskFreeSpaceExW(
            wide.as_ptr(),
            &mut free,
            &mut total,
            &mut total_free,
        )
    };
    if ok == 0 {
        return Err(format!(
            "读取磁盘信息失败：{}",
            std::io::Error::last_os_error()
        ));
    }
    Ok(DiskInfo {
        free_bytes: total_free,
        total_bytes: total,
    })
}

#[tauri::command]
async fn preflight(
    app: AppHandle,
    state: State<'_, AppState>,
    source: String,
    target: String,
    link_type: String,
) -> Result<model::PreflightReport, String> {
    let busy = state.busy.clone();
    let cancel = state.cancel.clone();
    let _guard = acquire_busy(&busy)?;
    cancel.store(false, Ordering::SeqCst);

    let app_handle = app.clone();
    let source_path = PathBuf::from(source);
    let target_path = PathBuf::from(target);

    tauri::async_runtime::spawn_blocking(move || {
        let emitter = app_handle.clone();
        let on_progress = move |payload: scan::ScanProgress| {
            let _ = emitter.emit("moveonlink://scan", payload);
        };
        scan::preflight(
            &source_path,
            &target_path,
            &link_type,
            &cancel,
            &on_progress,
        )
    })
    .await
    .map_err(|err| format!("预检线程失败：{err}"))?
}

#[tauri::command]
async fn start_migration(
    app: AppHandle,
    state: State<'_, AppState>,
    source: String,
    target: String,
    link_type: String,
    expected_bytes: u64,
    expected_files: u64,
    expected_dirs: u64,
) -> Result<model::MigrationRecord, String> {
    let busy = state.busy.clone();
    let cancel = state.cancel.clone();
    let _guard = acquire_busy(&busy)?;
    cancel.store(false, Ordering::SeqCst);

    let link = engine::LinkType::parse(&link_type)?;
    if let Some(hit) = protect::check(Path::new(&source)) {
        return Err(format!(
            "「{}」是系统保护目录（{}），不允许迁移",
            hit.path, hit.reason
        ));
    }
    let display_source = source.clone();
    let display_target = target.clone();
    let dir = data_dir(&app)?;
    let options = engine::MigrationOptions {
        source: PathBuf::from(&source),
        target: PathBuf::from(&target),
        link_type: link,
        expected_bytes,
        expected_files,
        expected_dirs,
        journal_path: Some(dir.join("migration-journal.json")),
    };

    let app_handle = app.clone();
    let outcome = tauri::async_runtime::spawn_blocking(move || {
        let emitter = app_handle.clone();
        let on_progress = move |payload: model::ProgressEvent| {
            let _ = emitter.emit("moveonlink://progress", payload);
        };
        engine::migrate(&options, &cancel, &on_progress)
    })
    .await
    .map_err(|err| format!("迁移线程失败：{err}"))??;

    let mut record = model::MigrationRecord {
        id: format!(
            "{}-{}",
            chrono::Local::now().timestamp_millis(),
            std::process::id()
        ),
        created_at: pathutil::now_display(),
        source: pathutil::display_path(Path::new(&display_source)),
        target: pathutil::display_path(Path::new(&display_target)),
        link_type: outcome.link_type.as_str().to_string(),
        size_bytes: outcome.size_bytes,
        file_count: outcome.file_count,
        dir_count: outcome.dir_count,
        status: if outcome.warning.is_some() {
            model::STATUS_WARNING.to_string()
        } else {
            model::STATUS_COMPLETED.to_string()
        },
        note: outcome.warning.clone(),
        revoked_at: None,
        backup_path: outcome.backup_path.clone(),
    };

    match data_dir(&app) {
        Ok(dir) => {
            let mut list = records::load(&dir);
            list.insert(0, record.clone());
            if let Err(err) = records::save(&dir, &list) {
                record.note = Some(join_note(
                    record.note.clone(),
                    format!("迁移记录保存失败：{err}"),
                ));
                record.status = model::STATUS_WARNING.to_string();
            }
        }
        Err(err) => {
            record.note = Some(join_note(record.note.clone(), err));
            record.status = model::STATUS_WARNING.to_string();
        }
    }

    Ok(record)
}

#[tauri::command]
fn cancel_task(state: State<'_, AppState>) {
    state.cancel.store(true, Ordering::SeqCst);
}

#[tauri::command]
fn list_records(app: AppHandle) -> Result<Vec<model::MigrationRecord>, String> {
    let dir = data_dir(&app)?;
    Ok(records::load(&dir))
}

#[tauri::command]
async fn undo_migration(
    app: AppHandle,
    state: State<'_, AppState>,
    id: String,
) -> Result<model::MigrationRecord, String> {
    let busy = state.busy.clone();
    let cancel = state.cancel.clone();
    let _guard = acquire_busy(&busy)?;
    cancel.store(false, Ordering::SeqCst);

    let dir = data_dir(&app)?;
    let mut list = records::load(&dir);
    let index = list
        .iter()
        .position(|record| record.id == id)
        .ok_or_else(|| "找不到迁移记录".to_string())?;
    let record = list[index].clone();

    if record.status == model::STATUS_REVOKED {
        return Err("该记录已经撤销".to_string());
    }

    let link = engine::LinkType::parse(&record.link_type)?;
    let source = PathBuf::from(&record.source);
    let target = PathBuf::from(&record.target);
    let app_handle = app.clone();

    let outcome = tauri::async_runtime::spawn_blocking(move || {
        let emitter = app_handle.clone();
        let on_progress = move |payload: model::ProgressEvent| {
            let _ = emitter.emit("moveonlink://progress", payload);
        };
        engine::undo(&source, &target, link, &cancel, &on_progress)
    })
    .await
    .map_err(|err| format!("撤销线程失败：{err}"))??;

    let mut updated = record;
    updated.status = model::STATUS_REVOKED.to_string();
    updated.revoked_at = Some(pathutil::now_iso());
    if let Some(warning) = outcome.warning {
        updated.note = Some(join_note(updated.note.clone(), warning));
    }

    list[index] = updated.clone();
    if let Err(err) = records::save(&dir, &list) {
        updated.note = Some(join_note(
            updated.note.clone(),
            format!("撤销记录保存失败：{err}"),
        ));
    }

    Ok(updated)
}

#[tauri::command]
fn take_recovery_notice(state: State<'_, AppState>) -> Option<String> {
    state
        .recovery
        .lock()
        .ok()
        .and_then(|mut slot| slot.take())
}

pub fn run() {
    tauri::Builder::default()
        .manage(AppState::default())
        .plugin(tauri_plugin_dialog::init())
        .plugin(tauri_plugin_opener::init())
        .setup(|app| {
            if let Ok(dir) = app.path().app_data_dir() {
                let _ = std::fs::create_dir_all(&dir);
                let notice = match engine::recover_interrupted(
                    &dir.join("migration-journal.json"),
                ) {
                    Ok(notice) => notice,
                    Err(err) => Some(format!("上次迁移的恢复检查失败：{err}")),
                };
                if let Some(notice) = notice {
                    if let Ok(mut slot) = app.state::<AppState>().recovery.lock() {
                        *slot = Some(notice);
                    }
                }
            }
            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            is_elevated,
            path_kind,
            check_protected_dir,
            disk_info,
            preflight,
            start_migration,
            cancel_task,
            list_records,
            undo_migration,
            take_recovery_notice
        ])
        .run(tauri::generate_context!())
        .expect("error while running MoveOnLink");
}
