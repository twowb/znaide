//! 跨模块共用的小工具(纯函数,无副作用)。

/// 按**字符**(不是字节)截断,避免切坏中文/emoji;只在真的截断时追加 `suffix`。
///
/// 以前这段逻辑在 openai / memory / web / session / tui 各处抄了 5、6 遍,
/// 只有后缀不同,谁改一处就漂一处。后缀交给调用方,实现只有这一份。
/// (顺手少一遍 `chars().count()`:一次迭代就能同时判断"是否超长"和"取前 N 个"。)
pub fn truncate_chars(s: &str, max: usize, suffix: &str) -> String {
    let mut it = s.chars();
    let cut: String = it.by_ref().take(max).collect();
    if it.next().is_some() {
        format!("{cut}{suffix}")
    } else {
        cut
    }
}

#[cfg(test)]
mod tests {
    use super::truncate_chars;

    #[test]
    fn keeps_short_text_untouched() {
        assert_eq!(truncate_chars("abc", 5, "…"), "abc");
        assert_eq!(truncate_chars("abc", 3, "…"), "abc", "正好等于上限不算截断");
        assert_eq!(truncate_chars("", 3, "…"), "");
    }

    #[test]
    fn cuts_on_char_boundary_and_marks() {
        // 中文按字符切:3 个汉字 + 后缀(不会切出半个 UTF-8)
        assert_eq!(truncate_chars("中文测试文本", 3, "…"), "中文测…");
        assert_eq!(truncate_chars("中文测试文本", 3, "\n…(已截断)"), "中文测\n…(已截断)");
        // emoji(多字节)同样按字符算
        assert_eq!(truncate_chars("😀😀😀😀", 2, "…"), "😀😀…");
    }
}
