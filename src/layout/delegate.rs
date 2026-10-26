//! 启动器列表代理：数据过滤、选中状态、启动逻辑

use gpui::*;
use gpui_kit::component::{
    list::{ListDelegate, ListItem, ListState},
    *,
};
use std::sync::Arc;

use crate::app_list::{
    record_launch,
    sort_entries_by_launch, AppEntry,
};
use crate::components::app_list_item::app_row;
use crate::locale::t;
use crate::utils::hide_window;

use super::icon_cache::IconCache;

// ---------- 平台启动 ----------

#[cfg(windows)]
pub(super) fn launch_entry(entry: &AppEntry) -> std::io::Result<()> {
    crate::platform::open_target(entry.launch_target.as_deref().unwrap_or(entry.name.as_ref()))
}

#[cfg(not(windows))]
pub(super) fn launch_entry(_entry: &AppEntry) -> std::io::Result<()> {
    Err(std::io::Error::new(
        std::io::ErrorKind::Unsupported,
        "当前平台尚未实现程序启动",
    ))
}

// ---------- List Delegate ----------

pub struct LauncherDelegate {
    pub(super) all_entries: Vec<AppEntry>,
    pub(super) filtered: Vec<AppEntry>,
    pub(super) selected_index: Option<IndexPath>,
    /// 图标缓存与后台请求排队（详见 [`super::icon_cache`]）
    icons: IconCache,
}

impl LauncherDelegate {
    pub fn new() -> Self {
        let all_entries = Vec::new();
        let filtered = all_entries.clone();
        Self {
            all_entries,
            filtered,
            selected_index: Some(IndexPath::default()),
            icons: IconCache::default(),
        }
    }

    /// 查图标缓存。渲染路径上只能内存查表，真正的提取在后台（见 [`super::icon_cache`]）。
    pub fn icon(&self, launch_target: Option<&str>) -> Option<Arc<RenderImage>> {
        self.icons.get(launch_target)
    }

    /// 回填后台提取好的图标（`None` 表示取不到，同样会被缓存）。
    pub fn insert_icons(&mut self, loaded: Vec<(String, Option<Arc<RenderImage>>)>) {
        self.icons.insert(loaded);
    }

    pub fn reload(&mut self, query: &str) {
        self.filter(query);
    }

    pub fn replace_entries(&mut self, entries: Vec<AppEntry>, query: &str) {
        self.icons.invalidate();
        self.all_entries = entries;
        self.filter(query);
    }

    pub fn filter(&mut self, query: &str) {
        let q = query.to_lowercase();
        self.filtered = if q.is_empty() {
            self.all_entries.clone()
        } else {
            self.all_entries
                .iter()
                .filter(|e| {
                    e.name.to_lowercase().contains(&q)
                        || e.category.to_lowercase().contains(&q)
                })
                .cloned()
                .collect()
        };
        self.selected_index = if self.filtered.is_empty() {
            None
        } else {
            Some(IndexPath::default())
        };
    }

    pub(super) fn mark_launched(&mut self, launched: &AppEntry) {
        let launched_id = &launched.id;

        if let Some(item) = self
            .all_entries
            .iter_mut()
            .find(|e| e.id == *launched_id)
        {
            item.launch_count += 1;
        }

        if let Some(item) = self
            .filtered
            .iter_mut()
            .find(|e| e.id == *launched_id)
        {
            item.launch_count += 1;
        }

        sort_entries_by_launch(&mut self.all_entries);
        sort_entries_by_launch(&mut self.filtered);
        self.selected_index = if self.filtered.is_empty() {
            None
        } else {
            Some(IndexPath::default())
        };

        record_launch(launched.id.as_ref());
    }

    /// 计算下一个选中 index（纯函数，不修改自身状态）
    pub fn navigate_selection(&self, forward: bool) -> Option<IndexPath> {
        let len = self.filtered.len();
        if len == 0 {
            return None;
        }
        Some(match self.selected_index {
            None => IndexPath::default(),
            Some(ix) => {
                if forward {
                    IndexPath { section: 0, row: (ix.row + 1).min(len - 1), column: 0 }
                } else {
                    IndexPath { section: 0, row: ix.row.saturating_sub(1), column: 0 }
                }
            }
        })
    }
}

impl ListDelegate for LauncherDelegate {
    type Item = ListItem;

    fn items_count(&self, _section: usize, _cx: &App) -> usize {
        self.filtered.len()
    }

    fn render_item(
        &mut self,
        ix: IndexPath,
        _window: &mut Window,
        cx: &mut Context<ListState<Self>>,
    ) -> Option<Self::Item> {
        let (name, category, launch_target) = {
            let entry = self.filtered.get(ix.row)?;
            (
                entry.name.clone(),
                entry.category.clone(),
                entry.launch_target.clone(),
            )
        };
        let selected = Some(ix) == self.selected_index;
        let icon = self.icon(launch_target.as_deref());
        if let Some(target) = launch_target.filter(|target| self.icons.request(target)) {
            cx.spawn(async move |this, cx| {
                let icon = crate::app_icon::load_async(target.clone()).await;
                let _ = this.update(cx, |state, cx| {
                    state.delegate_mut().insert_icons(vec![(target,icon)]);
                    cx.notify();
                });
            }).detach();
        }

        Some(
            ListItem::new(ix.row)
                .selected(selected)
                .child(app_row(icon, &name, &category, cx)),
        )
    }

    fn set_selected_index(
        &mut self,
        ix: Option<IndexPath>,
        _window: &mut Window,
        cx: &mut Context<ListState<Self>>,
    ) {
        if self.selected_index == ix {
            return;
        }
        self.selected_index = ix;
        cx.notify();
    }

    fn confirm(
        &mut self,
        _secondary: bool,
        window: &mut Window,
        cx: &mut Context<ListState<Self>>,
    ) {
        if let Some(ix) = self.selected_index
            && let Some(entry) = self.filtered.get(ix.row)
        {
            let launched = entry.clone();
            if let Err(err) = launch_entry(&launched) {
                eprintln!("启动失败 [{}]: {err}", entry.name);
            } else {
                self.mark_launched(&launched);
                cx.notify();
                hide_window(window);
            }
        }
    }

    fn render_empty(
        &mut self,
        _window: &mut Window,
        cx: &mut Context<ListState<Self>>,
    ) -> impl IntoElement {
        div()
            .flex()
            .items_center()
            .justify_center()
            .p_8()
            .child(t("search.empty", cx))
    }
}
