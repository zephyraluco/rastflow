//! 查询解析与匹配规则。
//!
//! 查询按 `;` 拆成多个关键字，**全部命中**才算匹配（AND）。含 `\` 或 `/` 的关键字
//! 按「路径关键字」处理，去比完整路径；其余只比文件名。大小写不敏感。
//!
//! | 查询形态 | 入口 |
//! | --- | --- |
//! | 纯文件名关键字 | [`SearchQuery::matches_name`]（只比字节，不分配） |
//! | 含路径分隔符的关键字 | [`SearchQuery::matches_parts`]（需要完整路径） |
//!
//! 用哪个由 [`SearchQuery::needs_dir_for_match`] 判定。

use std::fmt;

use super::pathutil::{self, contains_ascii_ignore_case};

/// 一次搜索的全部条件
#[derive(Clone)]
pub struct SearchQuery {
    /// 用户输入的原始串（只用来做长度上限检查）
    pub search_text: String,
    /// `search_text` 按 `;` 拆出的关键字，全部命中才算匹配（AND）
    pub keywords: Vec<String>,
    /// 与 `keywords` 一一对应的小写副本（避免每次比较都重新转换）
    pub keywords_lower: Vec<String>,
    /// 与 `keywords` 一一对应：这个关键字是「路径关键字」还是「文件名关键字」
    pub is_keyword_path: Vec<bool>,
}

impl fmt::Debug for SearchQuery {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SearchQuery")
            .field("search_text", &self.search_text)
            .field("keywords", &self.keywords)
            .field("is_keyword_path", &self.is_keyword_path)
            .finish()
    }
}

impl SearchQuery {
    /// 按关键字构造查询（大小写不敏感）
    pub fn parse(search_text: &str) -> Self {
        let text = search_text.trim().to_string();
        let mut keywords = Vec::new();
        let mut keywords_lower = Vec::new();
        let mut is_keyword_path = Vec::new();
        for raw in text.split(';') {
            let keyword = raw.trim();
            if keyword.is_empty() {
                continue;
            }
            keywords.push(keyword.to_string());
            keywords_lower.push(keyword.to_lowercase());
            // 含路径分隔符的关键字按「路径关键字」处理，去比父路径。
            // 口径取最直观的一个：含分隔符即路径。
            is_keyword_path.push(keyword.contains('\\') || keyword.contains('/'));
        }

        Self {
            search_text: text,
            keywords,
            keywords_lower,
            is_keyword_path,
        }
    }

    /// 没有任何有效关键字
    pub fn is_empty(&self) -> bool {
        self.keywords.is_empty()
    }

    /// 匹配时是否需要知道目录 —— 只有含 `\` 或 `/` 的路径关键字需要（它要比完整路径），
    /// 其余只比文件名。
    ///
    /// 返回 `false` 时调用方应当走 [`SearchQuery::matches_name`]。
    pub fn needs_dir_for_match(&self) -> bool {
        self.is_keyword_path.iter().any(|is_path| *is_path)
    }

    /// 只用文件名判定是否命中 —— 扫描内存索引的**热路径**。
    ///
    /// 前提：[`SearchQuery::needs_dir_for_match`] 为 `false`，否则必须改用
    /// [`SearchQuery::matches_parts`]。全程不分配内存、不校验 UTF-8。
    pub fn matches_name(&self, name: &[u8]) -> bool {
        debug_assert!(
            !self.needs_dir_for_match(),
            "含路径关键字的查询必须用 matches_parts"
        );
        (0..self.keywords.len()).all(|index| self.keyword_hits(name, index))
    }

    /// 第 `index` 个关键字是否命中 `name`
    fn keyword_hits(&self, name: &[u8], index: usize) -> bool {
        let keyword = &self.keywords[index];
        if keyword.is_ascii() {
            // 快路径：关键字全是 ASCII，直接在字节上比，不分配
            contains_ascii_ignore_case(name, keyword.as_bytes())
        } else {
            // 关键字含非 ASCII：需要 Unicode 大小写折叠，只能走慢路径。
            // 名字不是合法 UTF-8 就直接不算命中。
            let Ok(text) = std::str::from_utf8(name) else {
                return false;
            };
            text.to_lowercase().contains(&self.keywords_lower[index])
        }
    }

    /// 判断一个候选是否命中（候选以完整路径给出）
    ///
    /// 需要完整路径；能用 [`SearchQuery::matches_name`] 时应当优先用它。
    pub fn matches(&self, path: &str) -> bool {
        self.matches_parts(pathutil::file_name(path), pathutil::parent_path(path))
    }

    /// 判断一个候选是否命中，候选以 `(文件名, 目录)` 的形式给出。
    ///
    /// 会为每个候选拼出完整路径并做 Unicode 小写化 —— 只适合「含路径关键字的查询」
    /// 与「目录预扫」这类候选量不大的场景，**不要**拿它去扫整个内存索引。
    pub fn matches_parts(&self, name: &str, dir: &str) -> bool {
        !self.not_matched_parts(name, dir)
    }

    /// 逐个关键字判断，任一不命中就返回 `true`（语义是「不匹配」）
    fn not_matched_parts(&self, name: &str, dir: &str) -> bool {
        for index in 0..self.keywords.len() {
            // 文件名关键字只比文件名，路径关键字比完整路径（这样
            // `系统目录\文件.exe` 这种跨目录/文件边界的写法也能命中）
            let owned;
            let target = if self.is_keyword_path[index] {
                owned = join_path(dir, name);
                owned.as_str()
            } else {
                name
            };

            if !target.to_lowercase().contains(&self.keywords_lower[index]) {
                return true;
            }
        }
        false
    }
}

