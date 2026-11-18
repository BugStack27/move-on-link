//! 系统保护目录名单：这些位置一旦被迁移（原位置被替换成链接），系统或已安装软件可能无法运行。
//! 名单由环境变量（SystemRoot / ProgramFiles / ProgramData 等）与固定约定组成，尽量适配系统盘不在 C: 的情况。
//!
//! 分级原则：
//! - 目录本身是系统组件（Windows、引导 / 恢复目录、系统保留目录）→ 整个目录树都禁止迁移；
//! - 目录是“容器型”根目录（Program Files、ProgramData）→ 只禁止根目录本身，里面的具体程序 / 应用数据可以迁移；
//! - 用户数据（AppData、文档、下载等）→ 允许迁移，迁移后原位置由 Junction 接管。

use crate::pathutil;
use std::path::{Path, PathBuf};

const REASON_WINDOWS: &str = "Windows 系统目录（含子目录），迁移会导致系统无法启动或更新失败";
const REASON_PROGRAM_FILES_ROOT: &str = "程序安装根目录，请改为选择其中的具体程序文件夹";
const REASON_PROGRAM_FILES_SYSTEM: &str = "系统级程序目录（含子目录），迁移会导致系统组件或应用商店程序失效";
const REASON_PROGRAM_DATA_ROOT: &str = "系统共享数据根目录 ProgramData，请改为选择其中具体软件的文件夹";
const REASON_PROGRAM_DATA_SYSTEM: &str = "系统组件数据目录 ProgramData\\Microsoft（含子目录），迁移会影响系统服务与安全组件";
const REASON_PROFILE: &str = "用户配置文件目录，迁移会导致无法登录或用户数据损坏";
const REASON_USERS: &str = "用户目录根 Users，迁移会影响本机所有账户";
const REASON_BOOT: &str = "系统引导 / 恢复目录（含子目录），迁移会导致系统无法启动或修复";
const REASON_OLD_SYSTEM: &str = "旧系统安装目录（含子目录），迁移会影响系统回退与磁盘清理";
const REASON_RESERVED: &str = "Windows 保留目录，不能迁移或作为迁移目标";
const REASON_DRIVE_ROOT: &str = "磁盘根目录，不能整盘迁移或作为迁移目标";
const REASON_SELF: &str = "程序自身所在目录，运行期间不能迁移";

#[derive(Debug, Clone, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ProtectedDir {
    /// 命中的保护范围（实际目录，仅用于展示）
    pub path: String,
    /// 不允许迁移的原因
    pub reason: String,
}

struct Rule {
    display: String,
    norm: String,
    whole_tree: bool,
    reason: &'static str,
}

fn add_rule(rules: &mut Vec<Rule>, path: PathBuf, whole_tree: bool, reason: &'static str) {
    let norm = pathutil::normalize(&path);
    if norm.is_empty() {
        return;
    }
    if rules.iter().any(|rule| rule.norm == norm) {
        return;
    }
    rules.push(Rule {
        display: pathutil::display_path(&path),
        norm,
        whole_tree,
        reason,
    });
}

fn env_path(name: &str) -> Option<PathBuf> {
    std::env::var_os(name)
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
}

fn system_drive() -> Option<PathBuf> {
    env_path("SystemDrive")
        .map(|drive| PathBuf::from(format!("{}\\", pathutil::display_path(&drive))))
        .or_else(|| {
            env_path("SystemRoot").and_then(|root| {
                root.ancestors()
                    .last()
                    .map(|path| path.to_path_buf())
            })
        })
}

