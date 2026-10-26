//! 自动更新：检查、下载、安装，以及把结果告知用户。
//!
//! 两件事必须分清：
//!
//! 1. **检查本身在后台跑**（[`Updater::check`] 内部用 GPUI 的后台执行器），
//!    托盘点击只负责发起，不阻塞消息循环。
//! 2. **结果用原生消息框提示**，不唤出启动器主界面 —— 主窗口是隐藏的 `PopUp`
//!    窗口，为了看一眼「已是最新版本」把它弹出来既突兀又会抢焦点。
//!
//! 消息框跑在独立线程上：`MessageBoxW` 会阻塞调用线程直到用户点掉，放在 UI 线程
//! 上会让热键与托盘一起卡住。

use std::sync::mpsc::{self, Receiver, Sender};
use std::thread;

use gpui::{App, AppContext, Entity};
use gpui_updater::{EngineConfig, GitHubSource, UpdateStatus, Updater, Version};
use windows::Win32::UI::WindowsAndMessaging::{
    IDYES, MB_ICONERROR, MB_ICONINFORMATION, MB_ICONQUESTION, MB_OK, MB_SETFOREGROUND, MB_YESNO,
    MESSAGEBOX_STYLE, MessageBoxW,
};
use windows::core::PCWSTR;

use crate::locale::{t, update_available, update_staged};

pub type UpdateEntity = Entity<Updater>;

pub fn create(cx: &mut gpui::App) -> UpdateEntity {
    let source = GitHubSource::new("zephyraluco", "rastflow").asset_contains(".msi");
    let config = EngineConfig::new(
        Version::parse(env!("CARGO_PKG_VERSION")).expect("Cargo package version must be valid semver"),
    );
    cx.new(|cx| Updater::new(source, config, cx))
}

/// 用户在「发现新版本」提示框里做出的选择。
///
/// 消息框在后台线程上等待点击，所以选择要经通道回传给主线程，再由主线程去操作
/// `Updater` 实体（实体不是 `Send`）。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Choice {
    /// 立即下载并安装
    Install,
}

/// 把更新状态的变化翻译成用户可见的提示框。
///
/// 主循环每 50ms 调一次 [`poll`](Self::poll)。只有**用户主动发起**的流程才会提示：
/// 托盘点击时调 [`begin`](Self::begin) 标记，状态落定后弹一次框并解除。这样重复
/// 检查（结果相同）每次也都能得到反馈，而过程态（检查中、下载中）不会刷屏。
pub struct Reporter {
    /// 是否有一轮用户主动发起的流程在等待结果
    watching: bool,
    /// 后台消息框回传的选择
    choices: Receiver<Choice>,
    /// 分发给消息框线程的克隆
    sender: Sender<Choice>,
}

impl Reporter {
    pub fn new() -> Self {
        let (sender, choices) = mpsc::channel();
        Self {
            watching: false,
            choices,
            sender,
        }
    }

    /// 开始跟踪一轮流程（检查更新 / 下载安装）。
    pub fn begin(&mut self) {
        self.watching = true;
    }

    /// 状态落定时弹一次提示框。`Checking`/`Downloading`/`Installing` 只是过程态。
    pub fn poll(&mut self, status: &UpdateStatus, cx: &App) {
        if !self.watching {
            return;
        }
        // 过程态不算落定：等这一轮跑完再说话
        let Some(notice) = notice_for(status) else {
            return;
        };

        let title = t("update.title", cx).to_string();
        let (text, style, ask_install) = match notice {
            Notice::UpToDate => (
                t("update.up_to_date", cx).to_string(),
                MB_OK | MB_ICONINFORMATION,
                false,
            ),
            Notice::Available(version) => (
                update_available(&version, env!("CARGO_PKG_VERSION"), cx),
                MB_YESNO | MB_ICONQUESTION,
                true,
            ),
            Notice::Staged(version) => (
                update_staged(&version, cx),
                MB_OK | MB_ICONINFORMATION,
                false,
            ),
            Notice::Failed(message) => (
                format!("{}\n\n{message}", t("update.failed", cx)),
                MB_OK | MB_ICONERROR,
                false,
            ),
        };

        self.watching = false;
        self.show(title, text, style, ask_install);
    }

