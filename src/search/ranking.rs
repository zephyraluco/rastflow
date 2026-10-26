//! 有界且与索引插入顺序无关的 Top-K。
use std::cmp::Reverse;
use std::collections::{BTreeSet, HashSet};

#[derive(Eq, PartialEq, Ord, PartialOrd)]
struct Candidate {
    quality: u8,
    priority: Reverse<i32>,
    name: String,
    key: String,
    path: String,
}
pub struct TopResults {
    limit: usize,
    candidates: BTreeSet<Candidate>,
    paths: HashSet<String>,
}
impl TopResults {
    pub fn new(limit: usize) -> Self {
        Self {
            limit,
            candidates: BTreeSet::new(),
            paths: HashSet::new(),
        }
    }
    pub fn insert(&mut self, quality: u8, priority: i32, path: String) {
        if self.limit == 0 {
            return;
        }
        let key = path.to_lowercase();
        if self.paths.contains(&key) {
            return;
        }
        let candidate = Candidate {
            quality,
            priority: Reverse(priority),
            name: super::pathutil::file_name(&path).to_lowercase(),
            key: key.clone(),
            path,
        };
        if self.candidates.len() == self.limit
            && self
                .candidates
                .last()
                .is_some_and(|worst| &candidate >= worst)
        {
            return;
        }
        self.paths.insert(key);
        self.candidates.insert(candidate);
        if self.candidates.len() > self.limit
            && let Some(removed) = self.candidates.pop_last() {
                self.paths.remove(&removed.key);
            }
    }
    pub fn into_paths(self) -> Vec<String> {
        self.candidates
            .into_iter()
            .map(|candidate| candidate.path)
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn quality_precedes_extension_preference_and_order_is_stable() {
        let candidates = [
            (2, 30, r"C:\z.exe"),
            (0, 0, r"C:\exact.txt"),
            (1, 0, r"C:\prefix.txt"),
            (2, 0, r"C:\other.txt"),
        ];
        let mut first = TopResults::new(3);
        let mut second = TopResults::new(3);
        for (quality, priority, path) in candidates {
            first.insert(quality, priority, path.into());
        }
        for (quality, priority, path) in candidates.into_iter().rev() {
            second.insert(quality, priority, path.into());
        }
        let result = first.into_paths();
        assert_eq!(result, second.into_paths());
        assert_eq!(result, vec![r"C:\exact.txt", r"C:\prefix.txt", r"C:\z.exe"]);
    }
    #[test]
    fn duplicate_paths_do_not_use_up_result_slots() {
        let mut results = TopResults::new(2);
        results.insert(0, 0, r"C:\A.txt".into());
        results.insert(0, 0, r"c:\a.TXT".into());
        results.insert(0, 0, r"C:\b.txt".into());
        assert_eq!(results.into_paths().len(), 2);
        let mut empty = TopResults::new(0);
        empty.insert(0, 0, "a".into());
        assert!(empty.into_paths().is_empty());
    }
}
