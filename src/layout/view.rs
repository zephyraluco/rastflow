/// 启动器主视图：渲染搜索栏、应用列表与 Everything 文件搜索面板

use gpui::prelude::FluentBuilder as _;
use gpui::*;
use gpui_component::{
    button::{Button, ButtonVariants},
    input::{Escape, Input, InputEvent, InputState, MoveDown, MoveUp},
    list::{List, ListDelegate, ListState},
    *,
};
use std::time::Instant;

use crate::locale::t;
use crate::settings::{AppSettings, SettingsView};
use crate::utils::{center_window, hide_window};

use super::delegate::LauncherDelegate;
use super::everything::{EverythingStatus, open_everything_gui, open_path, search_with_es};

actions!(launcher, [ToggleEverythingMode]);

// ---------- 启动器主视图 ----------

pub struct LauncherView {
    input_state: Entity<InputState>,
    list_state: Entity<ListState<LauncherDelegate>>,
    /// 鼠标按下的时刻，用于判断长按拖动
    drag_start: Option<Instant>,
    /// 是否处于 Everything 搜索模式
    everything_mode: bool,
    /// Everything 安装检测状态
    everything_status: EverythingStatus,
    /// Everything 搜索结果（文件路径列表）
    everything_results: Vec<String>,
    /// Everything 搜索结果中当前选中的索引
    everything_selected: usize,
    _subscriptions: Vec<Subscription>,
}

