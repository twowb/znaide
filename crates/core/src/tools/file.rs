use super::{deny_to_result, resolve_path, ToolContext, ToolError, ToolOutput};
use serde::Deserialize;
use serde_json::Value;
use tokio::io::AsyncBufReadExt;

#[derive(Debug, Deserialize)]
struct ReadArgs {
    path: String,
    #[serde(default)]
    offset: Option<usize>,
    #[serde(default)]
    limit: Option<usize>,
}

pub async fn read_file(ctx: &ToolContext<'_>, args: &Value) -> Result<ToolOutput, ToolError> {
    let a: ReadArgs = serde_json::from_value(args.clone())?;
    let path = resolve_path(ctx.cwd, &a.path);
    if !path.exists() {
        return Err(ToolError(format!("文件不存在: {}", path.display())));
    }
    // 逐行流式读:只把要展示的行留在内存里(整文件 read_to_string 会让一个几 GB 的
    // 日志直接把进程撑爆),同时数出总行数用于"共 N 行"。
    // limit 来自模型给的 JSON,可能极大 → 用 saturating_add,别让 start + n 溢出成
    // end < start(那会让下面的格式化算出负数区间或整段错位)。
    let start = a.offset.unwrap_or(0);
    let stop = match a.limit {
        Some(n) => start.saturating_add(n),
        None => usize::MAX,
    };
    let f = tokio::fs::File::open(&path)
        .await
        .map_err(|e| ToolError(format!("读取 {} 失败: {e}", path.display())))?;
    let mut lines = tokio::io::BufReader::new(f).lines();
    let mut total = 0usize;
    let mut wanted: Vec<String> = Vec::new();
    while let Some(line) = lines
        .next_line()
        .await
        .map_err(|e| ToolError(format!("读取 {} 失败: {e}", path.display())))?
    {
        if total >= start && total < stop {
            wanted.push(line);
        }
        total += 1;
    }
    let shown_start = start.min(total);
    let end = shown_start + wanted.len();
    let mut out = String::new();
    out.push_str(&format!(
        "文件 {} 共 {total} 行,显示 {}-{}:\n",
        path.display(),
        shown_start + 1,
        end
    ));
    for (i, line) in wanted.iter().enumerate() {
        out.push_str(&format!("{:>6} | {line}\n", shown_start + i + 1));
    }
    Ok(out)
}

#[derive(Debug, Deserialize)]
struct WriteArgs {
    path: String,
    content: String,
}

pub async fn write_file(ctx: &ToolContext<'_>, args: &Value) -> Result<ToolOutput, ToolError> {
    deny_to_result(ctx.permission.check_file_write())?;
    let a: WriteArgs = serde_json::from_value(args.clone())?;
    let path = resolve_path(ctx.cwd, &a.path);
    if let Some(parent) = path.parent() {
        if !parent.as_os_str().is_empty() && !parent.exists() {
            tokio::fs::create_dir_all(parent).await?;
        }
    }
    crate::undo::backup_for_session(ctx.session_id, &path, "write_file")
        .map_err(|e| ToolError(format!("备份失败: {e}")))?;
    tokio::fs::write(&path, &a.content).await.map_err(|e| {
        ToolError(format!("写入 {} 失败: {e}", path.display()))
    })?;
    Ok(format!(
        "已写入 {} ({} 字节)",
        path.display(),
        a.content.len()
    ))
}

#[derive(Debug, Deserialize)]
struct EditArgs {
    path: String,
    old_string: String,
    new_string: String,
}

pub async fn edit(ctx: &ToolContext<'_>, args: &Value) -> Result<ToolOutput, ToolError> {
    deny_to_result(ctx.permission.check_file_write())?;
    let a: EditArgs = serde_json::from_value(args.clone())?;
    let path = resolve_path(ctx.cwd, &a.path);
    let content = tokio::fs::read_to_string(&path)
        .await
        .map_err(|e| ToolError(format!("读取 {} 失败: {e}", path.display())))?;
    if a.old_string.is_empty() {
        return Err(ToolError("old_string 不能为空".into()));
    }
    let occurrences = content.matches(&a.old_string).count();
    if occurrences == 0 {
        return Err(ToolError(format!(
            "未找到要替换的文本(old_string 不匹配)。请检查文件实际内容后重试,注意精确匹配(含缩进/空白)"
        )));
    }
    if occurrences > 1 {
        return Err(ToolError(format!(
            "old_string 在文件中出现 {occurrences} 次,请扩大上下文使其唯一"
        )));
    }
    crate::undo::backup_for_session(ctx.session_id, &path, "edit")
        .map_err(|e| ToolError(format!("备份失败: {e}")))?;
    let new_content = content.replace(&a.old_string, &a.new_string);
    tokio::fs::write(&path, &new_content).await?;
    Ok(format!(
        "已替换 {} 中的 1 处文本。",
        path.display()
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::permissions::{Mode, Permission};
    use serde_json::json;

    fn test_ctx<'a>(dir: &'a std::path::Path, perm: &'a Permission) -> ToolContext<'a> {
        ToolContext {
            cwd: dir,
            permission: perm,
            session_id: "ut",
            cancel: None,
            events: None,
        }
    }

    /// read_file 的 offset/limit 边界:分段、越界、以及**极大的 limit**
    /// (以前 `start + n` 用 usize 加法,模型传 18446744073709551615 会回绕成 end < start
    ///  直接把整段 panic 掉)
    #[tokio::test]
    async fn read_file_slices_and_survives_huge_limit() {
        let dir = std::env::temp_dir().join(format!("znaide_read_ut_{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let f = dir.join("t.txt");
        std::fs::write(&f, "l1\nl2\nl3\nl4\nl5\n").unwrap();
        let perm = Permission::new(Mode::BypassPermissions);
        let ctx = test_ctx(&dir, &perm);

        // 全量
        let all = read_file(&ctx, &json!({"path": "t.txt"})).await.unwrap();
        assert!(all.contains("共 5 行"), "{all}");
        assert!(all.contains("显示 1-5"), "{all}");

        // 偏移 + 条数
        let part = read_file(&ctx, &json!({"path": "t.txt", "offset": 2, "limit": 2}))
            .await
            .unwrap();
        assert!(part.contains("显示 3-4"), "{part}");
        assert!(part.contains("l3") && part.contains("l4") && !part.contains("l5"), "{part}");

        // offset 越界:空切片,不 panic
        let over = read_file(&ctx, &json!({"path": "t.txt", "offset": 99}))
            .await
            .unwrap();
        assert!(over.contains("显示 6-5"), "{over}");

        // 极大 limit(曾经的溢出点):照常返回全部
        let huge = read_file(
            &ctx,
            &json!({"path": "t.txt", "limit": 18_446_744_073_709_551_615u64}),
        )
        .await
        .unwrap();
        assert!(huge.contains("显示 1-5"), "{huge}");

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 中文/超长行按字符流式读,不整文件读进内存(这里只验证内容与行号正确)
    #[tokio::test]
    async fn read_file_keeps_cjk_lines_intact() {
        let dir = std::env::temp_dir().join(format!("znaide_read_cjk_{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let f = dir.join("c.txt");
        std::fs::write(&f, "第一行\n第二行\n第三行\n").unwrap();
        let perm = Permission::new(Mode::BypassPermissions);
        let ctx = test_ctx(&dir, &perm);
        let out = read_file(&ctx, &json!({"path": "c.txt", "offset": 1, "limit": 1}))
            .await
            .unwrap();
        assert!(out.contains("第二行"), "{out}");
        assert!(out.contains("显示 2-2"), "{out}");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
