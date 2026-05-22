/// 启动器主视图：渲染搜索栏、应用列表与 AI 对话面板

use gpui::prelude::FluentBuilder as _;
use gpui::*;
use gpui_component::{
    button::{Button, ButtonVariants},
    input::{Escape, Input, InputEvent, InputState, MoveDown, MoveUp},
    list::{List, ListDelegate, ListState},
    text,
    *,
};
use std::time::Instant;

use crate::ai;
use crate::locale::t;
use crate::settings::{AppSettings, SettingsView};
use crate::utils::{center_window, hide_window};

use super::chat::{ChatMessage, ChatRole};
use super::delegate::LauncherDelegate;

actions!(launcher, [ToggleAiMode]);

// ---------- 启动器主视图 ----------

pub struct LauncherView {
    input_state: Entity<InputState>,
    list_state: Entity<ListState<LauncherDelegate>>,
    /// 鼠标按下的时刻，用于判断长按拖动
    drag_start: Option<Instant>,
    /// 是否处于 AI 对话模式
    ai_mode: bool,
    /// 已发送的对话消息列表（含用户与 AI 回复）
    chat_messages: Vec<ChatMessage>,
    /// 对话区域的滚动句柄，用于自动滚到底部
    chat_scroll: ScrollHandle,
    _subscriptions: Vec<Subscription>,
}

