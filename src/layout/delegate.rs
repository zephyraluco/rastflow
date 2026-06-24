/// 启动器列表代理：数据过滤、选中状态、启动逻辑

use gpui::*;
use gpui_component::{
    list::{ListDelegate, ListItem, ListState},
    *,
};
use std::process::Command;

use crate::config::{
    entry_identity, load_or_discover_entries, save_entries,
    sort_entries_by_launch, AppEntry,
};
use crate::locale::t;
use crate::utils::hide_window;

// ---------- 平台启动 ----------

#[cfg(windows)]
pub(super) fn launch_entry(entry: &AppEntry) -> std::io::Result<()> {
    use std::os::windows::process::CommandExt;
    const CREATE_NO_WINDOW: u32 = 0x08000000;

    let mut cmd = Command::new("cmd");
    cmd.args(["/C", "start", ""]);

    if let Some(target) = &entry.launch_target {
        cmd.arg(target);
    } else {
        cmd.arg(entry.name.to_string());
    }

    cmd.creation_flags(CREATE_NO_WINDOW).spawn().map(|_| ())
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
}

impl LauncherDelegate {
    pub fn new() -> Self {
        let all_entries = load_or_discover_entries();
        let filtered = all_entries.clone();
        Self {
            all_entries,
            filtered,
            selected_index: Some(IndexPath::default()),
        }
    }

    pub fn reload(&mut self, query: &str) {
        self.all_entries = load_or_discover_entries();
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
        let launched_key = entry_identity(launched);

        if let Some(item) = self
            .all_entries
            .iter_mut()
            .find(|e| entry_identity(e) == launched_key)
        {
            item.launch_count += 1;
        }

        if let Some(item) = self
            .filtered
            .iter_mut()
            .find(|e| entry_identity(e) == launched_key)
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

        save_entries(&self.all_entries);
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
        let entry = self.filtered.get(ix.row)?;
        let selected = Some(ix) == self.selected_index;

        let first_char = entry
            .name
            .chars()
            .next()
            .map(|c| c.to_ascii_uppercase())
            .unwrap_or('A')
            .to_string();

        let name = entry.name.clone();
        let category = entry.category.clone();

        let icon_bg = cx.theme().accent;
        let icon_fg = cx.theme().accent_foreground;
        let muted_fg = cx.theme().muted_foreground;
        let muted_bg = cx.theme().muted;

        Some(
            ListItem::new(ix.row)
                .selected(selected)
                .child(
                    h_flex()
                        .gap_3()
                        .items_center()
                        .px_3()
                        .py_1()
                        .child(
                            div()
                                .w_7()
                                .h_7()
                                .rounded_md()
                                .bg(icon_bg)
                                .flex()
                                .items_center()
                                .justify_center()
                                .text_color(icon_fg)
                                .font_bold()
                                .text_sm()
                                .child(first_char),
                        )
                        .child(
                            div()
                                .flex_1()
                                .text_sm()
                                .child(name),
                        )
                        .child(
                            div()
                                .px_2()
                                .py_1()
                                .rounded_md()
                                .bg(muted_bg)
                                .text_xs()
                                .text_color(muted_fg)
                                .child(category),
                        ),
                ),
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
        if let Some(ix) = self.selected_index {
            if let Some(entry) = self.filtered.get(ix.row) {
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
