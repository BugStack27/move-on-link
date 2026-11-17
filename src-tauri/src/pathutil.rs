use std::os::windows::fs::MetadataExt;
use std::path::{Path, PathBuf};

pub const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x400;

pub fn is_reparse(meta: &std::fs::Metadata) -> bool {
    meta.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0
}

pub fn extended(path: &Path) -> PathBuf {
    let s = path.as_os_str().to_string_lossy();
    if s.starts_with(r"\\?\") {
        return path.to_path_buf();
    }
    if let Some(rest) = s.strip_prefix(r"\\") {
        return PathBuf::from(format!(r"\\?\UNC\{}", rest));
    }
    if s.len() >= 2 && s.as_bytes()[1] == b':' {
        return PathBuf::from(format!(r"\\?\{}", s));
    }
    path.to_path_buf()
}

pub fn display_path(path: &Path) -> String {
    let s = path.to_string_lossy().to_string();
    if let Some(rest) = s.strip_prefix(r"\\?\UNC\") {
        return format!(r"\\{}", rest);
    }
    if let Some(rest) = s.strip_prefix(r"\\?\") {
        return rest.to_string();
    }
    s
}

pub fn normalize(path: &Path) -> String {
    let mut s = display_path(path).replace('/', "\\");
    while s.len() > 3 && s.ends_with('\\') {
        s.pop();
    }
    s.to_lowercase()
}

fn prefix_base(path: &str) -> &str {
    path.trim_end_matches('\\')
}

pub fn is_within(child: &str, parent: &str) -> bool {
    child == parent || child.starts_with(&format!("{}\\", prefix_base(parent)))
}

pub fn canonicalize_existing(path: &Path) -> std::io::Result<PathBuf> {
    std::fs::canonicalize(extended(path)).map(|p| PathBuf::from(display_path(&p)))
}

pub fn resolve_target(path: &Path) -> std::io::Result<PathBuf> {
    if std::fs::symlink_metadata(extended(path)).is_ok() {
        return canonicalize_existing(path);
    }
    let parent = path.parent().ok_or_else(|| {
        std::io::Error::new(std::io::ErrorKind::InvalidInput, "路径没有父目录")
    })?;
    let name = path.file_name().ok_or_else(|| {
        std::io::Error::new(std::io::ErrorKind::InvalidInput, "路径没有名称")
    })?;
    let parent = canonicalize_existing(parent)?;
    Ok(parent.join(name))
}

pub fn invalid_final_name(path: &Path) -> Option<&'static str> {
    let name = match path.file_name() {
        Some(name) => name.to_string_lossy().trim().to_string(),
        None => return Some("目录名为空"),
    };
    if name.is_empty() {
        return Some("目录名为空");
    }
    let upper = name.to_uppercase();
    let base = upper.split('.').next().unwrap_or("");
    const RESERVED: [&str; 22] = [
        "CON", "PRN", "AUX", "NUL", "COM1", "COM2", "COM3", "COM4", "COM5", "COM6", "COM7",
        "COM8", "COM9", "LPT1", "LPT2", "LPT3", "LPT4", "LPT5", "LPT6", "LPT7", "LPT8", "LPT9",
    ];
    if RESERVED.contains(&base) {
        return Some("目录名是 Windows 保留名称");
    }
    if name.ends_with('.') || name.ends_with(' ') {
        return Some("目录名不能以点或空格结尾");
    }
    if name.chars().any(|c| r#"<>:"/\|?*"#.contains(c)) {
        return Some("目录名包含非法字符");
    }
    None
}

pub fn is_elevated() -> bool {
    unsafe { windows_sys::Win32::UI::Shell::IsUserAnAdmin() != 0 }
}

pub fn remove_empty_dir_with_retry(path: &Path) -> std::io::Result<()> {
    let mut last = None;
    for _ in 0..6 {
        match std::fs::remove_dir(extended(path)) {
            Ok(()) => return Ok(()),
            Err(err) => {
                if err.kind() == std::io::ErrorKind::NotFound {
                    return Ok(());
                }
                last = Some(err);
                std::thread::sleep(std::time::Duration::from_millis(60));
            }
        }
    }
    Err(last.unwrap_or_else(|| {
        std::io::Error::new(std::io::ErrorKind::Other, "删除空目录失败")
    }))
}

/// `junction::delete` 只移除重解析点，会留下一个空目录；这里把空目录一并删除。
pub fn delete_link_dir(path: &Path) -> std::io::Result<()> {
    match junction::exists(path) {
        Ok(true) => {
            junction::delete(path)?;
            remove_empty_dir_with_retry(path)
        }
        Ok(false) => std::fs::remove_dir(extended(path)),
        Err(err) => Err(err),
    }
}

pub fn now_iso() -> String {
    chrono::Local::now().to_rfc3339()
}

pub fn now_display() -> String {
    chrono::Local::now().format("%Y-%m-%d %H:%M").to_string()
}

pub fn stamp() -> String {
    chrono::Local::now().format("%Y%m%d_%H%M%S").to_string()
}

pub fn friendly_io_error(err: &std::io::Error) -> String {
    match err.raw_os_error() {
        Some(32) => "文件正被其他程序使用（共享冲突）".to_string(),
        Some(33) => "文件被锁定".to_string(),
        Some(5) => "没有访问权限".to_string(),
        Some(2) => "文件不存在或已被移除".to_string(),
        Some(3) => "路径不存在".to_string(),
        _ => err.to_string(),
    }
}
