use std::path::Path;

/// 权限模式
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    /// 默认:写文件/执行命令先问(无头模式没法问,直接拒)
    Ask,
    /// 改文件自动放行,命令仍要问
    AcceptEdits,
    /// 全放行,危险命令黑名单除外
    BypassPermissions,
    /// YOLO:一切放行、黑名单也跳过。只该在完全信任模型的自用场景开。
    Yolo,
}

impl Mode {
    pub fn parse(s: &str) -> anyhow::Result<Self> {
        match s {
            "ask" => Ok(Mode::Ask),
            "acceptEdits" => Ok(Mode::AcceptEdits),
            "bypassPermissions" => Ok(Mode::BypassPermissions),
            "yolo" | "YOLO" => Ok(Mode::Yolo),
            other => anyhow::bail!(
                "未知权限模式: {other}(可选: ask / acceptEdits / bypassPermissions / yolo)"
            ),
        }
    }
}

/// 一次权限判定结果
pub enum Decision {
    Allow,
    /// 无头模式下无法交互确认,拒绝并给出提示
    Deny(String),
}

/// 全局权限管理器(当前实现:无头模式按档位静态判定)
#[derive(Debug, Clone)]
pub struct Permission {
    pub mode: Mode,
}

impl Permission {
    pub fn new(mode: Mode) -> Self {
        Self { mode }
    }

    /// 文件写入类操作(read_file / write_file / edit 等改写文件)
    pub fn check_file_write(&self) -> Decision {
        match self.mode {
            Mode::Ask => Decision::Deny(
                "ask 模式下写文件要确认,无头模式直接拒绝。要自动改文件就加 --permission acceptEdits"
                    .into(),
            ),
            _ => Decision::Allow,
        }
    }

    /// 终端命令执行。YOLO:连危险命令黑名单也放行。
    pub fn check_command(&self, command: &str) -> Decision {
        if self.mode == Mode::Yolo {
            return Decision::Allow;
        }
        if let Some(reason) = is_dangerous_command(command) {
            return Decision::Deny(format!("命令被安全策略拦截: {reason}"));
        }
        match self.mode {
            Mode::Ask | Mode::AcceptEdits => Decision::Deny(
                "该模式下跑命令要确认,无头模式直接拒绝。要自动执行就加 --permission bypassPermissions"
                    .into(),
            ),
            // Yolo 已在上面直接放行;此分支仅为穷尽 match
            Mode::BypassPermissions | Mode::Yolo => Decision::Allow,
        }
    }
}

/// 危险命令启发式识别。返回被拦截的原因。
pub fn is_dangerous_command(cmd: &str) -> Option<&'static str> {
    let c = cmd.trim_start();
    // 删除根目录/家目录
    for pat in [
        "rm -rf /", "rm -fr /", "rm -rf --no-preserve-root /", "rm -rf ~",
    ] {
        if c.contains(pat) {
            return Some("检测到删除根目录/家目录操作");
        }
    }
    // 磁盘级破坏
    for pat in [
        "mkfs.", "fdisk", "dd if=", "of=/dev/sda", "of=/dev/sdb", "of=/dev/nvme",
        "> /dev/sda", "shred /dev/sda", "shutdown", "poweroff", "reboot",
    ] {
        if c.contains(pat) {
            return Some("检测到磁盘级/系统级破坏性操作");
        }
    }
    // git 强推
    if c.contains("git push") && (c.contains("--force") || c.contains(" -f ")) {
        return Some("检测到 git 强制推送(--force),可能覆盖远端历史");
    }
    // 权限递归放开
    if c.contains("chmod -R 777") || c.contains("chmod 777 -R") {
        return Some("检测到递归放开权限(chmod -R 777)");
    }
    None
}

/// 快照到 undo 目录,事后可回滚;返回快照路径,文件不存在(新建)返回 None
pub fn snapshot_file(session_dir: &Path, file: &Path) -> anyhow::Result<Option<std::path::PathBuf>> {
    if !file.exists() {
        return Ok(None);
    }
    let meta = std::fs::metadata(file)?;
    if meta.is_dir() {
        return Ok(None);
    }
    let target = session_dir.join(file.strip_prefix("/").unwrap_or(file));
    if let Some(parent) = target.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::copy(file, &target)?;
    Ok(Some(target))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn danger_detection() {
        assert!(is_dangerous_command("rm -rf /").is_some());
        assert!(is_dangerous_command("sudo rm -rf ~/x").is_some());
        assert!(is_dangerous_command("mkfs.ext4 /dev/sdb1").is_some());
        assert!(is_dangerous_command("git push --force origin main").is_some());
        assert!(is_dangerous_command("dd if=/dev/zero of=/dev/sda bs=1M").is_some());
    }

    #[test]
    fn safe_commands_pass() {
        assert!(is_dangerous_command("rm -rf ./build").is_none());
        assert!(is_dangerous_command("git push origin main").is_none());
        assert!(is_dangerous_command("ls -la").is_none());
        assert!(is_dangerous_command("grep -r force .").is_none());
    }

    #[test]
    fn mode_parsing() {
        assert_eq!(Mode::parse("ask").unwrap(), Mode::Ask);
        assert!(Mode::parse("nope").is_err());
    }

    #[test]
    fn ask_mode_denies_writes() {
        let p = Permission::new(Mode::Ask);
        assert!(matches!(p.check_file_write(), Decision::Deny(_)));
        assert!(matches!(p.check_command("echo hi"), Decision::Deny(_)));
    }

    #[test]
    fn bypass_allows_but_danger_blocked() {
        let p = Permission::new(Mode::BypassPermissions);
        assert!(matches!(p.check_file_write(), Decision::Allow));
        assert!(matches!(p.check_command("echo hi"), Decision::Allow));
        assert!(matches!(p.check_command("rm -rf /"), Decision::Deny(_)));
    }

    #[test]
    fn yolo_allows_everything_including_dangerous() {
        let p = Permission::new(Mode::Yolo);
        assert!(matches!(p.check_file_write(), Decision::Allow));
        assert!(matches!(p.check_command("echo hi"), Decision::Allow));
        assert!(matches!(p.check_command("rm -rf /"), Decision::Allow)); // YOLO 跳过黑名单
        assert_eq!(Mode::parse("yolo").unwrap(), Mode::Yolo);
        assert_eq!(Mode::parse("YOLO").unwrap(), Mode::Yolo);
    }
}
