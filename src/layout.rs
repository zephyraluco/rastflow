use gpui::*;
use gpui_component::{
    button::{Button, ButtonVariants},
    input::{Input, InputEvent, InputState},
    list::{List, ListDelegate, ListItem, ListState},
    *,
};
use std::time::Instant;

use crate::settings::{AppSettings, SettingsView};
use crate::utils::{center_window, hide_window};

// ---------- 数据结构 ----------

#[derive(Clone)]
pub struct AppEntry {
    pub name: SharedString,
    pub description: SharedString,
    pub category: SharedString,
}

impl AppEntry {
    pub fn new(name: &str, description: &str, category: &str) -> Self {
        Self {
            name: name.into(),
            description: description.into(),
            category: category.into(),
        }
    }
}

// ---------- List Delegate ----------

pub struct LauncherDelegate {
    all_entries: Vec<AppEntry>,
    filtered: Vec<AppEntry>,
    selected_index: Option<IndexPath>,
}

impl LauncherDelegate {
    pub fn new() -> Self {
        let all_entries = vec![
            AppEntry::new("Visual Studio Code", "强大的跨平台代码编辑器", "开发工具"),
            AppEntry::new("Firefox", "Mozilla 开源网页浏览器", "浏览器"),
            AppEntry::new("Chrome", "Google 高速网页浏览器", "浏览器"),
            AppEntry::new("Terminal", "系统命令行终端模拟器", "系统"),
            AppEntry::new("Finder", "macOS 文件管理器", "系统"),
            AppEntry::new("Spotify", "全球最大音乐流媒体平台", "娱乐"),
            AppEntry::new("Slack", "团队实时沟通协作工具", "通讯"),
            AppEntry::new("Notion", "一体化笔记与知识管理", "效率"),
            AppEntry::new("Figma", "专业界面与原型设计工具", "设计"),
            AppEntry::new("Postman", "API 接口调试与测试工具", "开发工具"),
            AppEntry::new("Docker", "容器化应用开发与部署平台", "开发工具"),
            AppEntry::new("iTerm2", "功能强大的终端增强工具", "系统"),
            AppEntry::new("Obsidian", "本地优先的知识图谱笔记", "效率"),
            AppEntry::new("Discord", "游戏玩家社区语音聊天工具", "通讯"),
            AppEntry::new("Xcode", "Apple 官方 IDE 开发工具", "开发工具"),
        ];
        let filtered = all_entries.clone();
        Self {
            all_entries,
            filtered,
            selected_index: Some(IndexPath::default()),
        }
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
                        || e.description.to_lowercase().contains(&q)
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

    pub fn navigate_selection(&mut self, forward: bool) {
        let len = self.filtered.len();
        if len == 0 {
            self.selected_index = None;
            return;
        }
        self.selected_index = Some(match self.selected_index {
            None => IndexPath::default(),
            Some(ix) => {
                if forward {
                    IndexPath { section: 0, row: (ix.row + 1).min(len - 1), column: 0 }
                } else {
                    IndexPath { section: 0, row: ix.row.saturating_sub(1), column: 0 }
                }
            }
        });
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
        let description = entry.description.clone();
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
                        .py_2()
                        .child(
                            div()
                                .w_10()
                                .h_10()
                                .rounded_lg()
                                .bg(icon_bg)
                                .flex()
                                .items_center()
                                .justify_center()
                                .text_color(icon_fg)
                                .font_bold()
                                .child(first_char),
                        )
                        .child(
                            v_flex()
                                .flex_1()
                                .gap_0p5()
                                .child(div().text_base().child(name))
                                .child(
                                    div()
                                        .text_sm()
                                        .text_color(muted_fg)
                                        .child(description),
                                ),
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
        _window: &mut Window,
        _cx: &mut Context<ListState<Self>>,
    ) {
        if let Some(ix) = self.selected_index {
            if let Some(entry) = self.filtered.get(ix.row) {
                println!("🚀 启动应用: {}", entry.name);
            }
        }
    }

    fn render_empty(
        &mut self,
        _window: &mut Window,
        _cx: &mut Context<ListState<Self>>,
    ) -> impl IntoElement {
        div()
            .flex()
            .items_center()
            .justify_center()
            .p_8()
            .child("没有找到匹配的应用程序")
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
            InputState::new(window, cx).placeholder("搜索应用程序...")
        });

        let list_state = cx.new(|cx| {
            let delegate = LauncherDelegate::new();
            ListState::new(delegate, window, cx)
        });

        let input_sub = cx.subscribe_in(
            &input_state,
            window,
            {
                let input_state = input_state.clone();
                let list_state = list_state.clone();
                move |_this, _, ev: &InputEvent, _window, cx| {
                    if matches!(ev, InputEvent::Change) {
                        let value = input_state.read(cx).value().to_string();
                        list_state.update(cx, |state, cx| {
                            state.delegate_mut().filter(&value);
                            cx.notify();
                        });
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

        // 失去焦点时彻底隐藏（SW_HIDE）；获得焦点时居中（从托盘唤出场景）
        let activation_sub = cx.observe_window_activation(window, |_, window, cx| {
            if window.is_window_active() {
                // 再次居中（作为保险，主要居中逻辑在托盘事件处理中）
                center_window(window, cx);
            } else {
                // SW_HIDE：彻底隐藏，不产生桌面图标
                hide_window(window);
            }
        });

        // 上下键导航列表选项（输入框始终保持焦点）
        let keystroke_sub = cx.observe_keystrokes(|this, ev, window, cx| {
            if ev.keystroke.modifiers.modified() {
                return;
            }
            let key = &ev.keystroke.key;
            if key != "down" && key != "up" {
                return;
            }
            if !this.input_state.read(cx).focus_handle(cx).is_focused(window) {
                return;
            }
            let forward = key == "down";
            this.list_state.update(cx, |list, cx| {
                list.delegate_mut().navigate_selection(forward);
                cx.notify();
            });
        });

        Self {
            input_state,
            list_state,
            drag_start: None,
            _subscriptions: vec![input_sub, bounds_sub, activation_sub, keystroke_sub],
        }
    }
}

impl Render for LauncherView {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        v_flex()
            .size_full()
            .bg(cx.theme().background)
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
                    .child("↑↓ 选择")
                    .child("↵ 启动")
                    .child("Esc 关闭")
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
                                        let view = cx.new(|_| SettingsView);
                                        cx.new(|cx| Root::new(view, window, cx))
                                    },
                                );
                            }),
                    ),
            )
    }
}
