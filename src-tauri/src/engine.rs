use crate::model::{
    ProgressEvent, RecoveryNotice, RecoveryPath, NOTICE_INFO, NOTICE_WARNING,
};
use crate::pathutil::{self, display_path, extended, friendly_io_error, is_reparse};
use fs2::available_space;
use sha2::{Digest, Sha256};
use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Write};
use std::os::windows::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

const FREE_SPACE_MARGIN: u64 = 1024 * 1024 * 1024;
const PROGRESS_INTERVAL: Duration = Duration::from_millis(120);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LinkType {
    Junction,
    Symlink,
}

impl LinkType {
    pub fn parse(value: &str) -> Result<Self, String> {
        match value {
            "junction" | "Junction" | "junction /J" => Ok(LinkType::Junction),
            "symlink" | "Symlink" | "symlink /D" => Ok(LinkType::Symlink),
            other => Err(format!("不支持的链接方式：{other}")),
        }
    }

    pub fn as_str(&self) -> &'static str {
        match self {
            LinkType::Junction => "junction",
            LinkType::Symlink => "symlink",
        }
    }
}

pub struct MigrationOptions {
    pub source: PathBuf,
    pub target: PathBuf,
    pub link_type: LinkType,
    pub expected_bytes: u64,
    pub expected_files: u64,
    pub expected_dirs: u64,
    pub journal_path: Option<PathBuf>,
}

#[derive(serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
struct SwapJournal {
    source: String,
    target: String,
    backup: String,
    link_type: String,
}

fn write_journal(path: &Path, journal: &SwapJournal) -> Result<(), String> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(extended(parent))
            .map_err(|err| format!("创建安全日志目录失败：{err}"))?;
    }
    let content = serde_json::to_string_pretty(journal)
        .map_err(|err| format!("序列化安全日志失败：{err}"))?;
    let tmp = path.with_extension("tmp");
    fs::write(extended(&tmp), content).map_err(|err| format!("写入安全日志失败：{err}"))?;
    if path.exists() {
        let _ = fs::remove_file(extended(path));
    }
    fs::rename(extended(&tmp), extended(path)).map_err(|err| format!("保存安全日志失败：{err}"))
}

fn clear_journal(options: &MigrationOptions) {
    if let Some(path) = &options.journal_path {
        let _ = fs::remove_file(extended(path));
    }
}

fn recovery_path(label: &str, path: &Path, is_dir: bool) -> RecoveryPath {
    RecoveryPath {
        label: label.to_string(),
        path: display_path(path),
        is_dir,
    }
}

pub fn recover_interrupted(journal_path: &Path) -> Result<Option<RecoveryNotice>, String> {
    if !journal_path.exists() {
        return Ok(None);
    }
    let raw = fs::read_to_string(extended(journal_path))
        .map_err(|err| format!("读取恢复日志失败：{err}"))?;
    let journal: SwapJournal = serde_json::from_str(&raw)
        .map_err(|err| format!("解析恢复日志失败：{err}"))?;
    let source = PathBuf::from(&journal.source);
    let target = PathBuf::from(&journal.target);
    let backup = PathBuf::from(&journal.backup);

    let source_meta = fs::symlink_metadata(extended(&source)).ok();
    if let Some(meta) = &source_meta {
        if is_reparse(meta) {
            let _ = fs::remove_file(extended(journal_path));
            return Ok(Some(RecoveryNotice {
                kind: NOTICE_INFO.to_string(),
                title: "上次迁移已完成".to_string(),
                message: "原路径已由链接接管，恢复日志已自动清理，无需处理。".to_string(),
                paths: Vec::new(),
            }));
        }
        if fs::symlink_metadata(extended(&backup)).is_err() {
            let _ = fs::remove_file(extended(journal_path));
            return Ok(Some(RecoveryNotice {
                kind: NOTICE_INFO.to_string(),
                title: "上次迁移在替换原路径前中止".to_string(),
                message: "原目录未受影响，恢复日志已清理。目标位置可能残留未接管的副本，确认无误后可手动删除。"
                    .to_string(),
                paths: vec![recovery_path("目标副本", &target, true)],
            }));
        }
    }

    if source_meta.is_none() {
        let backup_meta = fs::symlink_metadata(extended(&backup)).ok();
        if let Some(meta) = backup_meta {
            if !is_reparse(&meta) && meta.is_dir() {
                fs::rename(extended(&backup), extended(&source)).map_err(|err| {
                    format!(
                        "自动恢复原目录失败：{err}。备份保留在 {}",
                        display_path(&backup)
                    )
                })?;
                let _ = fs::remove_file(extended(journal_path));
                return Ok(Some(RecoveryNotice {
                    kind: NOTICE_WARNING.to_string(),
                    title: "已将中断迁移的备份恢复到原路径".to_string(),
                    message: "上次迁移中断，原目录已从备份恢复，文件可正常访问。目标副本仍保留，确认数据无误后可手动清理或重新迁移。"
                        .to_string(),
                    paths: vec![
                        recovery_path("已恢复的原目录", &source, true),
                        recovery_path("目标副本", &target, true),
                    ],
                }));
            }
        }
    }

    let unresolved = journal_path.with_extension("unresolved.json");
    let _ = fs::rename(extended(journal_path), extended(&unresolved));
    Ok(Some(RecoveryNotice {
        kind: NOTICE_WARNING.to_string(),
        title: "检测到未完成的迁移状态".to_string(),
        message: "现场已保留，请按下列路径手动检查；确认处理完成后可删除待检查日志。".to_string(),
        paths: vec![
            recovery_path("待检查日志", &unresolved, false),
            recovery_path("原目录", &source, true),
            recovery_path("备份目录", &backup, true),
            recovery_path("目标副本", &target, true),
        ],
    }))
}