impl LauncherView {
    pub fn new(window: &mut Window, cx: &mut Context<Self>) -> Self {
        // 注册 Tab 项：在 Launcher 上下文中按下 Tab 触发 ToggleAiMode
        cx.bind_keys([KeyBinding::new("tab", ToggleAiMode, Some("Launcher"))]);

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
                            if this.ai_mode {
                                // AI 模式：刷新对话预览
                                cx.notify();
                            } else {
                                let value = input_state.read(cx).value().to_string();
                                list_state.update(cx, |state, cx| {
                                    state.delegate_mut().filter(&value);
                                    // 同步 ListState::selected_index，否则视觉选中不生效
                                    let new_ix = state.delegate().selected_index;
                                    state.set_selected_index(new_ix, window, cx);
                                    // 滚动到第一项
                                    state.scroll_to_selected_item(window, cx);
                                });
                            }
                        }
                        InputEvent::PressEnter { secondary } => {
                            if this.ai_mode {
                                // AI 模式：发送消息，并触发后台 AI 调用
                                let value = input_state.read(cx).value().to_string();
                                if !value.trim().is_empty() {
                                    // 添加用户消息
                                    this.chat_messages.push(ChatMessage {
                                        role: ChatRole::User,
                                        content: value.clone(),
                                        loading: false,
                                    });
                                    // 添加 AI 占位消息（加载中）
                                    let ai_idx = this.chat_messages.len();
                                    this.chat_messages.push(ChatMessage {
                                        role: ChatRole::Assistant,
                                        content: String::new(),
                                        loading: true,
                                    });
                                    input_state.update(cx, |input, cx| {
                                        input.set_value("", window, cx);
                                    });
                                    this.chat_scroll.scroll_to_bottom();
                                    cx.notify();

                                    // 读取 AI 设置
                                    let settings = cx.global::<AppSettings>();
                                    let api_key = if !settings.ai_api_key.is_empty() {
                                        settings.ai_api_key.to_string()
                                    } else {
                                        std::env::var("ANTHROPIC_API_KEY").unwrap_or_default()
                                    };
                                    let base_url = settings.ai_base_url.to_string();
                                    let model = settings.ai_model.to_string();

                                    // 发送给 AI，在后台线程运行，通过 oneshot channel 返回结果
                                    let rx = ai::send_message(value, api_key, base_url, model);
                                    let entity = cx.entity().downgrade();

                                    cx.spawn(async move |_this, cx: &mut gpui::AsyncApp| {
                                        let result = rx.await;
                                        let _ = cx.update(move |app| {
                                            let _ = entity.update(app, move |this, cx| {
                                                if let Some(msg) = this.chat_messages.get_mut(ai_idx) {
                                                    match result {
                                                        Ok(Ok(response)) => {
                                                            msg.content = response;
                                                        }
                                                        Ok(Err(e)) => {
                                                            msg.content = format!("错误: {e}");
                                                        }
                                                        Err(_) => {
                                                            msg.content = "请求已取消".to_string();
                                                        }
                                                    }
                                                    msg.loading = false;
                                                }
                                                this.chat_scroll.scroll_to_bottom();
                                                cx.notify();
                                            });
                                        });
                                    }).detach();
                                }
                            } else {
                                // 启动器模式：启动选中项
                                let secondary = *secondary;
                                list_state.update(cx, |state, cx| {
                                    if let Some(ix) = state.selected_index() {
                                        state.delegate_mut().confirm(secondary, window, cx);
                                        let _ = ix; // suppress unused warning
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

        // 失去焦点时彻底隐藏（SW_HIDE）；获得焦点时居中并刷新列表排序
        let activation_sub = cx.observe_window_activation(window, |this, window, cx| {
            if window.is_window_active() {
                // 居中（作为保险，主要居中逻辑在托盘事件处理中）
                center_window(window, cx);
                // 每次重新显示都重置到启动器模式
                this.ai_mode = false;
                this.chat_messages.clear();
                // 清空输入框，并按最新启动序号重排列表
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
            ai_mode: false,
            chat_messages: Vec::new(),
            chat_scroll: ScrollHandle::new(),
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
            .key_context("Launcher")
            // Tab：切换 AI 对话模式
            .capture_action(cx.listener(|this, _: &ToggleAiMode, window, cx| {
                this.ai_mode = !this.ai_mode;
                if this.ai_mode {
                    this.input_state.update(cx, |input, cx| {
                        input.set_value("", window, cx);
                        input.set_placeholder("输入问题，按 Enter 发送...", window, cx);
                    });
                } else {
                    this.chat_messages.clear();
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
            // Esc：AI 模式下退出对话，普通模式下隐藏窗口
            .capture_action(cx.listener(|this, _: &Escape, window, cx| {
                if this.ai_mode {
                    this.ai_mode = false;
                    this.chat_messages.clear();
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
            // 上下键切换列表选项（仅在展示列表时生效）
            .capture_action(cx.listener(|this, _: &MoveDown, window, cx| {
                if !this.ai_mode {
                    let new_ix = this.list_state.read(cx).delegate().navigate_selection(true);
                    this.list_state.update(cx, |list, cx| {
                        list.set_selected_index(new_ix, window, cx);
                        list.scroll_to_selected_item(window, cx);
                    });
                }
                cx.stop_propagation();
            }))
            .capture_action(cx.listener(|this, _: &MoveUp, window, cx| {
                if !this.ai_mode {
                    let new_ix = this.list_state.read(cx).delegate().navigate_selection(false);
                    this.list_state.update(cx, |list, cx| {
                        list.set_selected_index(new_ix, window, cx);
                        list.scroll_to_selected_item(window, cx);
                    });
                }
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
                                    .child(if self.ai_mode { "💬" } else { "🔍" }),
                            )
                            .child(
                                Input::new(&self.input_state)
                                    .appearance(false)
                                    .flex_1(),
                            )
                            .when(!self.ai_mode, |this| {
                                this.child(
                                    h_flex()
                                        .gap_1()
                                        .items_center()
                                        .flex_shrink_0()
                                        .text_xs()
                                        .text_color(cx.theme().muted_foreground)
                                        .child("问 AI")
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
                            }),
                    ),
            )
            // 内容区：AI 对话 或 应用列表
            .child(if self.ai_mode {
                let current_input = self.input_state.read(cx).value().to_string();
                let accent = cx.theme().accent;
                let accent_fg = cx.theme().accent_foreground;
                let muted_fg = cx.theme().muted_foreground;
                let border_color = cx.theme().border;
                let messages = self.chat_messages.clone();
                let is_empty = messages.is_empty() && current_input.is_empty();
                v_flex()
                    .id("chat-area")
                    .flex_1()
                    .overflow_y_scroll()
                    .track_scroll(&self.chat_scroll)
                    .p_4()
                    .gap_3()
                    .when(is_empty, |this| {
                        this.child(
                            div()
                                .flex_1()
                                .flex()
                                .items_center()
                                .justify_center()
                                .text_sm()
                                .text_color(muted_fg)
                                .child("输入问题，按 Enter 发送"),
                        )
                    })
                    .children(messages.into_iter().enumerate().map(move |(idx, msg)| {
                        match msg.role {
                            ChatRole::User => {
                                // 用户消息：右对齐，accent 背景
                                h_flex()
                                    .w_full()
                                    .justify_end()
                                    .child(
                                        div()
                                            .max_w(px(440.))
                                            .px_3()
                                            .py_2()
                                            .rounded_lg()
                                            .bg(accent)
                                            .child(
                                                text::TextView::markdown(
                                                    format!("chat-user-{idx}"),
                                                    msg.content,
                                                )
                                                .selectable(true)
                                                .text_sm()
                                                .text_color(accent_fg),
                                            ),
                                    )
                                    .into_any_element()
                            }
                            ChatRole::Assistant => {
                                // AI 消息：左对齐，muted 背景 / 加载中显示省略号
                                let content = if msg.loading {
                                    "…".to_string()
                                } else {
                                    msg.content.clone()
                                };
                                h_flex()
                                    .w_full()
                                    .justify_start()
                                    .child(
                                        div()
                                            .max_w(px(440.))
                                            .px_3()
                                            .py_2()
                                            .rounded_lg()
                                            .border_1()
                                            .border_color(border_color)
                                            .child(
                                                text::TextView::markdown(
                                                    format!("chat-ai-{idx}"),
                                                    content,
                                                )
                                                .selectable(true)
                                                .text_sm()
                                                .text_color(muted_fg),
                                            ),
                                    )
                                    .into_any_element()
                            }
                        }
                    }))
                    .when(!current_input.is_empty(), |this| {
                        this.child(
                            h_flex()
                                .w_full()
                                .justify_end()
                                .child(
                                    div()
                                        .max_w(px(440.))
                                        .px_3()
                                        .py_2()
                                        .rounded_lg()
                                        .border_1()
                                        .border_color(border_color)
                                        .text_sm()
                                        .text_color(muted_fg)
                                        .child(current_input),
                                ),
                        )
                    })
                    .into_any_element()
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
