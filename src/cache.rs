//! 有界图标缓存；失败短期缓存，避免每帧反复查询 Shell。
use std::collections::{HashMap, HashSet};
use std::time::{Duration, Instant};
const CAPACITY: usize = 512;
const MAX_PENDING: usize = 32;
const FAILURE_TTL: Duration = Duration::from_secs(30);

pub struct IconCache<T> {
    icons: HashMap<String, (Option<T>, Instant)>,
    pending: HashSet<String>,
}
impl<T> Default for IconCache<T> {
    fn default() -> Self { Self { icons: HashMap::new(), pending: HashSet::new() } }
}
impl<T: Clone> IconCache<T> {
    pub fn get(&self, target: Option<&str>) -> Option<T> { self.icons.get(target?).and_then(|(icon,_)| icon.clone()) }
    pub fn request(&mut self, target: &str) -> bool { self.request_at(target,Instant::now()) }
    fn request_at(&mut self, target: &str, now: Instant) -> bool {
        if let Some((icon,stored)) = self.icons.get(target) {
            if icon.is_some() || now.saturating_duration_since(*stored) < FAILURE_TTL { return false; }
            self.icons.remove(target);
        }
        if self.pending.len() >= MAX_PENDING { return false; }
        self.pending.insert(target.into())
    }
    pub fn insert(&mut self, loaded: Vec<(String,Option<T>)>) {
        for (target,icon) in loaded {
            self.pending.remove(&target);
            if !self.icons.contains_key(&target) && self.icons.len() >= CAPACITY {
                if let Some(oldest) = self.icons.iter().min_by_key(|(_,(_,time))| *time).map(|(key,_)| key.clone()) { self.icons.remove(&oldest); }
            }
            self.icons.insert(target,(icon,Instant::now()));
        }
    }
    pub fn invalidate(&mut self) { self.icons.clear(); }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn failure_expires_pending_deduplicates_and_queues_are_bounded() {
        let mut cache=IconCache::<String>::default();
        assert!(cache.request("a")); assert!(!cache.request("a"));
        cache.insert(vec![("a".into(),None)]);
        assert!(!cache.request("a"));
        assert!(cache.request_at("a",Instant::now()+FAILURE_TTL));
        for i in 0..100 { cache.request(&i.to_string()); }
        assert_eq!(cache.pending.len(),MAX_PENDING);
        for i in 0..1000 { cache.insert(vec![(i.to_string(),Some(i.to_string()))]); }
        assert_eq!(cache.icons.len(),CAPACITY);
        assert_eq!(cache.get(Some("999")),Some("999".into()));
    }
}
