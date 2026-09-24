/// 启动器主视图：渲染搜索栏、应用列表与文件搜索面板
use gpui::prelude::FluentBuilder as _;
use gpui::*;
use gpui_kit::component::{
    button::{Button, ButtonVariants},
    input::{Escape, Input, InputEvent, InputState, MoveDown, MoveUp},
    list::{List, ListDelegate, ListItem, ListState},
    *,
};
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

const FILE_SEARCH_DEBOUNCE: Duration = Duration::from_millis(120);

/// 每批从后台取回的图标数量。
///
/// 分批是为了让首屏（十几行）先拿到图标，其余在后台陆续补齐；
/// 一次性把所有条目的图标都提完再回填的话，列表要等很久才能显示图标。
const ICON_PREFETCH_BATCH: usize = 32;

use crate::locale::t;
use crate::settings::{AppSettings, SettingsView};
use crate::utils::{center_window, hide_window};

use super::delegate::LauncherDelegate;
use super::filesearch::{
    FileResult, SearchError, SearchStatus, open_result, search as search_file_index,
};

actions!(launcher, [ToggleFileMode]);

struct FileSearchDelegate {
    results: Vec<FileResult>,
    selected_index: Option<IndexPath>,
}

impl FileSearchDelegate {
    fn new() -> Self {
        Self {
            results: Vec::new(),
            selected_index: None,
        }
    }

