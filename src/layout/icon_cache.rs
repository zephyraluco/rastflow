/// 应用图标缓存与后台请求排队
///
/// 取图标要调 Shell / GDI，是阻塞且有副作用的操作，必须放在后台线程里做；
/// 而列表是虚拟列表，`render_item` 每帧都会对可见行调用，渲染路径上只能查内存。
///
/// 这个模块只负责「谁已经取过、谁正在取、下一批该取谁」这三件事，
/// 不自己起线程，也不碰 GPU —— 让调用方决定何时在后台执行。
use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use gpui::RenderImage;

#[derive(Default)]
pub struct IconCache {
    /// 已取完的图标，key 为启动目标路径。
    /// 取不到时存 `None`：失败也是结论，不能反复重试。
    icons: HashMap<String, Option<Arc<RenderImage>>>,
    /// 已交给后台、还没回填的目标，避免同一批里重复请求。
    pending: HashSet<String>,
}

impl IconCache {
    /// 查缓存。没有启动目标（例如内置示例条目）或尚未取到都返回 `None`。
    pub fn get(&self, target: Option<&str>) -> Option<Arc<RenderImage>> {
        self.icons.get(target?).cloned().flatten()
    }

    /// 申请提取一个图标。
    ///
    /// 返回 `true` 表示这是一个新请求，调用方应把它交给后台线程；
    /// 已经缓存过或已经在请求中的返回 `false`。
    pub fn request(&mut self, target: &str) -> bool {
        if self.icons.contains_key(target) {
            return false;
        }
        self.pending.insert(target.to_string())
    }

    /// 回填后台取好的图标（`None` 表示取不到）。
    pub fn insert(&mut self, loaded: Vec<(String, Option<Arc<RenderImage>>)>) {
        for (target, icon) in loaded {
            self.pending.remove(&target);
            self.icons.insert(target, icon);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn requests_new_targets_only_once() {
        let mut cache = IconCache::default();

        assert!(cache.request(r"C:\a.exe"), "新目标应被接受");
        assert!(!cache.request(r"C:\a.exe"), "同一批内不应重复请求");
        assert!(!cache.get(Some(r"C:\a.exe")).is_some());

        // 回填后不再请求
        cache.insert(vec![(r"C:\a.exe".to_string(), None)]);
        assert!(!cache.request(r"C:\a.exe"), "回填后不应再请求");
    }

    #[test]
    fn failed_extraction_is_cached_as_missing() {
        let mut cache = IconCache::default();
        cache.insert(vec![(r"C:\missing.exe".to_string(), None)]);

        assert!(cache.get(Some(r"C:\missing.exe")).is_none());
        assert!(!cache.request(r"C:\missing.exe"), "失败的结果也应被记住");
    }

    #[test]
    fn entries_without_target_never_match() {
        let cache = IconCache::default();
        assert!(cache.get(None).is_none());
        assert!(cache.get(Some(r"C:\never-asked.exe")).is_none());
    }
}