impl LauncherView {
    pub fn new(window: &mut Window, cx: &mut Context<Self>) -> Self {
        // 注册 Tab 键：在 Launcher 上下文中切换 Everything 搜索模式
        cx.bind_keys([KeyBinding::new("tab", ToggleEverythingMode, Some("Launcher"))]);

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
                move |this, _, ev: &InputEvent, window, cx| {
                    match ev {
                        InputEvent::Change => {
                            if this.everything_mode {
                                // Everything 模式：触发后台文件搜索
                                let query = input_state.read(cx).value().to_string();
                                this.everything_selected = 0;
                                let status = this.everything_status.clone();
                                let entity = cx.entity().downgrade();
                                cx.spawn(async move |_this, cx: &mut gpui::AsyncApp| {
                                    let results = cx
                                        .background_executor()
                                        .spawn(async move {
                                            match &status {
                                                EverythingStatus::ReadyWithEs { es_path, .. } => {
                                                    search_with_es(es_path, &query, 50)
                                                }
                                                _ => vec![],
                                            }
                                        })
                                        .await;
                                    let _ = cx.update(|app| {
                                        let _ = entity.update(app, |this, cx| {
                                            this.everything_results = results;
                                            this.everything_selected = 0;
                                            cx.notify();
                                        });
                                    });
                                })
                                .detach();
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
                                let query = input_state.read(cx).value().to_string();
                                match &this.everything_status {
                                    EverythingStatus::ReadyWithEs { .. } => {
                                        // 打开选中的文件
                                        if let Some(path) =
                                            this.everything_results.get(this.everything_selected)
                                        {
                                            let path = path.clone();
                                            open_path(&path);
                                        }
                                    }
                                    EverythingStatus::InstalledOnly { exe_path } => {
                                        // 在 Everything GUI 中搜索
                                        if !query.trim().is_empty() {
                                            let exe = exe_path.clone();
                                            let q = query.clone();
                                            open_everything_gui(&exe, &q);
                                        }
                                    }
                                    _ => {}
                                }
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
            drag_start: None,
            everything_mode: false,
            everything_status: EverythingStatus::Unknown,
            everything_results: Vec::new(),
            everything_selected: 0,
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
                    cx.notify();
                });
            });
        })
        .detach();
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
                    this.input_state.update(cx, |input, cx| {
                        input.set_value("", window, cx);
                        input.set_placeholder("搜索文件...", window, cx);
                    });
                    // 后台检测 Everything 安装状态
                    this.detect_everything(cx);
                } else {
                    this.everything_results.clear();
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
                        cx.notify();
                    }
                } else {
                    let new_ix = this.list_state.read(cx).delegate().navigate_selection(false);
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
                            .child(
                                Input::new(&self.input_state)
                                    .appearance(false)
                                    .flex_1(),
                            )
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
        let accent = cx.theme().accent;
        let accent_fg = cx.theme().accent_foreground;
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
                .child(
                    div()
                        .text_2xl()
                        .child("📁"),
                )
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
                        .child(
                            Button::new("install-btn")
                                .child("前往下载")
                                .on_click(|_, _, _cx| {
                                    // 打开 Everything 官网下载页
                                    let _ = std::process::Command::new("cmd")
                                        .args(["/C", "start", "", "https://www.voidtools.com/downloads/"])
                                        .spawn();
                                }),
                        )
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

            // 已安装但无 es.exe（只能跳转到 GUI）
            EverythingStatus::InstalledOnly { exe_path } => {
                let query = self.input_state.read(cx).value().to_string();
                let exe = exe_path.clone();
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
                            .child("已安装 Everything，但未找到 es.exe 命令行工具。输入关键词后按 Enter 可在 Everything 中搜索，或下载 es.exe 以在此处显示结果。"),
                    )
                    .child(
                        h_flex()
                            .gap_2()
                            .child(
                                Button::new("open-everything-btn")
                                    .child("打开 Everything")
                                    .on_click(move |_, _, _cx| {
                                        open_everything_gui(&exe, "");
                                    }),
                            )
                            .when(has_query, |this| {
                                let q = query.clone();
                                this.child(
                                    Button::new("search-everything-btn")
                                        .ghost()
                                        .child(format!("搜索 \"{}\"", query))
                                        .on_click(move |_, _, _cx| {
                                            open_everything_gui(&exe2, &q);
                                        }),
                                )
                            })
                            .child(
                                Button::new("dl-es-btn")
                                    .ghost()
                                    .child("下载 es.exe")
                                    .on_click(|_, _, _cx| {
                                        let _ = std::process::Command::new("cmd")
                                            .args(["/C", "start", "", "https://www.voidtools.com/downloads/"])
                                            .spawn();
                                    }),
                            ),
                    )
                    .into_any_element()
            }

            // 完整模式：显示内联搜索结果
            EverythingStatus::ReadyWithEs { .. } => {
                let results = &self.everything_results;
                let selected = self.everything_selected;

                if results.is_empty() {
                    v_flex()
                        .flex_1()
                        .items_center()
                        .justify_center()
                        .child(
                            div()
                                .text_sm()
                                .text_color(muted_fg)
                                .child("输入关键词搜索文件"),
                        )
                        .into_any_element()
                } else {
                    v_flex()
                        .id("ev-results")
                        .flex_1()
                        .overflow_y_scroll()
                        .children(results.iter().enumerate().map(|(i, path)| {
                            let is_selected = i == selected;
                            let file_name = std::path::Path::new(path)
                                .file_name()
                                .map(|n| n.to_string_lossy().to_string())
                                .unwrap_or_else(|| path.clone());
                            let dir_path = std::path::Path::new(path)
                                .parent()
                                .map(|p| p.to_string_lossy().to_string())
                                .unwrap_or_default();
                            let path_clone = path.clone();

                            div()
                                .id(("ev-result", i))
                                .px_4()
                                .py_2()
                                .cursor_pointer()
                                .when(is_selected, |this| this.bg(accent))
                                .when(!is_selected, |this| {
                                    this.hover(|s| s.bg(cx.theme().secondary))
                                })
                                .on_click(move |_, _, _cx| {
                                    open_path(&path_clone);
                                })
                                .child(
                                    v_flex()
                                        .gap_px()
                                        .child(
                                            div()
                                                .text_sm()
                                                .font_semibold()
                                                .text_color(if is_selected { accent_fg } else { fg })
                                                .overflow_hidden()
                                                .whitespace_nowrap()
                                                .child(file_name),
                                        )
                                        .child(
                                            div()
                                                .text_xs()
                                                .text_color(if is_selected {
                                                    accent_fg
                                                } else {
                                                    muted_fg
                                                })
                                                .overflow_hidden()
                                                .whitespace_nowrap()
                                                .child(dir_path),
                                        ),
                                )
                        }))
                        .into_any_element()
                }
            }
        }
    }
}