#[derive(Debug)]
pub struct MigrationOutcome {
    pub size_bytes: u64,
    pub file_count: u64,
    pub dir_count: u64,
    pub link_type: LinkType,
    pub warning: Option<String>,
    pub backup_path: Option<String>,
}

#[derive(Debug)]
pub struct UndoOutcome {
    pub warning: Option<String>,
}

struct CopyStats {
    files: u64,
    dirs: u64,
    bytes: u64,
    digests: Vec<(PathBuf, [u8; 32])>,
}

fn cancelled(cancel: &AtomicBool) -> Result<(), String> {
    if cancel.load(Ordering::SeqCst) {
        Err("任务已取消，源文件未做任何改动".to_string())
    } else {
        Ok(())
    }
}

fn make_event(
    stage: &str,
    label: &str,
    percent: f64,
    bytes_done: u64,
    bytes_total: u64,
    files_done: u64,
    files_total: u64,
    current_file: Option<String>,
    message: Option<String>,
) -> ProgressEvent {
    ProgressEvent {
        stage: stage.to_string(),
        phase_label: label.to_string(),
        percent: percent.clamp(0.0, 100.0),
        bytes_done,
        bytes_total,
        files_done,
        files_total,
        current_file,
        message,
    }
}

fn remove_tree_safe(path: &Path) -> io::Result<()> {
    let meta = fs::symlink_metadata(extended(path))?;
    if is_reparse(&meta) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "拒绝递归删除重解析点",
        ));
    }
    fs::remove_dir_all(extended(path))
}

fn restore_from_backup(source: &Path, backup: &Path) -> bool {
    if let Ok(meta) = fs::symlink_metadata(extended(source)) {
        if is_reparse(&meta) {
            let _ = remove_link(source);
        }
    }
    fs::rename(extended(backup), extended(source)).is_ok()
}

fn create_link(target: &Path, link: &Path, kind: LinkType) -> Result<(), String> {
    match kind {
        LinkType::Junction => junction::create(target, link)
            .map_err(|err| format!("创建 Junction 失败：{err}")),
        LinkType::Symlink => std::os::windows::fs::symlink_dir(target, link)
            .map_err(|err| format!("创建符号链接失败：{err}")),
    }
}

fn remove_link(link: &Path) -> Result<(), String> {
    pathutil::delete_link_dir(link).map_err(|err| format!("删除链接失败：{err}"))
}

fn link_target(link: &Path, kind: LinkType) -> Result<PathBuf, String> {
    match kind {
        LinkType::Junction => {
            junction::get_target(link).map_err(|err| format!("读取 Junction 目标失败：{err}"))
        }
        LinkType::Symlink => {
            fs::read_link(extended(link)).map_err(|err| format!("读取符号链接目标失败：{err}"))
        }
    }
}

fn verify_link(link: &Path, target: &Path, kind: LinkType) -> Result<(), String> {
    let stored = link_target(link, kind)?;
    let stored = pathutil::canonicalize_existing(&stored).unwrap_or(stored);
    let target_canonical = pathutil::canonicalize_existing(target)
        .map_err(|err| format!("无法访问目标目录：{err}"))?;
    if pathutil::normalize(&stored) != pathutil::normalize(&target_canonical) {
        return Err(format!(
            "链接指向 {}，与目标 {} 不一致",
            display_path(&stored),
            display_path(&target_canonical)
        ));
    }
    let meta = fs::metadata(extended(link))
        .map_err(|err| format!("无法通过链接访问目标：{err}"))?;
    if !meta.is_dir() {
        return Err("链接没有解析为目录".to_string());
    }
    fs::read_dir(extended(link))
        .map_err(|err| format!("无法读取链接目录：{err}"))?;
    Ok(())
}

fn sha256_file(path: &Path) -> Result<Vec<u8>, String> {
    let mut file = File::open(extended(path))
        .map_err(|err| format!("读取校验文件失败（{}）：{err}", display_path(path)))?;
    let mut hasher = Sha256::new();
    io::copy(&mut file, &mut hasher)
        .map_err(|err| format!("计算校验值失败（{}）：{err}", display_path(path)))?;
    Ok(hasher.finalize().to_vec())
}

