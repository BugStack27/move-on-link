use crate::model::{Issue, LockedFile, PreflightReport};
use crate::pathutil::{self, display_path, extended, is_reparse};
use crate::protect;
use fs2::available_space;
use serde::Serialize;
use std::fs::{self, OpenOptions};
use std::io;
use std::os::windows::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

const FREE_SPACE_MARGIN: u64 = 1024 * 1024 * 1024;
const MAX_LOCKED_SAMPLES: usize = 50;

#[derive(Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ScanProgress {
    pub files: u64,
    pub bytes: u64,
    pub current_file: String,
}

struct ScanStats {
    files: u64,
    dirs: u64,
    bytes: u64,
    locked: Vec<LockedFile>,
    locked_count: u64,
    errors: Vec<Issue>,
}

fn issue(code: &str, message: &str, detail: Option<String>) -> Issue {
    Issue {
        code: code.to_string(),
        message: message.to_string(),
        detail,
    }
}

const PROBE_PREFIXES: [&str; 3] = [
    ".moveonlink_junction_probe_",
    ".moveonlink_symlink_probe_",
    ".moveonlink_write_probe_",
];

fn remove_file_with_retry(path: &Path) -> io::Result<()> {
    let mut last = None;
    for _ in 0..5 {
        match fs::remove_file(extended(path)) {
            Ok(()) => return Ok(()),
            Err(err) => {
                if err.kind() == io::ErrorKind::NotFound {
                    return Ok(());
                }
                last = Some(err);
                std::thread::sleep(Duration::from_millis(50));
            }
        }
    }
    Err(last.unwrap_or_else(|| io::Error::new(io::ErrorKind::Other, "删除探测文件失败")))
}

fn clear_probe_path(path: &Path) -> Result<(), String> {
    let meta = match fs::symlink_metadata(extended(path)) {
        Ok(meta) => meta,
        Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(err) => return Err(format!("读取探测路径失败：{err}")),
    };
    if is_reparse(&meta) {
        pathutil::delete_link_dir(path).map_err(|err| format!("删除探测链接失败：{err}"))
    } else if meta.is_file() {
        remove_file_with_retry(path).map_err(|err| format!("删除探测文件失败：{err}"))
    } else if meta.is_dir() {
        pathutil::remove_empty_dir_with_retry(path)
            .map_err(|err| format!("删除探测目录失败：{err}"))
    } else {
        Ok(())
    }
}

fn cleanup_stale_probes(parent: &Path) {
    let entries = match fs::read_dir(extended(parent)) {
        Ok(entries) => entries,
        Err(_) => return,
    };
    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().to_string();
        if !PROBE_PREFIXES.iter().any(|prefix| name.starts_with(prefix)) {
            continue;
        }
        let path = parent.join(entry.file_name());
        let meta = match fs::symlink_metadata(extended(&path)) {
            Ok(meta) => meta,
            Err(_) => continue,
        };
        if is_reparse(&meta) {
            let _ = pathutil::delete_link_dir(&path);
        } else if meta.is_file() {
            let _ = remove_file_with_retry(&path);
        } else if meta.is_dir() {
            let _ = pathutil::remove_empty_dir_with_retry(&path);
        }
    }
}

fn probe_write(parent: &Path) -> Result<(), String> {
    let probe = parent.join(format!(
        ".moveonlink_write_probe_{}_{}",
        std::process::id(),
        pathutil::stamp()
    ));
    let _ = remove_file_with_retry(&probe);
    match fs::write(extended(&probe), b"moveonlink") {
        Ok(()) => match remove_file_with_retry(&probe) {
            Ok(()) => Ok(()),
            Err(err) => Err(format!(
                "目标目录可写，但无法清理探测文件（{}）：{err}。请手动删除后重试",
                display_path(&probe)
            )),
        },
        Err(err) => {
            let _ = remove_file_with_retry(&probe);
            Err(format!("目标目录不可写：{err}"))
        }
    }
}