    fn set_results(&mut self, results: Vec<FileResult>) {
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

impl ListDelegate for FileSearchDelegate {
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
                            .child(result.dir.clone()),
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
    file_list_state: Entity<ListState<FileSearchDelegate>>,
    /// 鼠标按下的时刻，用于判断长按拖动
    drag_start: Option<Instant>,
    /// 是否处于文件搜索模式
    file_mode: bool,
    /// 文件索引状态
    file_status: SearchStatus,
    /// 文件搜索结果
    file_results: Vec<FileResult>,
    /// 文件搜索结果中当前选中的索引
    file_selected: usize,
    /// 文件搜索请求序号，用于丢弃过期后台结果
    file_search_generation: u64,
    /// 文件搜索是否进行中
    file_searching: bool,
    /// 文件搜索错误
    file_error: Option<String>,
    /// 是否已有一个图标预取任务在跑（避免重复开链）
    icon_prefetch_running: Arc<AtomicBool>,
    _subscriptions: Vec<Subscription>,
}

impl LauncherView {
    pub fn new(window: &mut Window, cx: &mut Context<Self>) -> Self {
        // 注册 Tab 键：在 Launcher 上下文中切换文件搜索模式
        cx.bind_keys([KeyBinding::new(
            "tab",
            ToggleFileMode,
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

        let file_list_state = cx.new(|cx| {
            let delegate = FileSearchDelegate::new();
            ListState::new(delegate, window, cx)
        });

        let input_sub = cx.subscribe_in(&input_state, window, {
            let input_state = input_state.clone();
            let list_state = list_state.clone();
            move |this, _, ev: &InputEvent, window, cx| match ev {
                InputEvent::Change => {
                    if this.file_mode {
                        let value = input_state.read(cx).value().to_string();
                        this.file_selected = 0;
                        this.search_files(value, cx);
                    } else {
                        let value = input_state.read(cx).value().to_string();
                        list_state.update(cx, |state, cx| {
                            state.delegate_mut().filter(&value);
                            let new_ix = state.delegate().selected_index;
                            state.set_selected_index(new_ix, window, cx);
                            state.scroll_to_selected_item(window, cx);
                        });
                        // 过滤后可能出现尚未提取过图标的条目
                        this.prefetch_icons(cx);
                    }
                }
                InputEvent::PressEnter { secondary, .. } => {
                    if this.file_mode {
                        this.open_selected_file(window, cx);
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
                if this.file_mode {
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
                    // 重载后可能多了新条目（例如新安装的程序）
                    this.prefetch_icons(cx);
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
            file_list_state,
            drag_start: None,
            file_mode: false,
            file_status: SearchStatus::Uninitialized,
            file_results: Vec::new(),
            file_selected: 0,
            file_search_generation: 0,
            file_searching: false,
            file_error: None,
            icon_prefetch_running: Arc::new(AtomicBool::new(false)),
            _subscriptions: vec![input_sub, bounds_sub, activation_sub, settings_sub],
        };
        input_state.update(cx, |input, cx| input.focus(window, cx));
        view.prefetch_icons(cx);
        view
    }

    /// 向列表代理讨下一批待提取图标的启动目标。
    fn take_icon_requests(&self, cx: &mut App) -> Vec<String> {
        self.list_state.update(cx, |state, _| {
            state.delegate_mut().take_icon_requests(ICON_PREFETCH_BATCH)
        })
    }

    /// 后台预取应用图标。
    ///
    /// 列表是虚拟列表（`render_item` 每帧都会对可见行调用），所以渲染路径上只查内存缓存。
    /// 每轮取一小批交给后台线程提取（Shell / GDI 调用），回填后通知列表重绘，
    /// 再继续下一批，直到没有新目标为止。
    ///
    /// `icon_prefetch_running` 只是省掉重复开链，不承担正确性：目标是否已请求由
    /// [`IconCache::request`] 去重，重复调用最多是多开一个立刻结束的任务。
    /// 过滤后新增的条目也会再次触发这里，所以图标是「先到先显示、其陆续补齐」。
    fn prefetch_icons(&self, cx: &mut App) {
        if self.icon_prefetch_running.load(Ordering::Relaxed) {
            return;
        }
        let first = self.take_icon_requests(cx);
        if first.is_empty() {
            return;
        }
        self.icon_prefetch_running.store(true, Ordering::Relaxed);

        // 用弱引用：任务可能比视图活得久（例如窗口关闭后），
        // 直接持强引用会阻止实体销毁，而 `update` 在这里不会 panic。
        let list_state = self.list_state.downgrade();
        let running = self.icon_prefetch_running.clone();
        cx.spawn(async move |cx: &mut gpui::AsyncApp| {
            let mut batch = first;
            loop {
                let loaded = cx
                    .background_executor()
                    .spawn(async move {
                        batch
                            .into_iter()
                            .map(|target| {
                                let icon = crate::app_icon::load(Path::new(&target));
                                (target, icon)
                            })
                            .collect::<Vec<_>>()
                    })
                    .await;

                let next = cx.update(|app| {
                    list_state.update(app, |state, cx| {
                        let delegate = state.delegate_mut();
                        delegate.insert_icons(loaded);
                        let next = delegate.take_icon_requests(ICON_PREFETCH_BATCH);
                        cx.notify();
                        next
                    })
                });

                match next {
                    Ok(next) if !next.is_empty() => batch = next,
                    // 取完或实体已释放
                    _ => break,
                }
            }
            running.store(false, Ordering::Relaxed);
        })
        .detach();
    }

    /// 启动文件搜索并跟踪索引状态。
    fn detect_file_search(&mut self, cx: &mut Context<Self>) {
        self.file_status = SearchStatus::Uninitialized;
        super::filesearch::start();
        self.watch_file_search(cx);
    }

    /// 轮询索引状态，直到稳定（就绪或不可用）。
    ///
    /// 建索引可能要几十秒，期间靠这个循环刷新进度；重建索引时也要重新订阅一次。
    fn watch_file_search(&mut self, cx: &mut Context<Self>) {
        let entity = cx.entity().downgrade();
        cx.spawn(async move |_this, cx: &mut gpui::AsyncApp| {
            loop {
                let status = super::filesearch::status();
                let settled = matches!(
                    status,
                    SearchStatus::Ready { .. } | SearchStatus::Unavailable { .. }
                );
                let _ = cx.update(|app| {
                    let _ = entity.update(app, |this, cx| {
                        let first_ready = !matches!(this.file_status, SearchStatus::Ready { .. })
                            && matches!(status, SearchStatus::Ready { .. });
                        this.file_status = status.clone();
                        if first_ready {
                            // 索引就绪后把当前输入立即搜一遍
                            let query = this.input_state.read(cx).value().to_string();
                            this.search_files(query, cx);
                        }
                        cx.notify();
                    });
                });
                if settled {
                    break;
                }
                cx.background_executor()
                    .timer(Duration::from_millis(400))
                    .await;
            }
        })
        .detach();
    }

    fn search_files(&mut self, query: String, cx: &mut Context<Self>) {
        if !matches!(self.file_status, SearchStatus::Ready { .. }) {
            self.file_results.clear();
            self.file_list_state.update(cx, |state, cx| {
                state.delegate_mut().clear();
                cx.notify();
            });
            self.file_error = None;
            self.file_searching = false;
            cx.notify();
            return;
        }

        if query.trim().is_empty() {
            self.file_search_generation = self.file_search_generation.wrapping_add(1);
            self.file_results.clear();
            self.file_list_state.update(cx, |state, cx| {
                state.delegate_mut().clear();
                cx.notify();
            });
            self.file_selected = 0;
            self.file_error = None;
            self.file_searching = false;
            cx.notify();
            return;
        }

        self.file_search_generation = self.file_search_generation.wrapping_add(1);
        let generation = self.file_search_generation;
        self.file_searching = true;
        self.file_error = None;

        let entity = cx.entity().downgrade();
        cx.spawn(async move |_this, cx: &mut gpui::AsyncApp| {
            cx.background_executor()
                .timer(FILE_SEARCH_DEBOUNCE)
                .await;
            let should_search = cx
                .update(|app| {
                    entity
                        .read_with(app, |this, _| this.file_search_generation == generation)
                        .unwrap_or(false)
                });
            if !should_search {
                return;
            }

            let result = cx
                .background_executor()
                .spawn(async move { search_file_index(&query) })
                .await;
            let _ = cx.update(|app| {
                let _ = entity.update(app, |this, cx| {
                    if this.file_search_generation != generation {
                        return;
                    }

                    this.file_searching = false;
                    this.file_selected = 0;
                    match result {
                        Ok(results) => {
                            this.file_results = results.clone();
                            this.file_list_state.update(cx, |state, cx| {
                                state.delegate_mut().set_results(results);
                                cx.notify();
                            });
                            this.file_error = None;
                        }
                        Err(err) => {
                            this.file_results.clear();
                            this.file_list_state.update(cx, |state, cx| {
                                state.delegate_mut().clear();
                                cx.notify();
                            });
                            this.file_error = Some(match &err {
                                SearchError::Unavailable(reason) => reason.clone(),
                                other => other.message(),
                            });
                        }
                    }
                    cx.notify();
                });
            });
        })
        .detach();

        cx.notify();
    }

    fn open_selected_file(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(result) = self.file_results.get(self.file_selected) {
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
            // Tab：切换文件搜索模式
            .capture_action(cx.listener(|this, _: &ToggleFileMode, window, cx| {
                this.file_mode = !this.file_mode;
                if this.file_mode {
                    this.file_results.clear();
                    this.file_selected = 0;
                    this.file_list_state.update(cx, |state, cx| {
                        state.delegate_mut().clear();
                        cx.notify();
                    });
                    this.input_state.update(cx, |input, cx| {
                        input.set_value("", window, cx);
                        input.set_placeholder("搜索文件...", window, cx);
                    });
                    // 后台启动文件索引（必要时会先建一遍）
                    this.detect_file_search(cx);
                } else {
                    this.file_results.clear();
                    this.file_list_state.update(cx, |state, cx| {
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
            // Esc：文件搜索模式下退出，普通模式下隐藏窗口
            .capture_action(cx.listener(|this, _: &Escape, window, cx| {
                if this.file_mode {
                    this.file_mode = false;
                    this.file_results.clear();
                    this.file_list_state.update(cx, |state, cx| {
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
            // 上下键：启动器模式切换选项，文件搜索模式切换搜索结果
            .capture_action(cx.listener(|this, _: &MoveDown, window, cx| {
                if this.file_mode {
                    if !this.file_results.is_empty() {
                        this.file_selected =
                            (this.file_selected + 1).min(this.file_results.len() - 1);
                        let ix = Some(IndexPath {
                            section: 0,
                            row: this.file_selected,
                            column: 0,
                        });
                        this.file_list_state.update(cx, |list, cx| {
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
                if this.file_mode {
                    if this.file_selected > 0 {
                        this.file_selected -= 1;
                        let ix = Some(IndexPath {
                            section: 0,
                            row: this.file_selected,
                            column: 0,
                        });
                        this.file_list_state.update(cx, |list, cx| {
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
                                    .child(if self.file_mode { "📁" } else { "🔍" }),
                            )
                            .child(Input::new(&self.input_state).appearance(false).flex_1())
                            .when(!self.file_mode, |this| {
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
                            .when(self.file_mode, |this| {
                                this.child(
                                    div()
                                        .px_2()
                                        .py_px()
                                        .rounded_sm()
                                        .bg(cx.theme().accent)
                                        .text_xs()
                                        .text_color(cx.theme().accent_foreground)
                                        .child("本地索引"),
                                )
                            }),
                    ),
            )
            // 内容区
            .child(if self.file_mode {
                self.render_file_search_content(cx).into_any_element()
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

// ---------- 文件搜索内容面板 ----------

impl LauncherView {
    fn render_file_search_content(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let muted_fg = cx.theme().muted_foreground;
        let fg = cx.theme().foreground;
        let border = cx.theme().border;
        let muted_bg = cx.theme().muted;

        match &self.file_status {
            // 还没开始初始化 / 正在等索引就绪
            SearchStatus::Uninitialized => v_flex()
                .flex_1()
                .items_center()
                .justify_center()
                .gap_2()
                .child(
                    div()
                        .text_sm()
                        .text_color(muted_fg)
                        .child("正在初始化文件索引..."),
                )
                .into_any_element(),

            // 正在建索引：显示进度。首次启动会走这里，可能要几十秒。
            SearchStatus::Indexing { disk, written } => v_flex()
                .flex_1()
                .items_center()
                .justify_center()
                .gap_3()
                .p_6()
                .child(div().text_2xl().child("🗂"))
                .child(
                    v_flex()
                        .items_center()
                        .gap_1()
                        .child(
                            div()
                                .text_sm()
                                .font_semibold()
                                .text_color(fg)
                                .child("正在建立文件索引"),
                        )
                        .child(
                            div().text_xs().text_color(muted_fg).child(match disk {
                                Some(disk) => format!("正在扫描 {disk}: 盘，已索引 {written} 个文件"),
                                None => "准备中...".to_string(),
                            }),
                        )
                        .child(
                            div()
                                .text_xs()
                                .text_color(muted_fg)
                                .child("索引只需建立一次，之后启动即可直接搜索"),
                        ),
                )
                .into_any_element(),

            // 不可用：需要管理员权限，或没有可索引的盘
            SearchStatus::Unavailable { reason, .. } => {
                let needs_admin = self.file_status.needs_admin();
                v_flex()
                    .flex_1()
                    .items_center()
                    .justify_center()
                    .gap_4()
                    .p_6()
                    .child(div().text_2xl().child("⚠"))
                    .child(
                        v_flex()
                            .items_center()
                            .gap_1()
                            .child(
                                div()
                                    .text_sm()
                                    .font_semibold()
                                    .text_color(fg)
                                    .child("文件索引不可用"),
                            )
                            .child(
                                div()
                                    .text_xs()
                                    .text_color(muted_fg)
                                    .child(reason.clone()),
                            )
                            .when(needs_admin, |this| {
                                this.child(
                                    div()
                                        .text_xs()
                                        .text_color(muted_fg)
                                        .child("读取 NTFS 主文件表需要管理员权限"),
                                )
                            }),
                    )
                    .child(
                        h_flex()
                            .gap_2()
                            .when(needs_admin, |this| {
                                this.child(
                                    Button::new("elevate-btn")
                                        .child("以管理员身份重启")
                                        .on_click(|_, _, cx| {
                                            // 拉起提权副本后退出当前实例，
                                            // 否则两个实例会抢同一个托盘图标
                                            if super::filesearch::relaunch_as_admin() {
                                                cx.quit();
                                            }
                                        }),
                                )
                            })
                            .child(
                                Button::new("retry-btn")
                                    .ghost()
                                    .child("重试")
                                    .on_click(cx.listener(|this, _, _, cx| {
                                        this.detect_file_search(cx);
                                    })),
                            ),
                    )
                    .into_any_element()
            }

            // 索引可用：显示查询结果
            SearchStatus::Ready { .. } => v_flex()
                .flex_1()
                .p_2()
                .gap_1()
                // 索引可能因为长时间没运行而落后（启动前的改动、监控被停），
                // 给一个手动重扫的入口
                .child(
                    h_flex()
                        .w_full()
                        .px_1()
                        .justify_end()
                        .child(
                            Button::new("rebuild-index-btn")
                                .ghost()
                                .label("重建索引")
                                .on_click(cx.listener(|this, _, _, cx| {
                                    super::filesearch::rebuild_index();
                                    // 重建会重新进入 Indexing，得重新订阅状态才能刷回来
                                    this.watch_file_search(cx);
                                })),
                        ),
                )
                .when(self.file_searching && self.file_results.is_empty(), |this| {
                    this.child(
                        div()
                            .px_2()
                            .py_1()
                            .text_xs()
                            .text_color(muted_fg)
                            .child("正在搜索本地索引..."),
                    )
                })
                .when_some(self.file_error.as_ref(), |this, error| {
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
                    !self.file_searching
                        && self.file_error.is_none()
                        && self.file_results.is_empty(),
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
                .when(!self.file_results.is_empty(), |this| {
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
                .when(!self.file_results.is_empty(), |this| {
                    this.child(List::new(&self.file_list_state).flex_1())
                })
                .into_any_element(),
        }
    }
}
