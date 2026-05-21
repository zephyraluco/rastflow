use gpui::*;
use gpui_component::{
    button::{Button, ButtonVariants},
    input::{Escape, Input, InputEvent, InputState, MoveDown, MoveUp},
    list::{List, ListDelegate, ListItem, ListState},
    *,
};
use std::process::Command;
use std::time::Instant;

use crate::config::{
    entry_identity, load_or_discover_entries, save_entries,
    sort_entries_by_launch, AppEntry,
};
use crate::settings::{AppSettings, SettingsView};
use crate::utils::{center_window, hide_window};
use crate::locale::t;

#[cfg(windows)]
fn launch_entry(entry: &AppEntry) -> std::io::Result<()> {
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
fn launch_entry(_entry: &AppEntry) -> std::io::Result<()> {
    Err(std::io::Error::new(
        std::io::ErrorKind::Unsupported,
        "当前平台尚未实现程序启动",
    ))
}

// ---------- List Delegate ----------

pub struct LauncherDelegate {
    all_entries: Vec<AppEntry>,
    filtered: Vec<AppEntry>,
    selected_index: Option<IndexPath>,
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

    fn mark_launched(&mut self, launched: &AppEntry) {
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

// ---------- 启动器主视图 ----------

pub struct LauncherView {
    input_state: Entity<InputState>,
    list_state: Entity<ListState<LauncherDelegate>>,
    /// 鼠标按下的时刻，用于判断长按拖动
    drag_start: Option<Instant>,
    _subscriptions: Vec<Subscription>,
}

impl LauncherView {
    pub fn new(window: &mut Window, cx: &mut Context<Self>) -> Self {
        let input_state = cx.new(|cx| {
            InputState::new(window, cx).placeholder(t("search.placeholder", cx))
        });

        let list_state = cx.new(|cx| {
            let delegate = LauncherDelegate::new();
            let mut state = ListState::new(delegate, window, cx);
            // 同步初始选中项：ListState 默认 selected_index 为 None，
            // 必须在此处手动设置，否则第一帧不会高亮第一项
            state.set_selected_index(Some(IndexPath::default()), window, cx);
            state
        });

        let input_sub = cx.subscribe_in(
            &input_state,
            window,
            {
                let input_state = input_state.clone();
                let list_state = list_state.clone();
                move |_this, _, ev: &InputEvent, window, cx| {
                    match ev {
                        InputEvent::Change => {
                            let value = input_state.read(cx).value().to_string();
                            list_state.update(cx, |state, cx| {
                                state.delegate_mut().filter(&value);
                                // 同步 ListState::selected_index，否则视觉选中不生效
                                let new_ix = state.delegate().selected_index;
                                state.set_selected_index(new_ix, window, cx);                                // 滚动到第一项
                                state.scroll_to_selected_item(window, cx);                            });
                        }
                        InputEvent::PressEnter { secondary } => {
                            // 输入框聚焦时 Enter 不在 List 的 dispatch path 里，
                            // 需要手动触发选中项的确认逻辑
                            let secondary = *secondary;
                            list_state.update(cx, |state, cx| {
                                if let Some(ix) = state.selected_index() {
                                    state.delegate_mut().confirm(secondary, window, cx);
                                    let _ = ix; // suppress unused warning
                                }
                            });
                        }
                        _ => {}
                    }
                }
            },
        );

        // 记录窗口当前所在屏幕，以便多屏时下次居中到同一屏幕
        let bounds_sub = cx.observe_window_bounds(window, |_, window, cx| {
            if let Some(display) = window.display(cx) {
                let id = display.id();
                cx.global_mut::<AppSettings>().last_display = Some(id);
            }
        });

        // 失去焦点时彻底隐藏（SW_HIDE）；获得焦点时居中并刷新列表排序
        let activation_sub = cx.observe_window_activation(window, |this, window, cx| {
            if window.is_window_active() {
                // 居中（作为保险，主要居中逻辑在托盘事件处理中）
                center_window(window, cx);
                // 清空输入框，并按最新启动序号重排列表
                this.input_state.update(cx, |input, cx| {
                    input.set_value("", window, cx);
                });
                this.list_state.update(cx, |list, cx| {
                    list.delegate_mut().reload("");
                    let new_ix = list.delegate().selected_index;
                    list.set_selected_index(new_ix, window, cx);
                    list.scroll_to_selected_item(window, cx);
                });
            } else {
                // SW_HIDE：彻底隐藏，不产生桌面图标
                hide_window(window);
            }
        });

        // 语言切换时更新搜索框占位符并通知重绘
        let settings_sub = cx.observe_global_in::<AppSettings>(window, {
            let input_state = input_state.clone();
            move |_, window, cx| {
                let new_placeholder = t("search.placeholder", cx);
                input_state.update(cx, |input, cx| {
                    input.set_placeholder(new_placeholder, window, cx);
                });
                cx.notify();
            }
        });

        let view = Self {
            input_state: input_state.clone(),
            list_state,
            drag_start: None,
            _subscriptions: vec![input_sub, bounds_sub, activation_sub, settings_sub],
        };
        // 窗口创建时立即聚焦输入框，无需鼠标点击即可输入
        input_state.update(cx, |input, cx| input.focus(window, cx));
        view
    }
}

impl Render for LauncherView {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        v_flex()
            .size_full()
            .bg(cx.theme().background)
            // Esc：清空输入框、重置列表、隐藏窗口
            .capture_action(cx.listener(|this, _: &Escape, window, cx| {
                // 清空输入框
                this.input_state.update(cx, |input, cx| {
                    input.set_value("", window, cx);
                });
                // 重置列表到全量 + 选中第一项
                this.list_state.update(cx, |list, cx| {
                    list.delegate_mut().filter("");
                    let new_ix = list.delegate().selected_index;
                    list.set_selected_index(new_ix, window, cx);
                    list.scroll_to_selected_item(window, cx);
                });
                // 隐藏窗口
                hide_window(window);
                cx.stop_propagation();
            }))
            // 上下键切换列表选项（capture 阶段先于 InputState 的 bubble 阶段处理）
            // 必须调用 set_selected_index 同时更新 ListState::selected_index 和
            // delegate::selected_index，否则视觉高亮不会刷新
            .capture_action(cx.listener(|this, _: &MoveDown, window, cx| {
                let new_ix = this.list_state.read(cx).delegate().navigate_selection(true);
                this.list_state.update(cx, |list, cx| {
                    list.set_selected_index(new_ix, window, cx);
                    list.scroll_to_selected_item(window, cx);
                });
                cx.stop_propagation();
            }))
            .capture_action(cx.listener(|this, _: &MoveUp, window, cx| {
                let new_ix = this.list_state.read(cx).delegate().navigate_selection(false);
                this.list_state.update(cx, |list, cx| {
                    list.set_selected_index(new_ix, window, cx);
                    list.scroll_to_selected_item(window, cx);
                });
                cx.stop_propagation();
            }))
            // 搜索栏：声明为窗口拖动区域，同时监听长按手势
            .child(
                div()
                    .px_4()
                    .py_3()
                    .border_b_1()
                    .border_color(cx.theme().border)
                    // Windows 原生：将此区域标记为标题栏拖动区
                    .window_control_area(WindowControlArea::Drag)
                    // 跨平台：长按 200ms 后触发 start_window_move
                    .on_mouse_down(
                        MouseButton::Left,
                        cx.listener(|this, _ev: &MouseDownEvent, _window, _cx| {
                            this.drag_start = Some(Instant::now());
                        }),
                    )
                    .on_mouse_move(cx.listener(|this, _ev: &MouseMoveEvent, window, _cx| {
                        if let Some(start) = this.drag_start {
                            if start.elapsed() >= std::time::Duration::from_millis(200) {
                                this.drag_start = None;
                                window.start_window_move();
                            }
                        }
                    }))
                    .on_mouse_up(
                        MouseButton::Left,
                        cx.listener(|this, _ev: &MouseUpEvent, _window, _cx| {
                            this.drag_start = None;
                        }),
                    )
                    .child(
                        h_flex()
                            .gap_2()
                            .items_center()
                            .child(
                                div()
                                    .text_color(cx.theme().muted_foreground)
                                    .child("🔍"),
                            )
                            .child(
                                Input::new(&self.input_state)
                                    .appearance(false)
                                    .flex_1(),
                            ),
                    ),
            )
            // 应用列表
            .child(List::new(&self.list_state).flex_1())
            // 底部提示栏
            .child(
                h_flex()
                    .px_4()
                    .py_1()
                    .gap_4()
                    .border_t_1()
                    .border_color(cx.theme().border)
                    .text_xs()
                    .text_color(cx.theme().muted_foreground)
                    .items_center()
                    .child(t("hint.select", cx))
                    .child(t("hint.launch", cx))
                    .child(t("hint.close", cx))
                    .child(div().flex_1())
                    .child(
                        Button::new("settings-btn")
                            .ghost()
                            .icon(IconName::Settings)
                            .on_click(|_ev, _window, cx| {
                                let _ = cx.open_window(
                                    WindowOptions {
                                        window_bounds: Some(WindowBounds::Windowed(Bounds {
                                            origin: point(px(460.), px(100.)),
                                            size: size(px(800.), px(600.)),
                                        })),
                                        titlebar: Some(TitlebarOptions {
                                            title: None,
                                            appears_transparent: true,
                                            traffic_light_position: Some(point(px(9.), px(9.))),
                                        }),
                                        ..Default::default()
                                    },
                                    |window, cx| {
                                        let view = cx.new(|cx| SettingsView::new(window, cx));
                                        cx.new(|cx| Root::new(view, window, cx))
                                    },
                                );
                            }),
                    ),
            )
    }
}
