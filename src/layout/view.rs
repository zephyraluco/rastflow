/// 启动器主视图：渲染搜索栏、应用列表与 Everything 文件搜索面板
use gpui::prelude::FluentBuilder as _;
use gpui::*;
use gpui_component::{
    button::{Button, ButtonVariants},
    input::{Escape, Input, InputEvent, InputState, MoveDown, MoveUp},
    list::{List, ListDelegate, ListItem, ListState},
    *,
};
use std::time::{Duration, Instant};

const EVERYTHING_SEARCH_DEBOUNCE: Duration = Duration::from_millis(120);

use crate::locale::t;
use crate::settings::{AppSettings, SettingsView};
use crate::utils::{center_window, hide_window};

use super::delegate::LauncherDelegate;
use super::everything::{
    EverythingResult, EverythingStatus, open_everything_gui, open_result,
    search as search_everything_index,
};

actions!(launcher, [ToggleEverythingMode]);

struct EverythingDelegate {
    results: Vec<EverythingResult>,
    selected_index: Option<IndexPath>,
}

impl EverythingDelegate {
    fn new() -> Self {
        Self {
            results: Vec::new(),
            selected_index: None,
        }
    }

    fn set_results(&mut self, results: Vec<EverythingResult>) {
        self.results = results;
        self.selected_index = if self.results.is_empty() {
            None
        } else {
            Some(IndexPath::default())
        };
    }

    fn clear(&mut self) {
        self.results.clear();
        self.selected_index = None;
    }
}

impl ListDelegate for EverythingDelegate {
    type Item = ListItem;

    fn items_count(&self, _section: usize, _cx: &App) -> usize {
        self.results.len()
    }

