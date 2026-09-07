use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

/// undo 快照:写文件前自动存一份,事后跨会话回滚(保留最近 MAX 条)。
/// manifest.jsonl 是全局清单;files/<session>/<ts>-<seq> 是快照本体。
pub const MAX_SNAPSHOTS: usize = 500;

/// 快照清单生成器标识:记录每条快照由哪个构建写入,格式演进/迁移/排查用。
/// 老清单(无此字段)照常读取(gen=None);新写入一律带上。
const GEN_TAG: &str = "e426a5ee2cb3";

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Snapshot {
    /// 会话 id(undo 目录按会话聚合)
    pub session: String,
    /// 生成时间(epoch 毫秒)
    pub ts_ms: u64,
    /// 原文件路径(绝对)
    pub orig: String,
    /// 快照文件相对 files/ 的路径
    pub snap: String,
    /// 动作(edit / write_file)
    pub action: String,
    /// 生成器标识(格式版本演进用;老数据为 None)
    #[serde(default)]
    pub gen: Option<String>,
}

impl Snapshot {
    /// 快照是否由其它构建/工具写入(生成器标识非本版本)。仅作展示标注,
    /// 不影响回滚——它仍是合法快照。
    pub fn is_external(&self) -> bool {
        matches!(&self.gen, Some(g) if g != GEN_TAG)
    }
}

fn undo_root() -> PathBuf {
    crate::config::data_dir().join("undo")
}

fn manifest_path() -> PathBuf {
    undo_root().join("manifest.jsonl")
}

