//! 后缀优先级表：`后缀 → 优先级`。数值越大越先搜；目录恒为 [`DIR_PRIORITY`]（-1）。
//!
//! 默认表：可执行文件与快捷方式先出，脚本类次之。

use std::collections::HashMap;

/// 目录的固定优先级
pub const DIR_PRIORITY: i32 = -1;

/// 其余文件在没有匹配到后缀规则时使用的优先级
pub const DEFAULT_PRIORITY: i32 = 0;

/// 后缀 → 优先级
#[derive(Debug, Clone, Default)]
pub struct PriorityTable {
    map: HashMap<String, i32>,
}

impl PriorityTable {
    /// 内置默认表（数值只有相对意义，见模块说明）
    pub fn with_builtin_defaults() -> Self {
        let mut table = Self::default();
        for (suffix, priority) in [
            ("exe", 30),
            ("lnk", 30),
            ("url", 20),
            ("bat", 10),
            ("cmd", 10),
            ("com", 10),
            ("ps1", 10),
            ("msi", 10),
            ("appref-ms", 10),
        ] {
            table.map.insert(suffix.to_string(), priority);
        }
        table
    }

    /// 从键值对构造（读快照时的入口）
    pub fn from_pairs<I, S>(pairs: I) -> Self
    where
        I: IntoIterator<Item = (S, i32)>,
        S: Into<String>,
    {
        Self {
            map: pairs.into_iter().map(|(k, v)| (k.into(), v)).collect(),
        }
    }

    /// 某个后缀的优先级
    pub fn of_suffix(&self, suffix: &str) -> i32 {
        if suffix.is_empty() {
            return DEFAULT_PRIORITY;
        }
        let lowered = suffix.to_ascii_lowercase();
        self.map.get(&lowered).copied().unwrap_or(DEFAULT_PRIORITY)
    }

    /// 所有出现过的优先级，**降序**（先搜高优先级）
    ///
    /// 即使表是空的也要返回一个 [`DEFAULT_PRIORITY`] —— 它同时是结果分档的兑底档，
    /// 少了它「没命中后缀规则的文件」就没地方归。
    pub fn priorities(&self) -> Vec<i32> {
        let mut values: Vec<i32> = self.map.values().copied().collect();
        if !values.contains(&DEFAULT_PRIORITY) {
            values.push(DEFAULT_PRIORITY);
        }
        values.sort_unstable_by(|a, b| b.cmp(a));
        values.dedup();
        values
    }

    /// 遍历所有规则（写进快照时用）
    pub fn iter(&self) -> impl Iterator<Item = (&str, i32)> {
        self.map.iter().map(|(k, v)| (k.as_str(), *v))
    }

    /// 是否为空表
    pub fn is_empty(&self) -> bool {
        self.map.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::search::pathutil;

    /// 按**生产路径**取优先级：先抽后缀，再查表（顺带测到 `suffix_str`）
    fn priority_of_path(table: &PriorityTable, path: &str) -> i32 {
        table.of_suffix(pathutil::suffix_str(path))
    }

    #[test]
    fn empty_table_still_yields_one_tier() {
        // 空表也要给出一档，不然「没命中后缀规则的文件」无处可归
        let table = PriorityTable::default();
        assert!(table.is_empty());
        assert_eq!(priority_of_path(&table, r"C:\a\b.exe"), DEFAULT_PRIORITY);
        assert_eq!(priority_of_path(&table, r"C:\a\b"), DEFAULT_PRIORITY);
        assert_eq!(table.priorities(), vec![DEFAULT_PRIORITY]);
    }

    #[test]
    fn builtin_defaults_rank_executables_first() {
        let table = PriorityTable::with_builtin_defaults();
        assert!(
            priority_of_path(&table, r"C:\a\b.exe") > priority_of_path(&table, r"C:\a\b.txt")
        );
        assert_eq!(
            priority_of_path(&table, r"C:\a\b.lnk"),
            priority_of_path(&table, r"C:\a\b.exe")
        );
        // 后缀大小写不敏感
        assert_eq!(
            priority_of_path(&table, r"C:\a\B.EXE"),
            priority_of_path(&table, r"C:\a\b.exe")
        );
        // 未知后缀落在 DEFAULT_PRIORITY
        assert_eq!(
            priority_of_path(&table, r"C:\a\b.unknown"),
            DEFAULT_PRIORITY
        );
    }

    #[test]
    fn priorities_are_descending_and_include_default() {
        let table = PriorityTable::with_builtin_defaults();
        let list = table.priorities();
        assert_eq!(list[0], 30);
        assert!(list.contains(&DEFAULT_PRIORITY));
        // 已降序
        assert!(list.windows(2).all(|w| w[0] > w[1]));
        // 目录优先级不参与文件任务矩阵
        assert!(!list.contains(&DIR_PRIORITY));
    }

    #[test]
    fn from_pairs_and_iter_roundtrip() {
        let table = PriorityTable::from_pairs([("exe", 5), ("txt", 1)]);
        assert_eq!(table.of_suffix("exe"), 5);
        assert_eq!(table.of_suffix("txt"), 1);
        let mut pairs: Vec<_> = table.iter().collect();
        pairs.sort_unstable();
        assert_eq!(pairs, vec![("exe", 5), ("txt", 1)]);
    }

    #[test]
    fn empty_suffix_gets_default_priority() {
        let table = PriorityTable::with_builtin_defaults();
        // 没有后缀的文件（或名为 ".gitignore"）落默认优先级
        assert_eq!(priority_of_path(&table, r"C:\a\b"), DEFAULT_PRIORITY);
        assert_eq!(priority_of_path(&table, r"C:\a\.gitignore"), DEFAULT_PRIORITY);
    }
}