fn rules() -> Vec<Rule> {
    let mut rules: Vec<Rule> = Vec::new();

    // Windows 自身：整个目录树都不能动。
    if let Some(root) = env_path("SystemRoot").or_else(|| env_path("windir")) {
        add_rule(&mut rules, root, true, REASON_WINDOWS);
    }

    // 程序安装目录：根目录禁止，具体程序文件夹允许迁移。
    for name in ["ProgramFiles", "ProgramFiles(x86)", "ProgramW6432"] {
        if let Some(root) = env_path(name) {
            add_rule(&mut rules, root.clone(), false, REASON_PROGRAM_FILES_ROOT);
            for system_dir in ["Common Files", "WindowsApps", "Windows Defender", "Windows NT"] {
                add_rule(&mut rules, root.join(system_dir), true, REASON_PROGRAM_FILES_SYSTEM);
            }
        }
    }

    // ProgramData：根目录与系统组件数据禁止，其他软件自己的数据允许迁移。
    if let Some(root) = env_path("ProgramData") {
        add_rule(&mut rules, root.clone(), false, REASON_PROGRAM_DATA_ROOT);
        add_rule(&mut rules, root.join("Microsoft"), true, REASON_PROGRAM_DATA_SYSTEM);
    }

    // 用户配置文件：整份配置文件不能迁移，AppData / 文档等子目录可以。
    if let Some(home) = env_path("USERPROFILE") {
        add_rule(&mut rules, home, false, REASON_PROFILE);
    }

    if let Some(drive) = system_drive() {
        add_rule(&mut rules, drive.join("Users"), false, REASON_USERS);
        add_rule(&mut rules, drive.join("Documents and Settings"), false, REASON_RESERVED);
    }

    // 任意盘符上的系统保留 / 引导 / 恢复目录。
    for drive in drive_roots() {
        add_rule(&mut rules, drive.join("$Recycle.Bin"), true, REASON_RESERVED);
        add_rule(&mut rules, drive.join("System Volume Information"), true, REASON_RESERVED);
        add_rule(&mut rules, drive.join("Config.Msi"), true, REASON_RESERVED);
        add_rule(&mut rules, drive.join("$WinREAgent"), true, REASON_RESERVED);
        add_rule(&mut rules, drive.join("Recovery"), true, REASON_BOOT);
        add_rule(&mut rules, drive.join("Boot"), true, REASON_BOOT);
        add_rule(&mut rules, drive.join("EFI"), true, REASON_BOOT);
        add_rule(&mut rules, drive.join("Windows.old"), true, REASON_OLD_SYSTEM);
        add_rule(&mut rules, drive.join("$SysReset"), true, REASON_RESERVED);
        add_rule(&mut rules, drive.join("$Windows.~BT"), true, REASON_RESERVED);
        add_rule(&mut rules, drive.join("$Windows.~WS"), true, REASON_RESERVED);
        add_rule(&mut rules, drive.join("PerfLogs"), false, REASON_RESERVED);
    }

    rules
}

/// 本机存在的盘符根（C:\、D:\ …）。
fn drive_roots() -> Vec<PathBuf> {
    let mut roots = Vec::new();
    let mask = unsafe { windows_sys::Win32::Storage::FileSystem::GetLogicalDrives() };
    for index in 0..26u32 {
        if mask & (1 << index) == 0 {
            continue;
        }
        let letter = (b'A' + index as u8) as char;
        roots.push(PathBuf::from(format!("{letter}:\\")));
    }
    if roots.is_empty() {
        if let Some(drive) = system_drive() {
            roots.push(drive);
        }
    }
    roots
}

/// 规范化后的路径若正好是盘符根（`c:` 或 `c:\`），返回该盘符。
fn drive_root_of(norm: &str) -> Option<String> {
    let bytes = norm.as_bytes();
    if bytes.len() >= 2 && bytes[1] == b':' && (bytes.len() == 2 || (bytes.len() == 3 && bytes[2] == b'\\')) {
        return Some(norm[..2].to_string());
    }
    None
}

