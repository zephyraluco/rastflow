//! 路径纯函数。

/// 取文件名（最后一段）。会先去掉尾部分隔符，所以 `C:\a\b\` 得到 `b`
pub fn file_name(path: &str) -> &str {
    let trimmed = path.trim_end_matches(['\\', '/']);
    match trimmed.rfind(['\\', '/']) {
        Some(index) => &trimmed[index + 1..],
        None => trimmed,
    }
}

/// 取父目录（去掉最后一段）。`C:\a\b.txt` → `C:\a`
pub fn parent_path(path: &str) -> &str {
    let trimmed = path.trim_end_matches(['\\', '/']);
    match trimmed.rfind(['\\', '/']) {
        Some(index) => &trimmed[..index],
        None => "",
    }
}

/// 取后缀名（不含点），借用且不做小写化
pub fn suffix_str(path: &str) -> &str {
    let name = file_name(path);
    match name.rfind('.') {
        // 排除 `.gitignore` 这种「点在开头」以及结尾就是点的情况
        Some(index) if index > 0 && index + 1 < name.len() => &name[index + 1..],
        _ => "",
    }
}

/// 字节级「包含」判断，ASCII 大小写不敏感（非 ASCII 字节要求逐位相等）
pub fn contains_ascii_ignore_case(hay: &[u8], needle: &[u8]) -> bool {
    if needle.is_empty() {
        return true;
    }
    if needle.len() > hay.len() {
        return false;
    }
    let first = needle[0];
    let limit = hay.len() - needle.len();
    let mut index = 0;
    while index <= limit {
        // 先比首字节，命中再比整段 —— 绝大多数位置会在这里被快速排除
        if hay[index].eq_ignore_ascii_case(&first)
            && hay[index..index + needle.len()]
                .iter()
                .zip(needle)
                .all(|(h, n)| h.eq_ignore_ascii_case(n))
        {
            return true;
        }
        index += 1;
    }
    false
}

/// 路径是否落在回收站里
pub fn is_recycle_bin(path: &str) -> bool {
    contains_ascii_ignore_case(path.as_bytes(), b"$recycle.bin")
}

/// 忽略目录规则（需预先构造）。
///
/// 命中就忽略**整棵子树**，且认目录边界：`C:\Windows` 不包含 `C:\WindowsApps`。
#[derive(Debug, Clone, Default)]
pub struct IgnoreRules {
    /// 已去掉尾部分隔符的前缀（保留原大小写，比较时忽略）
    prefixes: Vec<String>,
}

impl IgnoreRules {
    pub fn new(paths: &[String]) -> Self {
        let prefixes = paths
            .iter()
            .map(|path| path.trim_end_matches(['\\', '/']).to_string())
            .filter(|path| !path.is_empty())
            .collect();
        Self { prefixes }
    }

    pub fn is_empty(&self) -> bool {
        self.prefixes.is_empty()
    }

    /// `path` 是否落在某个被忽略的子树里
    pub fn matches(&self, path: &str) -> bool {
        if self.prefixes.is_empty() {
            return false;
        }
        let path = path.as_bytes();
        // `C:\` 这种根路径截尾后是 `C:`，所以先截掉尾分隔符再比
        let path = match path.iter().rposition(|b| *b != b'\\' && *b != b'/') {
            Some(last) => &path[..=last],
            None => return false,
        };

        for prefix in &self.prefixes {
            let prefix = prefix.as_bytes();
            if prefix.len() > path.len() {
                continue;
            }
            if !path[..prefix.len()].eq_ignore_ascii_case(prefix) {
                continue;
            }
            let rest = &path[prefix.len()..];
            // 完全相同，或者后面紧跟分隔符
            if rest.is_empty() || rest[0] == b'\\' || rest[0] == b'/' {
                return true;
            }
        }
        false
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn file_name_and_parent_path() {
        assert_eq!(file_name(r"C:\Windows\notepad.exe"), "notepad.exe");
        assert_eq!(file_name(r"C:\a\b\"), "b");
        assert_eq!(file_name("just-name.txt"), "just-name.txt");
        assert_eq!(parent_path(r"C:\Windows\notepad.exe"), r"C:\Windows");
        assert_eq!(parent_path(r"C:\a"), "C:");
        assert_eq!(parent_path("bare"), "");
    }

    #[test]
    fn suffix_str_extracts_extension() {
        // 热路径用的版本：拿到的还是原大小写，也不分配
        assert_eq!(suffix_str(r"C:\a\B.TXT"), "TXT");
        assert_eq!(suffix_str(r"C:\a\b.tar.gz"), "gz");
        assert_eq!(suffix_str(r"C:\a\b"), "");
        // 点在开头不算后缀
        assert_eq!(suffix_str(r"C:\a\.gitignore"), "");
        // 结尾是点也不算
        assert_eq!(suffix_str(r"C:\a\b."), "");
    }

    #[test]
    fn ignore_rules_respect_directory_boundary() {
        let rules = IgnoreRules::new(&[
            r"C:\Windows".to_string(),
            r"D:\temp".to_string(),
        ]);
        assert!(rules.matches(r"C:\Windows\System32\a.dll"));
        // 大小写不敏感
        assert!(rules.matches(r"c:\windows\a.dll"));
        // 目录自己也算
        assert!(rules.matches(r"C:\Windows"));
        assert!(rules.matches(r"C:\Windows\"));
        // 前缀相同但不是目录边界 → 不能误伤
        assert!(!rules.matches(r"C:\WindowsApps\a.dll"));
        assert!(!rules.matches(r"C:\Windows.old\a.dll"));
        assert!(!rules.matches(r"C:\Users\a"));
        assert!(rules.matches(r"D:\temp\x\y"));
    }

    #[test]
    fn ignore_rules_handle_empty_and_degenerate_input() {
        assert!(!IgnoreRules::new(&[]).matches(r"C:\anything"));
        assert!(IgnoreRules::new(&[]).is_empty());
        // 空白项与只有分隔符的项要能消化掉
        let rules = IgnoreRules::new(&["".to_string(), "   ".to_string(), "\\".to_string()]);
        assert!(!rules.matches(r"C:\a"));
    }
}
