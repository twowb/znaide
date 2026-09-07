use super::{deny_to_result, resolve_path, ToolContext, ToolError, ToolOutput};
use serde::Deserialize;
use serde_json::Value;

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
    let content = tokio::fs::read_to_string(&path).await.map_err(|e| {
        ToolError(format!("读取 {} 失败: {e}", path.display()))
    })?;
    let lines: Vec<&str> = content.lines().collect();
    let total = lines.len();
    let start = a.offset.unwrap_or(0).min(total);
    let end = match a.limit {
        Some(n) => (start + n).min(total),
        None => total,
    };
    let slice = &lines[start..end];
    let mut out = String::new();
    out.push_str(&format!("文件 {} 共 {total} 行,显示 {}-{}:\n", path.display(), start + 1, end));
    for (i, line) in slice.iter().enumerate() {
        out.push_str(&format!("{:>6} | {line}\n", start + i + 1));
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