/// 拼出完整路径（`dir` 可能已带结尾分隔符，比如 `C:\`）
fn join_path(dir: &str, name: &str) -> String {
    if dir.is_empty() {
        return name.to_string();
    }
    if dir.ends_with('\\') || dir.ends_with('/') {
        format!("{dir}{name}")
    } else {
        format!("{dir}\\{name}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::search::priority::{DIR_PRIORITY, PriorityTable};

    fn query(text: &str) -> SearchQuery {
        SearchQuery::parse(text)
    }

    #[test]
    fn splits_keywords_on_semicolon() {
        let q = query("abc; def");
        assert_eq!(q.keywords, vec!["abc", "def"]);
        assert_eq!(q.keywords_lower, vec!["abc", "def"]);
        assert!(!q.is_empty());
    }

    #[test]
    fn blank_keywords_are_dropped() {
        // 空关键字被丢掉；全是空的就等于没条件
        let q = query(";;");
        assert!(q.keywords.is_empty());
        assert!(q.is_empty());
    }

    #[test]
    fn separator_keyword_matches_whole_path() {
        let q = query(r"windows\system");
        assert_eq!(q.is_keyword_path, vec![true]);
        assert_eq!(q.keywords_lower, vec![r"windows\system"]);
        assert!(q.needs_dir_for_match());
    }

    #[test]
    fn multiple_keywords_are_and_semantics() {
        // "note pad" 是一个关键字（含空格），所以不命中 notepad.exe
        assert!(!query("note pad").matches(r"C:\a\notepad.exe"));
        // 分号才是分隔符
        assert!(query("note;pad").matches(r"C:\a\notepad.exe"));
    }

    #[test]
    fn matching_is_case_insensitive() {
        assert!(query("NOTEPAD").matches(r"C:\Windows\notepad.exe"));
        assert!(query("notepad").matches(r"C:\Windows\NOTEPAD.EXE"));
    }

    #[test]
    fn compares_file_name_not_directory_name() {
        // 关键字只与文件名比较，所以目录名命中不算
        assert!(!query("windows").matches(r"C:\Windows\notepad.exe"));
        // 路径关键字才会比父目录
        assert!(query(r"windows\notepad").matches(r"C:\Windows\notepad.exe"));
    }

    #[test]
    fn empty_query_matches_everything() {
        let q = query("");
        assert!(q.is_empty());
        assert!(q.matches(r"C:\anything"));
    }

    #[test]
    fn fast_and_slow_paths_agree() {
        // `matches_name` 是内存索引扫描的热路径，`matches_parts` 是「含路径关键字」
        // 时才走的那条。两者对**纯文件名关键字**必须给出一致结论，
        // 否则同一句话换个写法结果就变了。
        let cases = [
            ("notepad", r"C:\Windows\notepad.exe"),
            ("NOTEPAD", r"C:\Windows\notepad.exe"),
            ("not", r"C:\Windows\notepad.exe"),
            ("xyz", r"C:\Windows\notepad.exe"),
            ("abc.txt", r"C:\a\abc.txt"),
            ("中", r"C:\a\中文文档.txt"),
            ("中文", r"C:\a\中文文档.txt"),
            ("文", r"C:\a\中文文档.txt"),
            ("档", r"C:\a\中文文档.txt"),
            ("ZZ", r"C:\a\中文文档.txt"),
        ];
        for (keyword, path) in cases {
            let plain = query(keyword);
            assert!(!plain.needs_dir_for_match());
            let name = pathutil::file_name(path);
            let fast = plain.matches_name(name.as_bytes());
            let slow = plain.matches_parts(name, pathutil::parent_path(path));
            assert_eq!(fast, slow, "关键字 {keyword:?} 在 {path} 上结论不一致");
        }
    }

    #[test]
    fn fast_path_does_not_false_match_multibyte() {
        // 中文的 UTF-8 字节都 ≥ 0x80，不可能等于任何 ASCII 字节，
        // 所以「在字节上做 ASCII 关键字比较」对非 ASCII 名字也是安全的
        assert!(query("abc").matches_name("中abce.txt".as_bytes()));
        assert!(!query("abc").matches_name("中文文档.txt".as_bytes()));
        assert!(!query("AD").matches_name("中文".as_bytes()));
    }

    #[test]
    fn case_insensitive_holds_on_fast_path() {
        assert!(query("NOTEPAD").matches_name(b"notepad.exe"));
        assert!(query("notepad").matches_name(b"Notepad.exe"));
    }

    #[test]
    fn multiple_keywords_are_and_on_fast_path() {
        assert!(query("note;pad").matches_name(b"notepad.exe"));
        // 缺一个关键字就不该命中
        assert!(!query("note;pad").matches_name(b"note-only.exe"));
        assert!(!query("note;pad").matches_name(b"pad-only.exe"));
    }

    #[test]
    fn path_keyword_requires_dir_aware_matching() {
        let q = query(r"aaa\block.txt");
        assert!(q.needs_dir_for_match());
        assert!(q.matches(r"C:\ppp\aaa\block.txt"));
        assert!(!q.matches(r"C:\ppp\aaa\block.bak"));
        // 关键字跨「目录名 / 文件名」边界的写法也要能命中
        let q = query(r"aaa\block");
        assert!(q.matches_parts("block.txt", r"C:\ppp\aaa"));
        assert!(!q.matches_parts("block.txt", r"C:\ppp\zzz"));
    }

    #[test]
    fn dir_priority_is_not_a_file_tier() {
        let table = PriorityTable::with_builtin_defaults();
        let priorities = table.priorities();
        assert!(priorities.len() >= 2);
        assert!(!priorities.contains(&DIR_PRIORITY));
        // 降序：程序类在前
        assert!(priorities[0] > *priorities.last().unwrap());
    }
}