fn verify_digests(
    digests: &[(PathBuf, [u8; 32])],
    total_bytes: u64,
    cancel: &AtomicBool,
    progress: &dyn Fn(ProgressEvent),
) -> Result<(), String> {
    let mut verified_files = 0u64;
    let mut verified_bytes = 0u64;
    let mut last_emit = Instant::now();

    for (path, expected) in digests {
        cancelled(cancel)?;
        let actual = sha256_file(path)?;
        if actual.as_slice() != &expected[..] {
            return Err(format!("内容校验失败：{}", display_path(path)));
        }
        let size = fs::metadata(extended(path)).map(|m| m.len()).unwrap_or(0);
        verified_bytes = verified_bytes.saturating_add(size);
        verified_files += 1;

        if last_emit.elapsed() >= PROGRESS_INTERVAL {
            last_emit = Instant::now();
            let percent = if total_bytes > 0 {
                99.0 + (verified_bytes as f64 / total_bytes as f64) * 0.9
            } else {
                99.5
            };
            progress(make_event(
                "verifying",
                "逐文件校验",
                percent,
                verified_bytes,
                total_bytes,
                verified_files,
                digests.len() as u64,
                Some(display_path(path)),
                Some("正在逐文件核对 SHA-256".to_string()),
            ));
        }
    }
    Ok(())
}

fn tree_totals(root: &Path) -> Result<(u64, u64, u64), String> {
    let mut files = 0u64;
    let mut dirs = 0u64;
    let mut bytes = 0u64;
    let mut stack = vec![root.to_path_buf()];

    while let Some(dir) = stack.pop() {
        let entries = fs::read_dir(extended(&dir))
            .map_err(|err| format!("读取目录失败（{}）：{err}", display_path(&dir)))?;
        for entry in entries {
            let entry =
                entry.map_err(|err| format!("枚举目录失败（{}）：{err}", display_path(&dir)))?;
            let path = dir.join(entry.file_name());
            let meta = fs::symlink_metadata(extended(&path))
                .map_err(|err| format!("读取文件信息失败（{}）：{err}", display_path(&path)))?;
            if is_reparse(&meta) {
                return Err(format!(
                    "发现链接或重解析点，已中止：{}",
                    display_path(&path)
                ));
            }
            if meta.is_dir() {
                dirs += 1;
                stack.push(path);
            } else if meta.is_file() {
                files += 1;
                bytes = bytes.saturating_add(meta.len());
            } else {
                return Err(format!(
                    "发现不支持的文件类型，已中止：{}",
                    display_path(&path)
                ));
            }
        }
    }
    Ok((files, dirs, bytes))
}

fn copy_tree(
    source_root: &Path,
    destination_root: &Path,
    expected_files: u64,
    _expected_dirs: u64,
    expected_bytes: u64,
    cancel: &AtomicBool,
    progress: &dyn Fn(ProgressEvent),
) -> Result<CopyStats, String> {
    let mut stats = CopyStats {
        files: 0,
        dirs: 0,
        bytes: 0,
        digests: Vec::new(),
    };
    let mut stack: Vec<PathBuf> = vec![source_root.to_path_buf()];
    let mut last_emit = Instant::now();
    let mut buffer = vec![0u8; 1024 * 1024];

    while let Some(dir) = stack.pop() {
        cancelled(cancel)?;
        let entries = fs::read_dir(extended(&dir))
            .map_err(|err| format!("读取源目录失败（{}）：{err}", display_path(&dir)))?;

        for entry in entries {
            cancelled(cancel)?;
            let entry =
                entry.map_err(|err| format!("枚举源目录失败（{}）：{err}", display_path(&dir)))?;
            let path = dir.join(entry.file_name());
            let meta = fs::symlink_metadata(extended(&path))
                .map_err(|err| format!("读取源文件信息失败（{}）：{err}", display_path(&path)))?;

            if is_reparse(&meta) {
                return Err(format!(
                    "源目录中出现了链接或重解析点，已中止：{}",
                    display_path(&path)
                ));
            }

            let relative = path
                .strip_prefix(source_root)
                .map_err(|_| format!("无法计算相对路径：{}", display_path(&path)))?;
            let destination = destination_root.join(relative);

            if meta.is_dir() {
                fs::create_dir(extended(&destination)).map_err(|err| {
                    format!("创建目标目录失败（{}）：{err}", display_path(&destination))
                })?;
                stats.dirs += 1;
                stack.push(path);
            } else if meta.is_file() {
                let mut input = OpenOptions::new()
                    .read(true)
                    .share_mode(0)
                    .open(extended(&path))
                    .map_err(|err| {
                        format!(
                            "无法独占打开文件，可能被其他程序占用（{}）：{}",
                            display_path(&path),
                            friendly_io_error(&err)
                        )
                    })?;
                let handle_meta = input
                    .metadata()
                    .map_err(|err| format!("读取文件信息失败（{}）：{err}", display_path(&path)))?;
                if !handle_meta.is_file() {
                    return Err(format!(
                        "文件类型发生变化，已中止：{}",
                        display_path(&path)
                    ));
                }
                let size = handle_meta.len();

                let mut output = OpenOptions::new()
                    .write(true)
                    .create_new(true)
                    .open(extended(&destination))
                    .map_err(|err| {
                        format!(
                            "创建目标文件失败（{}）：{err}",
                            display_path(&destination)
                        )
                    })?;
                let mut hasher = Sha256::new();
                let mut written = 0u64;
                loop {
                    cancelled(cancel)?;
                    let read = input
                        .read(&mut buffer)
                        .map_err(|err| format!("读取源文件失败（{}）：{err}", display_path(&path)))?;
                    if read == 0 {
                        break;
                    }
                    hasher.update(&buffer[..read]);
                    output.write_all(&buffer[..read]).map_err(|err| {
                        format!("写入目标文件失败（{}）：{err}", display_path(&destination))
                    })?;
                    written = written.saturating_add(read as u64);

                    if last_emit.elapsed() >= PROGRESS_INTERVAL {
                        last_emit = Instant::now();
                        let done = stats.bytes.saturating_add(written);
                        let percent = if expected_bytes > 0 {
                            done as f64 / expected_bytes as f64 * 100.0
                        } else {
                            0.0
                        };
                        progress(make_event(
                            "copying",
                            "复制文件",
                            percent.min(99.0),
                            done,
                            expected_bytes,
                            stats.files,
                            expected_files,
                            Some(display_path(&path)),
                            None,
                        ));
                    }
                }
                if written != size {
                    return Err(format!(
                        "文件复制长度不一致，已中止：{}",
                        display_path(&path)
                    ));
                }
                output
                    .flush()
                    .map_err(|err| format!("写入目标文件失败（{}）：{err}", display_path(&destination)))?;
                drop(output);

                let digest: [u8; 32] = hasher.finalize().into();
                stats.digests.push((destination.clone(), digest));
                stats.files += 1;
                stats.bytes = stats.bytes.saturating_add(size);

                if last_emit.elapsed() >= PROGRESS_INTERVAL {
                    last_emit = Instant::now();
                    let percent = if expected_bytes > 0 {
                        stats.bytes as f64 / expected_bytes as f64 * 100.0
                    } else {
                        0.0
                    };
                    progress(make_event(
                        "copying",
                        "复制文件",
                        percent.min(99.0),
                        stats.bytes,
                        expected_bytes,
                        stats.files,
                        expected_files,
                        Some(display_path(&path)),
                        None,
                    ));
                }
            } else {
                return Err(format!(
                    "发现不支持的文件类型，已中止：{}",
                    display_path(&path)
                ));
            }
        }
    }

    progress(make_event(
        "copying",
        "复制文件",
        99.0,
        stats.bytes,
        expected_bytes,
        stats.files,
        expected_files,
        None,
        Some("文件已复制完成，开始完整性校验".to_string()),
    ));
    Ok(stats)
}