fn probe_junction(parent: &Path) -> Result<(), String> {
    let probe = parent.join(format!(
        ".moveonlink_junction_probe_{}_{}",
        std::process::id(),
        pathutil::stamp()
    ));
    let _ = clear_probe_path(&probe);
    match junction::create(parent, &probe) {
        Ok(()) => clear_probe_path(&probe).map_err(|err| {
            format!(
                "Junction 探测成功，但无法清理探测目录（{}）：{err}。请手动删除该目录后重试",
                display_path(&probe)
            )
        }),
        Err(err) => {
            let _ = clear_probe_path(&probe);
            Err(format!(
                "目标位置不支持 Junction（需要本地 NTFS 卷）：{err}"
            ))
        }
    }
}

fn probe_symlink(parent: &Path) -> Result<(), String> {
    let probe = parent.join(format!(
        ".moveonlink_symlink_probe_{}_{}",
        std::process::id(),
        pathutil::stamp()
    ));
    let _ = clear_probe_path(&probe);
    match std::os::windows::fs::symlink_dir(parent, &probe) {
        Ok(()) => clear_probe_path(&probe).map_err(|err| {
            format!(
                "符号链接探测成功，但无法清理探测目录（{}）：{err}。请手动删除该目录后重试",
                display_path(&probe)
            )
        }),
        Err(err) => {
            let _ = clear_probe_path(&probe);
            Err(format!(
                "当前权限无法创建目录符号链接（需要管理员权限或开发者模式）：{err}"
            ))
        }
    }
}

fn probe_lock(path: &Path) -> Option<String> {
    match OpenOptions::new()
        .read(true)
        .share_mode(0)
        .open(extended(path))
    {
        Ok(file) => {
            drop(file);
            None
        }
        Err(err) => Some(pathutil::friendly_io_error(&err)),
    }
}

fn scan_tree(
    root: &Path,
    cancel: &AtomicBool,
    on_progress: &dyn Fn(ScanProgress),
) -> ScanStats {
    let mut stats = ScanStats {
        files: 0,
        dirs: 0,
        bytes: 0,
        locked: Vec::new(),
        locked_count: 0,
        errors: Vec::new(),
    };
    let mut stack: Vec<PathBuf> = vec![root.to_path_buf()];
    let mut last_emit = Instant::now();

    'outer: while let Some(dir) = stack.pop() {
        if cancel.load(Ordering::SeqCst) {
            stats.errors.push(issue(
                "cancelled",
                "预检已取消",
                Some(display_path(&dir)),
            ));
            break;
        }

        let entries = match fs::read_dir(extended(&dir)) {
            Ok(entries) => entries,
            Err(err) => {
                stats.errors.push(issue(
                    "read_dir_failed",
                    "无法读取源目录，无法保证完整复制",
                    Some(format!("{}：{}", display_path(&dir), err)),
                ));
                continue;
            }
        };

        for entry in entries {
            if cancel.load(Ordering::SeqCst) {
                stats.errors.push(issue(
                    "cancelled",
                    "预检已取消",
                    Some(display_path(&dir)),
                ));
                break 'outer;
            }

            let entry = match entry {
                Ok(entry) => entry,
                Err(err) => {
                    stats.errors.push(issue(
                        "read_entry_failed",
                        "枚举源目录时出错，无法保证完整复制",
                        Some(format!("{}：{}", display_path(&dir), err)),
                    ));
                    continue;
                }
            };

            let path = dir.join(entry.file_name());
            let meta = match fs::symlink_metadata(extended(&path)) {
                Ok(meta) => meta,
                Err(err) => {
                    stats.errors.push(issue(
                        "metadata_failed",
                        "无法读取文件信息，无法保证完整复制",
                        Some(format!("{}：{}", display_path(&path), err)),
                    ));
                    continue;
                }
            };

            if is_reparse(&meta) {
                stats.errors.push(issue(
                    "nested_link",
                    "源目录包含链接或重解析点，为避免误操作已阻止迁移",
                    Some(display_path(&path)),
                ));
                continue;
            }

            if meta.is_dir() {
                stats.dirs += 1;
                stack.push(path.clone());
            } else if meta.is_file() {
                stats.files += 1;
                stats.bytes = stats.bytes.saturating_add(meta.len());
                if let Some(reason) = probe_lock(&path) {
                    stats.locked_count += 1;
                    if stats.locked.len() < MAX_LOCKED_SAMPLES {
                        stats.locked.push(LockedFile {
                            path: display_path(&path),
                            reason,
                        });
                    }
                }
            }

            if last_emit.elapsed() >= Duration::from_millis(120) {
                last_emit = Instant::now();
                on_progress(ScanProgress {
                    files: stats.files,
                    bytes: stats.bytes,
                    current_file: display_path(&path),
                });
            }
        }
    }

    on_progress(ScanProgress {
        files: stats.files,
        bytes: stats.bytes,
        current_file: String::new(),
    });
    stats
}