    fn render_item(
        &mut self,
        ix: IndexPath,
        _window: &mut Window,
        cx: &mut Context<ListState<Self>>,
    ) -> Option<Self::Item> {
        let result = self.results.get(ix.row)?;
        let selected = Some(ix) == self.selected_index;
        let muted_fg = cx.theme().muted_foreground;

        Some(
            ListItem::new(ix.row).selected(selected).child(
                h_flex()
                    .gap_3()
                    .items_center()
                    .px_3()
                    .py_1()
                    .child(
                        div()
                            .w(px(180.))
                            .text_sm()
                            .font_semibold()
                            .truncate()
                            .child(result.name.clone()),
                    )
                    .child(
                        div()
                            .flex_1()
                            .text_sm()
                            .text_color(muted_fg)
                            .truncate()
                            .child(result.path.clone()),
                    )
                    .child(
                        div()
                            .w(px(86.))
                            .text_sm()
                            .text_color(muted_fg)
                            .truncate()
                            .child(result.size.clone()),
                    )
                    .child(
                        div()
                            .w(px(128.))
                            .text_sm()
                            .text_color(muted_fg)
                            .truncate()
                            .child(result.modified.clone()),
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
        _cx: &mut Context<ListState<Self>>,
    ) {
        if let Some(ix) = self.selected_index {
            if let Some(result) = self.results.get(ix.row) {
                let _ = open_result(result);
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
            .text_color(cx.theme().muted_foreground)
            .child("未找到匹配文件")
    }
}

// ---------- 启动器主视图 ----------

pub struct LauncherView {
    input_state: Entity<InputState>,
    list_state: Entity<ListState<LauncherDelegate>>,
    everything_list_state: Entity<ListState<EverythingDelegate>>,
    /// 鼠标按下的时刻，用于判断长按拖动
    drag_start: Option<Instant>,
    /// 是否处于 Everything 搜索模式
    everything_mode: bool,
    /// Everything 安装检测状态
    everything_status: EverythingStatus,
    /// Everything 搜索结果
    everything_results: Vec<EverythingResult>,
    /// Everything 搜索结果中当前选中的索引
    everything_selected: usize,
    /// Everything 搜索请求序号，用于丢弃过期后台结果
    everything_search_generation: u64,
    /// Everything 是否正在搜索
    everything_searching: bool,
    /// Everything 搜索错误
    everything_error: Option<String>,
    _subscriptions: Vec<Subscription>,
}

impl LauncherView {
    pub fn new(window: &mut Window, cx: &mut Context<Self>) -> Self {
        // 注册 Tab 键：在 Launcher 上下文中切换 Everything 搜索模式
        cx.bind_keys([KeyBinding::new(
            "tab",
            ToggleEverythingMode,
            Some("Launcher"),
        )]);

        let input_state =
            cx.new(|cx| InputState::new(window, cx).placeholder(t("search.placeholder", cx)));

        let list_state = cx.new(|cx| {
            let delegate = LauncherDelegate::new();
            let mut state = ListState::new(delegate, window, cx);
            // 同步初始选中项：ListState 默认 selected_index 为 None，
            // 必须在此处手动设置，否则第一帧不会高亮第一项
            state.set_selected_index(Some(IndexPath::default()), window, cx);
            state
        });

        let everything_list_state = cx.new(|cx| {
            let delegate = EverythingDelegate::new();
            ListState::new(delegate, window, cx)
        });

        let input_sub = cx.subscribe_in(&input_state, window, {
            let input_state = input_state.clone();
            let list_state = list_state.clone();
            move |this, _, ev: &InputEvent, window, cx| match ev {
                InputEvent::Change => {
                    if this.everything_mode {
                        let value = input_state.read(cx).value().to_string();
                        this.everything_selected = 0;
                        this.search_everything(value, cx);
                    } else {
                        let value = input_state.read(cx).value().to_string();
                        list_state.update(cx, |state, cx| {
                            state.delegate_mut().filter(&value);
                            let new_ix = state.delegate().selected_index;
                            state.set_selected_index(new_ix, window, cx);
                            state.scroll_to_selected_item(window, cx);
                        });
                    }
                }
                InputEvent::PressEnter { secondary } => {
                    if this.everything_mode {
                        this.open_selected_everything(window, cx);
                    } else {
                        let secondary = *secondary;
                        list_state.update(cx, |state, cx| {
                            if let Some(ix) = state.selected_index() {
                                state.delegate_mut().confirm(secondary, window, cx);
                                let _ = ix;
                            }
                        });
                    }
                }
                _ => {}
            }
        });

        // 记录窗口当前所在屏幕，以便多屏时下次居中到同一屏幕
        let bounds_sub = cx.observe_window_bounds(window, |_, window, cx| {
            if let Some(display) = window.display(cx) {
                let id = display.id();
                cx.global_mut::<AppSettings>().last_display = Some(id);
            }
        });

        // 失去焦点时彻底隐藏；获得焦点时居中并刷新列表
        let activation_sub = cx.observe_window_activation(window, |this, window, cx| {
            if window.is_window_active() {
                center_window(window, cx);
                if this.everything_mode {
                    cx.notify();
                } else {
                    this.input_state.update(cx, |input, cx| {
                        input.set_value("", window, cx);
                        input.set_placeholder(t("search.placeholder", cx), window, cx);
                    });
                    this.list_state.update(cx, |list, cx| {
                        list.delegate_mut().reload("");
                        let new_ix = list.delegate().selected_index;
                        list.set_selected_index(new_ix, window, cx);
                        list.scroll_to_selected_item(window, cx);
                    });
                }
            } else {
                hide_window(window);
            }
        });

        // 语言切换时更新搜索框占位符
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
            everything_list_state,
            drag_start: None,
            everything_mode: false,
            everything_status: EverythingStatus::Unknown,
            everything_results: Vec::new(),
            everything_selected: 0,
            everything_search_generation: 0,
            everything_searching: false,
            everything_error: None,
            _subscriptions: vec![input_sub, bounds_sub, activation_sub, settings_sub],
        };
        input_state.update(cx, |input, cx| input.focus(window, cx));
        view
    }

    /// 在后台线程中检测 Everything，完成后更新状态并通知重绘
    fn detect_everything(&mut self, cx: &mut Context<Self>) {
        self.everything_status = EverythingStatus::Unknown;
        let entity = cx.entity().downgrade();
        cx.spawn(async move |_this, cx: &mut gpui::AsyncApp| {
            let status = cx
                .background_executor()
                .spawn(async { super::everything::detect() })
                .await;
            let _ = cx.update(|app| {
                let _ = entity.update(app, |this, cx| {
                    this.everything_status = status;
                    if matches!(this.everything_status, EverythingStatus::Indexed { .. }) {
                        let query = this.input_state.read(cx).value().to_string();
                        this.search_everything(query, cx);
                    }
                    cx.notify();
                });
            });
        })
        .detach();
    }

    fn search_everything(&mut self, query: String, cx: &mut Context<Self>) {
        if !matches!(self.everything_status, EverythingStatus::Indexed { .. }) {
            self.everything_results.clear();
            self.everything_list_state.update(cx, |state, cx| {
                state.delegate_mut().clear();
                cx.notify();
            });
            self.everything_error = None;
            self.everything_searching = false;
            cx.notify();
            return;
        }

        if query.trim().is_empty() {
            self.everything_search_generation = self.everything_search_generation.wrapping_add(1);
            self.everything_results.clear();
            self.everything_list_state.update(cx, |state, cx| {
                state.delegate_mut().clear();
                cx.notify();
            });
            self.everything_selected = 0;
            self.everything_error = None;
            self.everything_searching = false;
            cx.notify();
            return;
        }

        self.everything_search_generation = self.everything_search_generation.wrapping_add(1);
        let generation = self.everything_search_generation;
        self.everything_searching = true;
        self.everything_error = None;

        let entity = cx.entity().downgrade();
        cx.spawn(async move |_this, cx: &mut gpui::AsyncApp| {
            cx.background_executor()
                .timer(EVERYTHING_SEARCH_DEBOUNCE)
                .await;
            let should_search = cx
                .update(|app| {
                    entity
                        .read_with(app, |this, _| this.everything_search_generation == generation)
                        .unwrap_or(false)
                });
            if !should_search {
                return;
            }

            let result = cx
                .background_executor()
                .spawn(async move { search_everything_index(&query) })
                .await;
            let _ = cx.update(|app| {
                let _ = entity.update(app, |this, cx| {
                    if this.everything_search_generation != generation {
                        return;
                    }

                    this.everything_searching = false;
                    this.everything_selected = 0;
                    match result {
                        Ok(results) => {
                            this.everything_results = results.clone();
                            this.everything_list_state.update(cx, |state, cx| {
                                state.delegate_mut().set_results(results);
                                cx.notify();
                            });
                            this.everything_error = None;
                        }
                        Err(err) => {
                            this.everything_results.clear();
                            this.everything_list_state.update(cx, |state, cx| {
                                state.delegate_mut().clear();
                                cx.notify();
                            });
                            this.everything_error =
                                Some(format!("Everything_QueryW failed: {}", err.code));
                        }
                    }
                    cx.notify();
                });
            });
        })
        .detach();

        cx.notify();
    }

    fn open_selected_everything(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(result) = self.everything_results.get(self.everything_selected) {
            let _ = open_result(result);
            hide_window(window);
            cx.notify();
        }
    }
}

impl Render for LauncherView {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        v_flex()
            .size_full()
            .bg(cx.theme().background)
            .key_context("Launcher")
            // Tab：切换 Everything 搜索模式
            .capture_action(cx.listener(|this, _: &ToggleEverythingMode, window, cx| {
                this.everything_mode = !this.everything_mode;
                if this.everything_mode {
                    this.everything_results.clear();
                    this.everything_selected = 0;
                    this.everything_list_state.update(cx, |state, cx| {
                        state.delegate_mut().clear();
                        cx.notify();
                    });
                    this.input_state.update(cx, |input, cx| {
                        input.set_value("", window, cx);
                        input.set_placeholder("搜索文件...", window, cx);
                    });
                    // 后台检测 Everything 安装状态
                    this.detect_everything(cx);
                } else {
                    this.everything_results.clear();
                    this.everything_list_state.update(cx, |state, cx| {
                        state.delegate_mut().clear();
                        cx.notify();
                    });
                    this.input_state.update(cx, |input, cx| {
                        input.set_value("", window, cx);
                        input.set_placeholder(t("search.placeholder", cx), window, cx);
                    });
                    this.list_state.update(cx, |list, cx| {
                        list.delegate_mut().filter("");
                        let new_ix = list.delegate().selected_index;
                        list.set_selected_index(new_ix, window, cx);
                        list.scroll_to_selected_item(window, cx);
                    });
                }
                cx.stop_propagation();
                cx.notify();
            }))
            // Esc：Everything 模式下退出，普通模式下隐藏窗口
            .capture_action(cx.listener(|this, _: &Escape, window, cx| {
                if this.everything_mode {
                    this.everything_mode = false;
                    this.everything_results.clear();
                    this.everything_list_state.update(cx, |state, cx| {
                        state.delegate_mut().clear();
                        cx.notify();
                    });
                    this.input_state.update(cx, |input, cx| {
                        input.set_value("", window, cx);
                        input.set_placeholder(t("search.placeholder", cx), window, cx);
                    });
                    this.list_state.update(cx, |list, cx| {
                        list.delegate_mut().filter("");
                        let new_ix = list.delegate().selected_index;
                        list.set_selected_index(new_ix, window, cx);
                        list.scroll_to_selected_item(window, cx);
                    });
                } else {
                    this.input_state.update(cx, |input, cx| {
                        input.set_value("", window, cx);
                    });
                    this.list_state.update(cx, |list, cx| {
                        list.delegate_mut().filter("");
                        let new_ix = list.delegate().selected_index;
                        list.set_selected_index(new_ix, window, cx);
                        list.scroll_to_selected_item(window, cx);
                    });
                    hide_window(window);
                }
                cx.stop_propagation();
            }))
            // 上下键：启动器模式切换选项，Everything 模式切换搜索结果
            .capture_action(cx.listener(|this, _: &MoveDown, window, cx| {
                if this.everything_mode {
                    if !this.everything_results.is_empty() {
                        this.everything_selected =
                            (this.everything_selected + 1).min(this.everything_results.len() - 1);
                        let ix = Some(IndexPath {
                            section: 0,
                            row: this.everything_selected,
                            column: 0,
                        });
                        this.everything_list_state.update(cx, |list, cx| {
                            list.set_selected_index(ix, window, cx);
                            list.scroll_to_selected_item(window, cx);
                        });
                        cx.notify();
                    }
                } else {
                    let new_ix = this.list_state.read(cx).delegate().navigate_selection(true);
                    this.list_state.update(cx, |list, cx| {
                        list.set_selected_index(new_ix, window, cx);
                        list.scroll_to_selected_item(window, cx);
                    });
                }
                cx.stop_propagation();
            }))
            .capture_action(cx.listener(|this, _: &MoveUp, window, cx| {
                if this.everything_mode {
                    if this.everything_selected > 0 {
                        this.everything_selected -= 1;
                        let ix = Some(IndexPath {
                            section: 0,
                            row: this.everything_selected,
                            column: 0,
                        });
                        this.everything_list_state.update(cx, |list, cx| {
                            list.set_selected_index(ix, window, cx);
                            list.scroll_to_selected_item(window, cx);
                        });
                        cx.notify();
                    }
                } else {
                    let new_ix = this
                        .list_state
                        .read(cx)
                        .delegate()
                        .navigate_selection(false);
                    this.list_state.update(cx, |list, cx| {
                        list.set_selected_index(new_ix, window, cx);
                        list.scroll_to_selected_item(window, cx);
                    });
                }
                cx.stop_propagation();
            }))
            // 搜索栏
            .child(
                div()
                    .px_4()
                    .py_3()
                    .border_b_1()
                    .border_color(cx.theme().border)
                    .window_control_area(WindowControlArea::Drag)
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
                                    .child(if self.everything_mode { "📁" } else { "🔍" }),
                            )
                            .child(Input::new(&self.input_state).appearance(false).flex_1())
                            .when(!self.everything_mode, |this| {
                                this.child(
                                    h_flex()
                                        .gap_1()
                                        .items_center()
                                        .flex_shrink_0()
                                        .text_xs()
                                        .text_color(cx.theme().muted_foreground)
                                        .child("文件搜索")
                                        .child(
                                            div()
                                                .px_1()
                                                .py_px()
                                                .rounded_sm()
                                                .border_1()
                                                .border_color(cx.theme().border)
                                                .bg(cx.theme().muted)
                                                .text_xs()
                                                .text_color(cx.theme().muted_foreground)
                                                .child("Tab"),
                                        ),
                                )
                            })
                            .when(self.everything_mode, |this| {
                                this.child(
                                    div()
                                        .px_2()
                                        .py_px()
                                        .rounded_sm()
                                        .bg(cx.theme().accent)
                                        .text_xs()
                                        .text_color(cx.theme().accent_foreground)
                                        .child("Everything"),
                                )
                            }),
                    ),
            )
            // 内容区
            .child(if self.everything_mode {
                self.render_everything_content(cx).into_any_element()
            } else {
                List::new(&self.list_state).flex_1().into_any_element()
            })
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

// ---------- Everything 内容面板 ----------

impl LauncherView {
    fn render_everything_content(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let muted_fg = cx.theme().muted_foreground;
        let fg = cx.theme().foreground;
        let border = cx.theme().border;
        let muted_bg = cx.theme().muted;

        match &self.everything_status {
            // 正在检测
            EverythingStatus::Unknown => v_flex()
                .flex_1()
                .items_center()
                .justify_center()
                .gap_2()
                .child(
                    div()
                        .text_sm()
                        .text_color(muted_fg)
                        .child("正在检测 Everything..."),
                )
                .into_any_element(),

            // 未安装
            EverythingStatus::NotInstalled => v_flex()
                .flex_1()
                .items_center()
                .justify_center()
                .gap_4()
                .p_6()
                .child(div().text_2xl().child("📁"))
                .child(
                    v_flex()
                        .items_center()
                        .gap_1()
                        .child(
                            div()
                                .text_sm()
                                .font_semibold()
                                .text_color(fg)
                                .child("未检测到 Everything"),
                        )
                        .child(
                            div()
                                .text_xs()
                                .text_color(muted_fg)
                                .child("需要安装 Everything 才能使用文件搜索功能"),
                        ),
                )
                .child(
                    h_flex()
                        .gap_2()
                        .child(Button::new("install-btn").child("前往下载").on_click(
                            |_, _, _cx| {
                                // 打开 Everything 官网下载页
                                let _ = std::process::Command::new("cmd")
                                    .args([
                                        "/C",
                                        "start",
                                        "",
                                        "https://www.voidtools.com/downloads/",
                                    ])
                                    .spawn();
                            },
                        ))
                        .child(
                            Button::new("redetect-btn")
                                .ghost()
                                .child("重新检测")
                                .on_click(cx.listener(|this, _, _, cx| {
                                    this.detect_everything(cx);
                                })),
                        ),
                )
                .into_any_element(),

            // 已安装 Everything，但当前用户还没有索引数据库
            EverythingStatus::NotIndexed { exe_path } => {
                let query = self.input_state.read(cx).value().to_string();
                let exe2 = exe_path.clone();
                let has_query = !query.trim().is_empty();
                v_flex()
                    .flex_1()
                    .p_4()
                    .gap_3()
                    .child(
                        div()
                            .px_3()
                            .py_2()
                            .rounded_lg()
                            .bg(muted_bg)
                            .border_1()
                            .border_color(border)
                            .text_xs()
                            .text_color(muted_fg)
                            .child("Everything 未检索。请先打开 Everything 完成索引后再使用搜索。"),
                    )
                    .child(h_flex().gap_2().when(has_query, |this| {
                        let q = query.clone();
                        this.child(
                            Button::new("search-everything-btn")
                                .ghost()
                                .child(format!("搜索 \"{}\"", query))
                                .on_click(move |_, _, _cx| {
                                    open_everything_gui(&exe2, &q);
                                }),
                        )
                    }))
                    .into_any_element()
            }

            // 已安装 Everything 且已有数据库，显示 SDK 查询结果
            EverythingStatus::Indexed { .. } => v_flex()
                .flex_1()
                .p_2()
                .gap_1()
                .when(self.everything_searching && self.everything_results.is_empty(), |this| {
                    this.child(
                        div()
                            .px_2()
                            .py_1()
                            .text_xs()
                            .text_color(muted_fg)
                            .child("正在搜索 Everything 索引..."),
                    )
                })
                .when_some(self.everything_error.as_ref(), |this, error| {
                    this.child(
                        div()
                            .px_2()
                            .py_1()
                            .rounded_md()
                            .bg(muted_bg)
                            .border_1()
                            .border_color(border)
                            .text_xs()
                            .text_color(muted_fg)
                            .child(error.clone()),
                    )
                })
                .when(
                    !self.everything_searching
                        && self.everything_error.is_none()
                        && self.everything_results.is_empty(),
                    |this| {
                        this.child(
                            div()
                                .px_2()
                                .py_1()
                                .text_xs()
                                .text_color(muted_fg)
                                .child("未找到匹配文件"),
                        )
                    },
                )
                .when(!self.everything_results.is_empty(), |this| {
                    this.child(
                        h_flex()
                            .w_full()
                            .px_3()
                            .py_1()
                            .gap_3()
                            .border_b_1()
                            .border_color(border)
                            .text_xs()
                            .font_semibold()
                            .text_color(muted_fg)
                            .child(div().w(px(180.)).child("名称"))
                            .child(div().flex_1().child("路径"))
                            .child(div().w(px(86.)).child("大小"))
                            .child(div().w(px(128.)).child("修改时间")),
                    )
                })
                .when(!self.everything_results.is_empty(), |this| {
                    this.child(List::new(&self.everything_list_state).flex_1())
                })
                .into_any_element(),
        }
    }
}