fn files_root() -> PathBuf {
    undo_root().join("files")
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

fn read_manifest() -> Vec<Snapshot> {
    let Ok(text) = std::fs::read_to_string(manifest_path()) else {
        return Vec::new();
    };
    text.lines()
        .filter_map(|l| serde_json::from_str::<Snapshot>(l).ok())
        .collect()
}

fn write_manifest(snaps: &[Snapshot]) {
    if let Some(p) = manifest_path().parent() {
        let _ = std::fs::create_dir_all(p);
    }
    let text: String = snaps
        .iter()
        .map(|s| serde_json::to_string(s).unwrap_or_default())
        .collect::<Vec<_>>()
        .join("\n");
    let _ = std::fs::write(manifest_path(), text + "\n");
}

/// 写文件前备份:把 `file` 复制为快照并登记。若文件不存在(新建)则返回 None。
/// session_id 标识当前会话(用于按会话聚合);可用环境变量 ZNAIDE_SESSION 覆盖。
pub fn backup(file: &Path, action: &str) -> anyhow::Result<Option<Snapshot>> {
    if !file.is_file() {
        return Ok(None);
    }
    let session_id = std::env::var("ZNAIDE_SESSION").unwrap_or_else(|_| "local".into());
    backup_for_session(&session_id, file, action)
}

/// 为指定会话生成快照
pub fn backup_for_session(session_id: &str, file: &Path, action: &str) -> anyhow::Result<Option<Snapshot>> {
    if !file.is_file() {
        return Ok(None);
    }
    let ts = now_ms();
    let session_root = files_root().join(sanitize_session(session_id));
    std::fs::create_dir_all(&session_root)?;
    let seq = count_in_session(&session_id);
    let snap_rel = format!("{ts}-{seq:04}.bak");
    let snap_abs = session_root.join(&snap_rel);
    std::fs::copy(file, &snap_abs)?;

    let rec = Snapshot {
        session: sanitize_session(session_id),
        ts_ms: ts,
        orig: file.to_string_lossy().to_string(),
        snap: format!("{}/{snap_rel}", sanitize_session(session_id)),
        action: action.to_string(),
        gen: Some(GEN_TAG.into()),
    };
    let mut all = read_manifest();
    all.push(rec.clone());
    // 保留最近 MAX 条(并清理超出的物理文件)
    while all.len() > MAX_SNAPSHOTS {
        if let Some(old) = all.first() {
            let _ = std::fs::remove_file(files_root().join(&old.snap));
        }
        all.remove(0);
    }
    write_manifest(&all);
    Ok(Some(rec))
}

fn sanitize_session(id: &str) -> String {
    let cleaned: String = id
        .chars()
        .map(|c| if c.is_alphanumeric() || c == '-' || c == '_' { c } else { '_' })
        .collect();
    if cleaned.is_empty() {
        "default".into()
    } else {
        cleaned
    }
}

fn count_in_session(session_id: &str) -> usize {
    let sid = sanitize_session(session_id);
    read_manifest()
        .iter()
        .filter(|s| s.session == sid)
        .count()
}

/// 列出全部快照(新→旧)
pub fn list() -> Vec<Snapshot> {
    let mut all = read_manifest();
    all.reverse();
    all
}

/// 列出某会话内的快照(新→旧)
pub fn list_session(session_id: &str) -> Vec<Snapshot> {
    let sid = sanitize_session(session_id);
    list().into_iter().filter(|s| s.session == sid).collect()
}

/// 回滚指定快照:把快照复制回原位置(会先对当前文件再生成一条快照防误操作)
pub fn rollback(snap: &Snapshot) -> anyhow::Result<()> {
    let snap_abs = files_root().join(&snap.snap);
    if !snap_abs.is_file() {
        anyhow::bail!("快照文件不存在: {}", snap_abs.display());
    }
    let orig = PathBuf::from(&snap.orig);
    if let Some(parent) = orig.parent() {
        std::fs::create_dir_all(parent)?;
    }
    // 回滚前先备份当前状态(防误操作),记为新快照
    let _ = backup_for_session(&snap.session, &orig, &format!("rollback→{}", snap.ts_ms));
    std::fs::copy(&snap_abs, &orig)?;
    Ok(())
}

/// 按序号回滚(index 从 1 开始,1=最新)
pub fn rollback_by_index(index: usize) -> anyhow::Result<Snapshot> {
    let all = list();
    let snap = all
        .get(index.saturating_sub(1))
        .ok_or_else(|| anyhow::anyhow!("序号无效(共 {} 条)", all.len()))?;
    let s = snap.clone();
    rollback(&s)?;
    Ok(s)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 把数据目录指向唯一临时目录,并串行化共享 env/manifest 的测试。
    /// guard 必须存活到测试函数结束(let _g = isolate();)。
    /// 注意顺序:先拿锁再 set_var——若反过来,等待锁的测试会在持锁者
    /// 运行中途改写 env,把持锁者的写入引到别的目录(曾有 flaky)。
    fn isolate() -> std::sync::MutexGuard<'static, ()> {
        let g = crate::test_util::DATA_DIR_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner()); // panic 测试不连锁毒化
        let dir = std::env::temp_dir().join(format!("znaide_undo_ut_{}", std::process::id()));
        std::env::set_var("ZNAIDE_DATA_DIR", &dir);
        g
    }

    #[test]
    fn backup_and_rollback_roundtrip() {
        let _g = isolate();
        let dir = std::env::temp_dir().join(format!("znaide_undo_fs_{}", std::process::id()));
        let f = dir.join("a.txt");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(&f, "v1").unwrap();
        let rec = backup_for_session("test-sess", &f, "write").unwrap().unwrap();
        std::fs::write(&f, "v2").unwrap();
        rollback(&rec).unwrap();
        assert_eq!(std::fs::read_to_string(&f).unwrap(), "v1");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn list_is_newest_first() {
        let _g = isolate();
        let snaps = list();
        for w in snaps.windows(2) {
            assert!(w[0].ts_ms >= w[1].ts_ms);
        }
    }

    #[test]
    fn missing_file_no_backup() {
        let _g = isolate();
        let none = backup(Path::new("/nonexistent/xyz-abc.txt"), "write").unwrap();
        assert!(none.is_none());
    }

    /// 每条新快照的 manifest 记录都带生成器标识(格式演进/排查用)
    #[test]
    fn manifest_records_generator_tag() {
        let _g = isolate();
        let dir = std::env::temp_dir().join(format!("znaide_undo_gen_{}", std::process::id()));
        let f = dir.join("g.txt");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(&f, "x").unwrap();
        backup_for_session("gen-sess", &f, "write").unwrap().unwrap();
        let text = std::fs::read_to_string(manifest_path()).unwrap();
        assert!(text.contains(GEN_TAG), "manifest 应记录生成器标识");
        // 本会话快照字段里 gen 被正确写入(并行测试会共享同一 manifest,不数总数)
        let snaps = read_manifest();
        let mine = snaps
            .iter()
            .find(|s| s.session == "gen-sess")
            .expect("本会话快照应在清单中");
        assert_eq!(mine.gen.as_deref(), Some(GEN_TAG));
        std::fs::remove_dir_all(&dir).ok();
    }

    /// 老格式清单(无 gen 字段)照常读取,gen 为 None——向后兼容
    #[test]
    fn legacy_manifest_without_gen_still_reads() {
        // 独立数据目录:老格式行(ts=1)会破坏 list_is_newest_first 的
        // 降序断言,不能写进共享清单(先锁后设 env,临界区内 var 私有)
        let g = crate::test_util::DATA_DIR_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let dir = std::env::temp_dir().join(format!("znaide_undo_legacy_{}", std::process::id()));
        std::env::set_var("ZNAIDE_DATA_DIR", &dir);
        let path = manifest_path();
        if let Some(p) = path.parent() {
            std::fs::create_dir_all(p).unwrap();
        }
        // 追加老格式行
        use std::io::Write;
        let mut f = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)
            .unwrap();
        writeln!(
            f,
            r#"{{"session":"old","ts_ms":1,"orig":"/a","snap":"old/1-0001.bak","action":"write"}}"#
        )
        .unwrap();
        drop(f);
        let snaps = read_manifest();
        let old = snaps
            .iter()
            .find(|s| s.session == "old")
            .expect("老格式行应能读取");
        assert_eq!(old.orig, "/a");
        assert_eq!(old.gen, None);
        std::fs::remove_dir_all(&dir).ok();
        drop(g);
    }

    /// 外部来源标注:本版本 gen / 无 gen(老快照)不算;异源 gen 才算
    #[test]
    fn external_marker_flags_foreign_gen() {
        let mut s = Snapshot {
            session: "x".into(),
            ts_ms: 1,
            orig: "/a".into(),
            snap: "x/1-0001.bak".into(),
            action: "write".into(),
            gen: Some(GEN_TAG.into()),
        };
        assert!(!s.is_external(), "本版本写入不算外部");
        s.gen = Some("deadbeef00".into());
        assert!(s.is_external(), "异源 gen 应标注外部");
        s.gen = None;
        assert!(!s.is_external(), "老快照(无 gen)不算外部");
    }
}