pub fn preflight(
    source: &Path,
    target: &Path,
    link_type: &str,
    cancel: &AtomicBool,
    on_progress: &dyn Fn(ScanProgress),
) -> Result<PreflightReport, String> {
    let elevated = pathutil::is_elevated();
    let mut blocking: Vec<Issue> = Vec::new();
    let mut warnings: Vec<Issue> = Vec::new();
    let source_display = display_path(source);
    let target_display = display_path(target);

    let source_meta = fs::symlink_metadata(extended(source));
    let mut source_canonical: Option<PathBuf> = None;
    let mut scan_allowed = false;

    match &source_meta {
        Ok(meta) => {
            if is_reparse(meta) {
                blocking.push(issue(
                    "source_is_link",
                    "源目录本身是链接或重解析点，拒绝再次迁移",
                    Some(source_display.clone()),
                ));
            } else if !meta.is_dir() {
                blocking.push(issue(
                    "source_not_dir",
                    "源路径不是文件夹",
                    Some(source_display.clone()),
                ));
            } else if source.parent().is_none() {
                blocking.push(issue(
                    "source_is_root",
                    "不能迁移磁盘根目录",
                    Some(source_display.clone()),
                ));
            } else if let Some(hit) = protect::check(source) {
                blocking.push(issue(
                    "source_protected",
                    &format!("该目录受系统保护，不允许迁移：{}", hit.reason),
                    Some(format!("{}（保护范围：{}）", source_display, hit.path)),
                ));
            } else {
                scan_allowed = true;
                source_canonical = pathutil::canonicalize_existing(source).ok();
            }
        }
        Err(err) => {
            blocking.push(issue(
                "source_missing",
                "源目录不存在或无法访问",
                Some(format!("{}：{}", source_display, err)),
            ));
        }
    }

    if source_display.starts_with(r"\\") {
        blocking.push(issue(
            "source_unc",
            "源目录不能是网络共享路径",
            Some(source_display.clone()),
        ));
        scan_allowed = false;
    }

    match target.parent() {
        Some(parent) if parent.is_dir() => {}
        _ => blocking.push(issue(
            "target_parent_missing",
            "目标位置的父目录不存在",
            Some(target_display.clone()),
        )),
    }

    if fs::symlink_metadata(extended(target)).is_ok() {
        blocking.push(issue(
            "target_exists",
            "目标位置已经存在，请选择一个不存在的目录名",
            Some(target_display.clone()),
        ));
    }

    if let Some(name_issue) = pathutil::invalid_final_name(target) {
        blocking.push(issue("target_name", name_issue, Some(target_display.clone())));
    }

    // 目标父目录本身可以是盘符根（如 D:\），只拦截落在受保护目录内部的情况。
    if let Some(parent) = target.parent() {
        if parent.parent().is_some() {
            if let Some(hit) = protect::check(parent) {
                blocking.push(issue(
                    "target_protected",
                    &format!("目标位置位于系统保护目录内，禁止使用：{}", hit.reason),
                    Some(format!("{}（保护范围：{}）", target_display, hit.path)),
                ));
            }
        }
    }

    if target_display.starts_with(r"\\") {
        blocking.push(issue(
            "target_unc",
            "目标目录不能是网络共享路径（Junction 仅支持本地 NTFS 卷）",
            Some(target_display.clone()),
        ));
    }

    if let (Some(src_canonical), Ok(dst_resolved)) =
        (&source_canonical, pathutil::resolve_target(target))
    {
        let src_norm = pathutil::normalize(src_canonical);
        let dst_norm = pathutil::normalize(&dst_resolved);
        if pathutil::is_within(&dst_norm, &src_norm) {
            blocking.push(issue(
                "target_inside_source",
                "目标目录不能位于源目录内部",
                Some(dst_resolved.display().to_string()),
            ));
        }
        if pathutil::is_within(&src_norm, &dst_norm) {
            blocking.push(issue(
                "source_inside_target",
                "源目录不能位于目标目录内部",
                Some(dst_resolved.display().to_string()),
            ));
        }
    }

    if let Some(parent) = target.parent() {
        if parent.is_dir() && !target_display.starts_with(r"\\") {
            cleanup_stale_probes(parent);
        }
    }

    let base_path_ok = source_meta
        .as_ref()
        .map(|m| m.is_dir() && !is_reparse(m))
        .unwrap_or(false)
        && target.parent().map(|p| p.is_dir()).unwrap_or(false)
        && !target_display.starts_with(r"\\");

    if base_path_ok {
        if let Some(parent) = target.parent() {
            if let Err(err) = probe_write(parent) {
                blocking.push(issue("target_not_writable", &err, None));
            }
            if link_type == "symlink" {
                if let Err(err) = probe_symlink(parent) {
                    blocking.push(issue("symlink_unavailable", &err, None));
                }
            } else if let Err(err) = probe_junction(parent) {
                blocking.push(issue("junction_unavailable", &err, None));
            }
        }
    }

    if !elevated {
        warnings.push(issue(
            "not_elevated",
            "当前未以管理员身份运行，受保护目录可能无法读取或迁移",
            None,
        ));
    }

    if let (Some(src_canonical), Ok(dst_resolved)) =
        (&source_canonical, pathutil::resolve_target(target))
    {
        let same_drive = pathutil::normalize(src_canonical)
            .chars()
            .next()
            == pathutil::normalize(&dst_resolved).chars().next();
        if same_drive {
            warnings.push(issue(
                "same_drive",
                "源目录与目标目录位于同一磁盘，迁移不会释放该磁盘总空间",
                None,
            ));
        }
    }

    let mut file_count = 0u64;
    let mut dir_count = 0u64;
    let mut total_bytes = 0u64;
    let mut locked_files = Vec::new();
    let mut locked_count = 0u64;

    if scan_allowed {
        let stats = scan_tree(source, cancel, on_progress);
        file_count = stats.files;
        dir_count = stats.dirs;
        total_bytes = stats.bytes;
        locked_files = stats.locked;
        locked_count = stats.locked_count;
        blocking.extend(stats.errors);
    }

    if locked_count > 0 {
        blocking.push(issue(
            "files_in_use",
            &format!(
                "检测到 {} 个文件可能被占用或不可读，请关闭相关程序后重新预检",
                locked_count
            ),
            None,
        ));
    }

    let mut target_free_bytes = 0u64;
    if let Some(parent) = target.parent() {
        if let Ok(free) = available_space(extended(parent)) {
            target_free_bytes = free;
            if scan_allowed {
                let required = total_bytes.saturating_add(FREE_SPACE_MARGIN);
                if free < required {
                    blocking.push(issue(
                        "not_enough_space",
                        "目标盘剩余空间不足（需预留至少 1 GB 安全余量）",
                        Some(format!(
                            "需要约 {} 字节，可用 {} 字节",
                            required, free
                        )),
                    ));
                }
            }
        } else if scan_allowed {
            warnings.push(issue(
                "free_space_unknown",
                "无法读取目标盘剩余空间，请自行确认空间充足",
                None,
            ));
        }
    }

    let blocked = !blocking.is_empty();

    Ok(PreflightReport {
        source: source_display,
        target: target_display,
        link_type: link_type.to_string(),
        total_bytes,
        file_count,
        dir_count,
        target_free_bytes,
        blocked,
        blocking_issues: blocking,
        warnings,
        locked_files,
        locked_count,
        elevated,
    })
}