pub fn migrate(
    options: &MigrationOptions,
    cancel: &AtomicBool,
    progress: &dyn Fn(ProgressEvent),
) -> Result<MigrationOutcome, String> {
    let source = pathutil::canonicalize_existing(&options.source)
        .map_err(|err| format!("源目录不可用：{err}"))?;
    let source_meta = fs::symlink_metadata(extended(&source))
        .map_err(|err| format!("源目录不可用：{err}"))?;
    if is_reparse(&source_meta) {
        return Err("源目录本身是链接，拒绝迁移".to_string());
    }
    if !source_meta.is_dir() {
        return Err("源路径不是文件夹".to_string());
    }
    if fs::symlink_metadata(extended(&options.target)).is_ok() {
        return Err("目标目录已经存在，拒绝覆盖".to_string());
    }

    let target = pathutil::resolve_target(&options.target)
        .map_err(|err| format!("目标路径不可用：{err}"))?;
    let parent = target
        .parent()
        .ok_or_else(|| "目标路径没有父目录".to_string())?;
    if !parent.is_dir() {
        return Err("目标位置的父目录不存在".to_string());
    }

    let source_norm = pathutil::normalize(&source);
    let target_norm = pathutil::normalize(&target);
    if pathutil::is_within(&target_norm, &source_norm)
        || pathutil::is_within(&source_norm, &target_norm)
    {
        return Err("源目录与目标目录不能互相嵌套".to_string());
    }

    if let Ok(free) = available_space(extended(parent)) {
        if free < options.expected_bytes.saturating_add(FREE_SPACE_MARGIN) {
            return Err("目标盘可用空间不足，已拒绝开始迁移".to_string());
        }
    }

    let stamp = pathutil::stamp();
    let staging = parent.join(format!(".moveonlink-staging-{stamp}"));
    if fs::symlink_metadata(extended(&staging)).is_ok() {
        return Err("暂存目录已存在，请稍后重试".to_string());
    }
    fs::create_dir(extended(&staging))
        .map_err(|err| format!("创建暂存目录失败：{err}"))?;

    progress(make_event(
        "preparing",
        "准备迁移",
        0.0,
        0,
        options.expected_bytes,
        0,
        options.expected_files,
        None,
        Some("正在创建安全暂存副本".to_string()),
    ));

    let mut staging_active = true;

    let result: Result<MigrationOutcome, String> = (|| {
        let stats = copy_tree(
            &source,
            &staging,
            options.expected_files,
            options.expected_dirs,
            options.expected_bytes,
            cancel,
            progress,
        )?;

        if stats.files != options.expected_files
            || stats.bytes != options.expected_bytes
            || stats.dirs != options.expected_dirs
        {
            return Err(format!(
                "源目录内容在预检后发生变化（预检 {} 文件 / {} 字节 / {} 目录，实际 {} 文件 / {} 字节 / {} 目录）。为安全起见已中止，原目录未做任何改动。",
                options.expected_files,
                options.expected_bytes,
                options.expected_dirs,
                stats.files,
                stats.bytes,
                stats.dirs
            ));
        }

        progress(make_event(
            "verifying",
            "校验副本",
            99.0,
            stats.bytes,
            options.expected_bytes,
            stats.files,
            options.expected_files,
            None,
            Some("正在核对暂存副本".to_string()),
        ));

        let (verify_files, verify_dirs, verify_bytes) = tree_totals(&staging)?;
        if verify_files != stats.files || verify_dirs != stats.dirs || verify_bytes != stats.bytes {
            return Err("暂存副本文件数量或大小不一致，已中止".to_string());
        }
        verify_digests(&stats.digests, stats.bytes, cancel, progress)?;

        let (source_files, source_dirs, source_bytes) = tree_totals(&source)?;
        if source_files != stats.files || source_dirs != stats.dirs || source_bytes != stats.bytes {
            return Err("预检后源目录内容发生了变化，已中止，原目录未做任何改动".to_string());
        }

        cancelled(cancel)?;

        fs::rename(extended(&staging), extended(&target))
            .map_err(|err| format!("将副本移动到目标位置失败：{err}"))?;
        staging_active = false;

        if let Err(err) = cancelled(cancel) {
            let _ = remove_tree_safe(&target);
            return Err(err);
        }

        let source_meta = fs::symlink_metadata(extended(&source))
            .map_err(|err| format!("读取原目录失败：{err}"))?;
        if is_reparse(&source_meta) {
            let _ = remove_tree_safe(&target);
            return Err("原目录在迁移过程中变成了链接，已中止".to_string());
        }

        let source_name = source
            .file_name()
            .ok_or_else(|| "源目录名称无效".to_string())?
            .to_string_lossy()
            .to_string();
        let backup = source
            .parent()
            .ok_or_else(|| "源目录没有父目录".to_string())?
            .join(format!("{source_name}.moveonlink-backup-{stamp}"));
        if fs::symlink_metadata(extended(&backup)).is_ok() {
            let _ = remove_tree_safe(&target);
            return Err("备份目录名冲突，已中止".to_string());
        }

        progress(make_event(
            "linking",
            "建立链接",
            99.5,
            stats.bytes,
            options.expected_bytes,
            stats.files,
            options.expected_files,
            None,
            Some("正在替换原路径".to_string()),
        ));

        if let Some(journal_path) = &options.journal_path {
            write_journal(
                journal_path,
                &SwapJournal {
                    source: display_path(&source),
                    target: display_path(&target),
                    backup: display_path(&backup),
                    link_type: options.link_type.as_str().to_string(),
                },
            )
            .map_err(|err| {
                let _ = remove_tree_safe(&target);
                format!("无法写入安全日志，已拒绝迁移：{err}")
            })?;
        }

        if let Err(err) = fs::rename(extended(&source), extended(&backup)) {
            clear_journal(options);
            let _ = remove_tree_safe(&target);
            return Err(format!(
                "无法将原目录暂时改名（可能有文件被占用）：{err}。原目录未做任何改动。"
            ));
        }

        if let Err(err) = create_link(&target, &source, options.link_type) {
            let restored = restore_from_backup(&source, &backup);
            if restored {
                clear_journal(options);
                let _ = remove_tree_safe(&target);
                return Err(format!(
                    "创建链接失败：{err}。原目录已恢复，文件未受影响。"
                ));
            }
            return Err(format!(
                "创建链接失败：{err}。原目录保留在 {}，目标副本保留在 {}，请手动恢复。",
                display_path(&backup),
                display_path(&target)
            ));
        }

        if let Err(err) = verify_link(&source, &target, options.link_type) {
            let _ = remove_link(&source);
            let restored = restore_from_backup(&source, &backup);
            if restored {
                clear_journal(options);
                let _ = remove_tree_safe(&target);
                return Err(format!(
                    "链接校验失败：{err}。原目录已恢复，文件未受影响。"
                ));
            }
            return Err(format!(
                "链接校验失败：{err}。原目录保留在 {}，目标副本保留在 {}，请手动恢复。",
                display_path(&backup),
                display_path(&target)
            ));
        }

        clear_journal(options);

        let mut warning = None;
        let mut retained_backup = None;
        if let Err(err) = remove_tree_safe(&backup) {
            warning = Some(format!(
                "迁移已完成，但旧目录备份清理失败：{err}。备份保留在 {}",
                display_path(&backup)
            ));
            retained_backup = Some(display_path(&backup));
        }

        progress(make_event(
            "done",
            "迁移完成",
            100.0,
            stats.bytes,
            options.expected_bytes,
            stats.files,
            options.expected_files,
            None,
            Some("原路径已由链接接管".to_string()),
        ));

        Ok(MigrationOutcome {
            size_bytes: stats.bytes,
            file_count: stats.files,
            dir_count: stats.dirs,
            link_type: options.link_type,
            warning,
            backup_path: retained_backup,
        })
    })();

    if staging_active {
        let _ = remove_tree_safe(&staging);
    }

    result
}