/// 判断目录是否属于系统保护范围；命中时返回命中的保护范围与原因。
pub fn check(path: &Path) -> Option<ProtectedDir> {
    let norm = pathutil::normalize(path);
    if norm.is_empty() {
        return None;
    }

    if let Some(drive) = drive_root_of(&norm) {
        return Some(ProtectedDir {
            path: format!("{}:\\", drive.to_uppercase()),
            reason: REASON_DRIVE_ROOT.to_string(),
        });
    }

    // 程序自身所在目录（含正在运行的 exe），运行期间迁移会直接破坏当前进程。
    if let Ok(exe) = std::env::current_exe() {
        if let Some(dir) = exe.parent() {
            let dir_norm = pathutil::normalize(dir);
            if !dir_norm.is_empty()
                && (pathutil::is_within(&norm, &dir_norm)
                    || pathutil::is_within(&dir_norm, &norm))
            {
                return Some(ProtectedDir {
                    path: pathutil::display_path(dir),
                    reason: REASON_SELF.to_string(),
                });
            }
        }
    }

    // Users 下的直接子目录都是某个账户的配置文件（含 Public / Default），整目录迁移会破坏登录。
    if let Some(drive) = system_drive() {
        let users = pathutil::normalize(&drive.join("Users"));
        if let Some(parent) = Path::new(&norm).parent() {
            if pathutil::normalize(parent) == users {
                return Some(ProtectedDir {
                    path: pathutil::display_path(path),
                    reason: REASON_PROFILE.to_string(),
                });
            }
        }
    }

    for rule in rules() {
        if norm == rule.norm || (rule.whole_tree && pathutil::is_within(&norm, &rule.norm)) {
            return Some(ProtectedDir {
                path: rule.display,
                reason: rule.reason.to_string(),
            });
        }
    }

    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn env(name: &str) -> PathBuf {
        PathBuf::from(std::env::var(name).unwrap_or_else(|_| panic!("缺少环境变量 {name}")))
    }

    #[test]
    fn drive_root_is_protected() {
        assert!(check(Path::new("C:\\")).is_some());
        assert!(check(Path::new("D:\\")).is_some());
        let hit = check(Path::new("C:\\")).unwrap();
        assert!(hit.reason.contains("磁盘根目录"));
    }

    #[test]
    fn windows_tree_is_protected() {
        let system32 = env("SystemRoot").join("System32");
        assert!(check(&system32).is_some());
    }

    #[test]
    fn user_profile_root_protected_but_subfolder_allowed() {
        let home = env("USERPROFILE");
        assert!(check(&home).is_some());
        assert!(check(&home.join("Documents").join("WeChat Files")).is_none());
        assert!(check(&home.join("Downloads")).is_none());
    }

    #[test]
    fn appdata_is_allowed() {
        let home = env("USERPROFILE");
        let appdata = home.join("AppData");
        assert!(check(&appdata).is_none());
        assert!(check(&appdata.join("Local").join("Tencent")).is_none());
    }

    #[test]
    fn program_files_root_blocked_but_child_allowed() {
        let root = env("ProgramFiles");
        assert!(check(&root).is_some());
        assert!(check(&root.join("SomeGame")).is_none());
        assert!(check(&root.join("WindowsApps")).is_some());
    }

    #[test]
    fn program_data_root_blocked_but_app_data_allowed() {
        let root = env("ProgramData");
        assert!(check(&root).is_some());
        assert!(check(&root.join("Package Cache")).is_none());
        assert!(check(&root.join("Microsoft")).is_some());
    }

    #[test]
    fn users_root_children_are_protected() {
        let drive = std::env::var("SystemDrive").unwrap_or_else(|_| "C:".to_string());
        let public = PathBuf::from(format!("{}\\Users\\Public", drive));
        assert!(check(&public).is_some());
    }

    #[test]
    fn recycle_bin_and_system_volume_information_are_protected() {
        let drive = std::env::var("SystemDrive").unwrap_or_else(|_| "C:".to_string());
        let recycle = PathBuf::from(format!("{}\\$Recycle.Bin", drive));
        let svi = PathBuf::from(format!("{}\\System Volume Information", drive));
        assert!(check(&recycle).is_some());
        assert!(check(&svi).is_some());
    }
}