    /// 取出用户在提示框里做出的选择（非阻塞）。
    pub fn take_choice(&mut self) -> Option<Choice> {
        self.choices.try_recv().ok()
    }

    /// 在独立线程上弹消息框，不阻塞调用方。
    fn show(&self, title: String, text: String, style: MESSAGEBOX_STYLE, ask_install: bool) {
        let sender = self.sender.clone();
        let spawned = thread::Builder::new()
            .name("rastflow-update-prompt".to_string())
            .spawn(move || {
                let text_wide = to_wide(&text);
                let title_wide = to_wide(&title);
                let result = unsafe {
                    MessageBoxW(
                        None,
                        PCWSTR(text_wide.as_ptr()),
                        PCWSTR(title_wide.as_ptr()),
                        style | MB_SETFOREGROUND,
                    )
                };
                if ask_install && result == IDYES {
                    // 主线程可能已经退出，发不出去也无所谓
                    let _ = sender.send(Choice::Install);
                    crate::application::send(crate::application::Event::UpdaterChanged);
                }
            });
        if let Err(err) = spawned {
            eprintln!("[rastflow] 无法创建更新提示框线程：{err}");
        }
    }
}

impl Default for Reporter {
    fn default() -> Self {
        Self::new()
    }
}

/// 一次「落定」的状态，对应一个提示框。
///
/// 抽成独立类型是为了让「哪些状态该弹框、弹哪种框」可以脱离 GPUI 与 Win32 测试。
#[derive(Clone, Debug, PartialEq, Eq)]
enum Notice {
    /// 已是最新版本
    UpToDate,
    /// 有新版本（版本号）
    Available(String),
    /// 已下载并安装，重启生效（版本号）
    Staged(String),
    /// 失败原因
    Failed(String),
}

/// 状态是否需要提示；过程态（检查中 / 下载中 / 安装中）返回 `None`。
///
/// 这条规则很要紧：一次点击只能得到**一个**结论框。`Downloading`/`Installing` 是
/// 过程态，若也为它们弹框，用户会连点好几下才等到结果。
fn notice_for(status: &UpdateStatus) -> Option<Notice> {
    match status {
        UpdateStatus::UpToDate => Some(Notice::UpToDate),
        UpdateStatus::Available(version) => Some(Notice::Available(version.to_string())),
        UpdateStatus::Staged(version) => Some(Notice::Staged(version.to_string())),
        UpdateStatus::Errored(error) => Some(Notice::Failed(error.message().to_string())),
        // `Idle` 是初始值；其余都是过程态
        _ => None,
    }
}

/// 转成以 NUL 结尾的 UTF-16
fn to_wide(value: &str) -> Vec<u16> {
    value.encode_utf16().chain(std::iter::once(0)).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui_updater::ErrorKind;

    fn version(text: &str) -> Version {
        Version::parse(text).expect("测试版本号应当合法")
    }

    /// 落定的三种结果各弹一种框
    #[test]
    fn settled_states_produce_one_notice() {
        assert_eq!(notice_for(&UpdateStatus::UpToDate), Some(Notice::UpToDate));
        assert_eq!(
            notice_for(&UpdateStatus::Available(version("0.2.0"))),
            Some(Notice::Available("0.2.0".to_string()))
        );
        assert_eq!(
            notice_for(&UpdateStatus::Staged(version("0.2.0"))),
            Some(Notice::Staged("0.2.0".to_string()))
        );
    }

    /// 失败要带上原始原因，方便用户反馈
    #[test]
    fn errored_state_keeps_the_reason() {
        let status = UpdateStatus::Errored(gpui_updater::UpdateError::new(
            ErrorKind::Network,
            "http error: GET releases -> 503",
        ));
        assert_eq!(
            notice_for(&status),
            Some(Notice::Failed(
                "http error: GET releases -> 503".to_string()
            ))
        );
    }

    /// 过程态与初始态都不弹框：否则一次点击会连续冒出好几个框
    #[test]
    fn in_flight_states_stay_silent() {
        for status in [
            UpdateStatus::Idle,
            UpdateStatus::Checking,
            UpdateStatus::Downloading {
                downloaded: 1024,
                total: Some(2048),
            },
            UpdateStatus::Installing,
        ] {
            assert_eq!(notice_for(&status), None, "{status:?} 不应弹框");
        }
    }
}