pub fn undo(
    source: &Path,
    target: &Path,
    link_type: LinkType,
    cancel: &AtomicBool,
    progress: &dyn Fn(ProgressEvent),
) -> Result<UndoOutcome, String> {
    let source_meta = fs::symlink_metadata(extended(source))
        .map_err(|_| "原路径不存在，无法撤销".to_string())?;
    if !is_reparse(&source_meta) {
        return Err("原路径已经不是链接，可能已经恢复过".to_string());
    }

    let stored = link_target(source, link_type)?;
    let stored = pathutil::canonicalize_existing(&stored).unwrap_or(stored);
    let target_canonical = pathutil::canonicalize_existing(target)
        .map_err(|err| format!("目标目录不可用：{err}"))?;
    if pathutil::normalize(&stored) != pathutil::normalize(&target_canonical) {
        return Err("链接指向与迁移记录不一致，拒绝撤销".to_string());
    }

    let target_meta = fs::symlink_metadata(extended(&target_canonical))
        .map_err(|err| format!("读取目标目录失败：{err}"))?;
    if !target_meta.is_dir() || is_reparse(&target_meta) {
        return Err("目标目录状态异常，拒绝撤销".to_string());
    }

    let (files, dirs, bytes) = tree_totals(&target_canonical)?;
    let parent = source
        .parent()
        .ok_or_else(|| "原路径没有父目录".to_string())?;
    let stamp = pathutil::stamp();
    let staging = parent.join(format!(".moveonlink-undo-{stamp}"));
    if fs::symlink_metadata(extended(&staging)).is_ok() {
        return Err("撤销暂存目录已存在，请稍后重试".to_string());
    }
    fs::create_dir(extended(&staging))
        .map_err(|err| format!("创建撤销暂存目录失败：{err}"))?;

    let mut staging_active = true;
    let result: Result<UndoOutcome, String> = (|| {
        let stats = copy_tree(
            &target_canonical,
            &staging,
            files,
            dirs,
            bytes,
            cancel,
            progress,
        )?;
        if stats.files != files || stats.dirs != dirs || stats.bytes != bytes {
            return Err("目标目录内容发生变化，已中止撤销".to_string());
        }

        progress(make_event(
            "verifying",
            "校验撤销副本",
            99.0,
            stats.bytes,
            bytes,
            stats.files,
            files,
            None,
            Some("正在核对撤销副本".to_string()),
        ));

        let (vf, vd, vb) = tree_totals(&staging)?;
        if vf != files || vd != dirs || vb != bytes {
            return Err("撤销副本校验不一致，已中止".to_string());
        }
        verify_digests(&stats.digests, stats.bytes, cancel, progress)?;

        let (current_files, current_dirs, current_bytes) = tree_totals(&target_canonical)?;
        if current_files != files || current_dirs != dirs || current_bytes != bytes {
            return Err("目标目录内容在撤销过程中发生变化，已中止，链接保持不变".to_string());
        }

        cancelled(cancel)?;

        progress(make_event(
            "linking",
            "恢复原目录",
            99.5,
            stats.bytes,
            bytes,
            stats.files,
            files,
            None,
            Some("正在移除链接并恢复原路径".to_string()),
        ));

        let link_backup = parent.join(format!(".moveonlink-undo-link-{stamp}"));
        if let Err(err) = fs::rename(extended(source), extended(&link_backup)) {
            return Err(format!("暂时移开链接失败，撤销未开始：{err}"));
        }

        // 从这里开始保留校验副本，便于任何失败后人工恢复。
        staging_active = false;

        if let Err(err) = fs::rename(extended(&staging), extended(source)) {
            let link_restored = fs::rename(extended(&link_backup), extended(source)).is_ok();
            return Err(format!(
                "恢复原目录失败：{err}。{}",
                if link_restored {
                    "原链接已恢复，文件仍可访问".to_string()
                } else {
                    format!(
                        "校验副本保留在 {}，目标数据保留在 {}，请手动恢复",
                        display_path(&staging),
                        display_path(&target_canonical)
                    )
                }
            ));
        }

        let _ = remove_link(&link_backup);

        let warning = match remove_tree_safe(&target_canonical) {
            Ok(()) => None,
            Err(err) => Some(format!(
                "已恢复原目录，但目标盘副本清理失败：{err}。副本保留在 {}",
                display_path(&target_canonical)
            )),
        };

        progress(make_event(
            "done",
            "撤销完成",
            100.0,
            stats.bytes,
            bytes,
            stats.files,
            files,
            None,
            Some("文件已移回原路径".to_string()),
        ));

        Ok(UndoOutcome { warning })
    })();

    if staging_active {
        let _ = remove_tree_safe(&staging);
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::scan;
    use std::fs;
    use std::os::windows::fs::OpenOptionsExt;

    fn setup() -> (tempfile::TempDir, PathBuf, PathBuf) {
        let temp = tempfile::tempdir().expect("tempdir");
        let source = temp.path().join("src");
        let target = temp.path().join("dst");
        fs::create_dir_all(source.join("nested")).expect("create source");
        fs::write(source.join("a.txt"), b"hello world").expect("write a");
        fs::write(source.join("nested").join("b.bin"), vec![7u8; 4096]).expect("write b");
        (temp, source, target)
    }

    fn options(source: &Path, target: &Path) -> MigrationOptions {
        let (files, dirs, bytes) = tree_totals(source).expect("totals");
        MigrationOptions {
            source: source.to_path_buf(),
            target: target.to_path_buf(),
            link_type: LinkType::Junction,
            expected_bytes: bytes,
            expected_files: files,
            expected_dirs: dirs,
            journal_path: None,
        }
    }

    fn cleanup(source: &Path, target: &Path) {
        if junction::exists(source).unwrap_or(false) {
            let _ = junction::delete(source);
        }
        let _ = remove_tree_safe(target);
        let _ = remove_tree_safe(source);
    }

    fn has_entry(parent: &Path, prefix: &str) -> bool {
        fs::read_dir(parent)
            .map(|entries| {
                entries.filter_map(|entry| entry.ok()).any(|entry| {
                    entry
                        .file_name()
                        .to_string_lossy()
                        .starts_with(prefix)
                })
            })
            .unwrap_or(false)
    }

    #[test]
    fn migrate_creates_junction_and_preserves_content() {
        let (_temp, source, target) = setup();
        let opts = options(&source, &target);
        let cancel = AtomicBool::new(false);
        let outcome = migrate(&opts, &cancel, &|_| {}).expect("migrate");

        assert_eq!(outcome.file_count, 2);
        assert_eq!(outcome.dir_count, 1);
        assert!(junction::exists(&source).unwrap_or(false));
        assert_eq!(fs::read(source.join("a.txt")).expect("read through link"), b"hello world");
        assert!(target.join("nested").join("b.bin").is_file());
        assert!(!has_entry(source.parent().unwrap(), ".moveonlink-staging-"));
        assert!(!has_entry(source.parent().unwrap(), ".moveonlink-backup-"));

        cleanup(&source, &target);
    }

    #[test]
    fn migrate_refuses_locked_file_without_touching_source() {
        let (_temp, source, target) = setup();
        let held = OpenOptions::new()
            .read(true)
            .write(true)
            .share_mode(0)
            .open(source.join("a.txt"))
            .expect("hold file");

        let opts = options(&source, &target);
        let cancel = AtomicBool::new(false);
        let err = migrate(&opts, &cancel, &|_| {}).expect_err("should refuse locked file");

        assert!(err.contains("独占打开") || err.contains("占用"), "{err}");
        assert!(!junction::exists(&source).unwrap_or(false));
        assert!(source.join("a.txt").is_file());
        assert!(!target.exists());
        assert!(!has_entry(source.parent().unwrap(), ".moveonlink-staging-"));

        drop(held);
        cleanup(&source, &target);
    }

    #[test]
    fn preflight_reports_locked_file() {
        let (_temp, source, target) = setup();
        let held = OpenOptions::new()
            .read(true)
            .write(true)
            .share_mode(0)
            .open(source.join("a.txt"))
            .expect("hold file");

        let report = scan::preflight(
            &source,
            &target,
            "junction",
            &AtomicBool::new(false),
            &|_| {},
        )
        .expect("preflight");

        assert!(report.locked_count >= 1);
        assert!(report.blocked);
        assert!(report
            .blocking_issues
            .iter()
            .any(|issue| issue.code == "files_in_use"));

        drop(held);
        cleanup(&source, &target);
    }

    #[test]
    fn preflight_rejects_existing_target() {
        let (_temp, source, target) = setup();
        fs::create_dir(&target).expect("create target");

        let report = scan::preflight(
            &source,
            &target,
            "junction",
            &AtomicBool::new(false),
            &|_| {},
        )
        .expect("preflight");

        assert!(report.blocked);
        assert!(report
            .blocking_issues
            .iter()
            .any(|issue| issue.code == "target_exists"));

        cleanup(&source, &target);
    }

    #[test]
    fn preflight_rejects_target_inside_source() {
        let (_temp, source, _target) = setup();
        let nested = source.join("inside");

        let report = scan::preflight(
            &source,
            &nested,
            "junction",
            &AtomicBool::new(false),
            &|_| {},
        )
        .expect("preflight");

        assert!(report.blocked);
        assert!(report
            .blocking_issues
            .iter()
            .any(|issue| issue.code == "target_inside_source"));

        cleanup(&source, &nested);
    }

    #[test]
    fn migrate_rejects_existing_target() {
        let (_temp, source, target) = setup();
        fs::create_dir(&target).expect("create target");

        let opts = options(&source, &target);
        let err = migrate(&opts, &AtomicBool::new(false), &|_| {})
            .expect_err("existing target must be rejected");
        assert!(err.contains("已经存在"), "{err}");
        assert!(source.join("a.txt").is_file());

        cleanup(&source, &target);
    }

    #[test]
    fn undo_restores_original_directory() {
        let (_temp, source, target) = setup();
        let opts = options(&source, &target);
        migrate(&opts, &AtomicBool::new(false), &|_| {}).expect("migrate");
        assert!(junction::exists(&source).unwrap_or(false));

        let outcome = undo(
            &source,
            &target,
            LinkType::Junction,
            &AtomicBool::new(false),
            &|_| {},
        )
        .expect("undo");

        assert!(outcome.warning.is_none(), "{:?}", outcome.warning);
        assert!(!junction::exists(&source).unwrap_or(false));
        assert!(source.join("a.txt").is_file());
        assert!(source.join("nested").join("b.bin").is_file());
        assert!(!target.exists());

        let undo_leftovers = fs::read_dir(source.parent().unwrap())
            .unwrap()
            .filter_map(|entry| entry.ok())
            .filter(|entry| {
                entry
                    .file_name()
                    .to_string_lossy()
                    .starts_with(".moveonlink-undo-link-")
            })
            .count();
        assert_eq!(undo_leftovers, 0);

        cleanup(&source, &target);
    }

    #[test]
    fn remove_link_removes_empty_directory_left_by_junction_delete() {
        let temp = tempfile::tempdir().expect("tempdir");
        let target = temp.path().join("target");
        fs::create_dir(&target).expect("target");
        let link = temp.path().join("link");
        junction::create(&target, &link).expect("junction");

        assert!(junction::exists(&link).unwrap_or(false));
        remove_link(&link).expect("remove_link");
        assert!(
            fs::symlink_metadata(&link).is_err(),
            "junction 删除后不应留下空目录"
        );
    }

    #[test]
    fn preflight_cleans_stale_probe_artifacts() {
        let (temp, source, target) = setup();
        let parent = temp.path();
        fs::create_dir(parent.join(".moveonlink_junction_probe_stale_empty"))
            .expect("stale empty dir");
        junction::create(
            parent,
            &parent.join(".moveonlink_junction_probe_stale_link"),
        )
        .expect("stale junction");
        fs::write(parent.join(".moveonlink_write_probe_stale"), b"x")
            .expect("stale probe file");

        let report = scan::preflight(
            &source,
            &target,
            "junction",
            &AtomicBool::new(false),
            &|_| {},
        )
        .expect("preflight");
        assert!(!report.blocked, "{:?}", report.blocking_issues);

        let leftovers: Vec<String> = fs::read_dir(parent)
            .unwrap()
            .filter_map(|entry| entry.ok())
            .map(|entry| entry.file_name().to_string_lossy().to_string())
            .filter(|name| name.starts_with(".moveonlink_"))
            .collect();
        assert!(leftovers.is_empty(), "leftover probes: {leftovers:?}");

        cleanup(&source, &target);
    }

    #[test]
    fn migration_removes_journal_on_success() {
        let (temp, source, target) = setup();
        let journal_path = temp.path().join("journal.json");
        let mut opts = options(&source, &target);
        opts.journal_path = Some(journal_path.clone());

        migrate(&opts, &AtomicBool::new(false), &|_| {}).expect("migrate");
        assert!(!journal_path.exists());

        cleanup(&source, &target);
    }

    #[test]
    fn recovers_interrupted_swap_from_backup() {
        let (temp, source, target) = setup();
        let backup = source.parent().unwrap().join("src.moveonlink-backup-test");
        fs::rename(&source, &backup).expect("rename to backup");
        fs::create_dir_all(&target).expect("target");
        fs::write(target.join("copy.txt"), b"copy").expect("copy file");

        let journal_path = temp.path().join("migration-journal.json");
        let journal = serde_json::json!({
            "source": source.to_string_lossy(),
            "target": target.to_string_lossy(),
            "backup": backup.to_string_lossy(),
            "linkType": "junction"
        });
        fs::write(&journal_path, journal.to_string()).expect("journal");

        let notice = recover_interrupted(&journal_path).expect("recover");
        let notice = notice.expect("notice");
        assert_eq!(notice.kind, NOTICE_WARNING);
        assert!(notice
            .paths
            .iter()
            .any(|item| item.is_dir && item.path == display_path(&target)));
        assert!(source.join("a.txt").is_file());
        assert!(!journal_path.exists());

        cleanup(&source, &target);
        let _ = fs::remove_dir_all(&backup);
    }

    #[test]
    fn preflight_rejects_reparse_source() {
        let (temp, source, target) = setup();
        let real = temp.path().join("real");
        fs::rename(&source, &real).expect("move real dir");
        junction::create(&real, &source).expect("create junction");

        let report = scan::preflight(
            &source,
            &target,
            "junction",
            &AtomicBool::new(false),
            &|_| {},
        )
        .expect("preflight");

        assert!(report.blocked);
        assert!(report
            .blocking_issues
            .iter()
            .any(|issue| issue.code == "source_is_link"));

        let _ = junction::delete(&source);
        let _ = fs::remove_dir_all(&real);
    }

    #[test]
    fn preflight_rejects_nested_reparse_point() {
        let (temp, source, target) = setup();
        let outside = temp.path().join("outside");
        fs::create_dir(&outside).expect("outside");
        let link = source.join("linked");
        junction::create(&outside, &link).expect("nested junction");

        let report = scan::preflight(
            &source,
            &target,
            "junction",
            &AtomicBool::new(false),
            &|_| {},
        )
        .expect("preflight");

        assert!(report.blocked);
        assert!(report
            .blocking_issues
            .iter()
            .any(|issue| issue.code == "nested_link"));

        let _ = junction::delete(&link);
        let _ = fs::remove_dir_all(&outside);
        cleanup(&source, &target);
    }
}
