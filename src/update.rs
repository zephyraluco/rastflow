//! 自动升级
//!
//! 走 [`cargo_packager_updater`]：请求远端更新清单（`latest.json`）→ 比较版本 →
//! 下载安装包 → **校验 minisign 签名** → 交给打包器生成的 NSIS 安装器静默安装 → 重启。
//!
//! # 为什么用现成更新器而不是自己替换 exe
//!
//! 「怎么安全地结束正在运行的自己、把文件换掉、失败还能回滚」才是升级里真正难的部分。
//! 本项目用的 `cargo-packager` 生成的 NSIS 安装器已经具备这些能力，而且是靠模板实现的：
//!
//! - 被动模式（`/P`）下 `CheckIfAppIsRunning` **直接结束正在运行的同名进程**，不弹确认框；
//! - `/R` 让安装器装完自动把应用重新拉起来；
//! - 升级路径**不会**触碰卸载段里的 `appdataPaths`，所以 `%LOCALAPPDATA%\rastflow`
//!   下的索引与设置不受影响。
//!
//! 自己写这套时序（重命名运行中的镜像 → 换文件 → 回滚）能不弹安装器进度条，
//! 代价是几百行容易写错的代码，不划算。
//!
//! # 代价（都是已知且可接受的）
//!
//! - [`cargo_packager_updater::Update::install`] 最后会调用 `std::process::exit(0)`，
//!   **进程立刻消失**，来不及做任何收尾。所以内存索引不会落快照 —— 但这不是问题：
//!   下次启动会从 USN 日志补齐，只是首次启动略慢。
//! - 安装器按**镜像名**结束进程，因此所有叫 `rastflow.exe` 的进程都会被结束。
//!   升级时不要同时跑 release 目录里的那份副本。
//!
//! # 线程模型
//!
//! 与 [`crate::layout::filesearch`] 一致：全局 [`STATE`] 持有状态，网络与安装都在
//! 后台线程上跑，界面只读 [`status`]，靠轮询刷新。所有网络失败一律**静默**
//! （只落到状态里），绝不弹框 —— 托盘常驻程序弹报错框是最容易被骂的设计。

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, OnceLock};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use cargo_packager_updater::{
    Config, Error as UpdaterError, Update, UpdaterBuilder, WindowsConfig, WindowsUpdateInstallMode,
    semver::Version,
};

// ---------- 配置 ----------

/// 更新清单端点。
///
/// 指到「最新 Release 的一个固定资产名」上，好处是不需要任何服务端：
/// 发布流水线只要把 `latest.json` 当附件传上去，这个 URL 恒定不变。
const ENDPOINT: &str =
    "https://github.com/zephyraluco/rastflow/releases/latest/download/latest.json";

/// 更新清单的 minisign 公钥（base64 后的公钥盒，即 `*.key.pub` 文件的内容）。
///
/// 用 `cargo packager signer generate --path <私钥路径>` 生成；私钥只放 CI secret，
/// **绝不进仓库**。公钥换了而旧安装包还在，升级就会校验失败，所以这串要尽量稳定。
const PUBKEY: &str = "dW50cnVzdGVkIGNvbW1lbnQ6IG1pbmlzaWduIHB1YmxpYyBrZXk6IDYwNkUzQzU2QTA4QkNFNDUKUldSRnpvdWdWanh1WVAzdkozbnBqWDZ2VmxscUtwUHlGOGlsaHRqbzUzcnpERk9NMFhMa2hSc2MK";

/// 单次网络请求超时。检查用 15 秒足够；下载不设这个限制（安装包可能有几十 MB）。
const CHECK_TIMEOUT: Duration = Duration::from_secs(15);

/// 启动后延迟多久做首次自动检查。
///
/// 不能一启动就查：此时正在建索引（首次要跑几十秒），再叠一个网络请求会一起抢 IO。
const AUTO_CHECK_DELAY: Duration = Duration::from_secs(30);

/// 自动检查的间隔。
const AUTO_CHECK_INTERVAL: Duration = Duration::from_secs(24 * 60 * 60);

// ---------- 系统代理 ----------

/// 把 Windows 的「系统代理」转成 `HTTP_PROXY` / `HTTPS_PROXY` 环境变量。
///
/// # 为什么非做不可
///
/// 更新器内部用的是 `reqwest`，而 reqwest **只认环境变量**，不读注册表里的系统代理
/// —— 那是 WinINET / WinHTTP 的机制。于是会出现一种很有欺骗性的现象：
/// `Invoke-WebRequest`、`git push` 全都正常，看着「网络没问题」，
/// 而更新检查却一直转到超时才失败（本机实测：不转这一步 15 秒超时，
/// 转完 0.7 秒返回结果）。默认开着代理的环境基本都会踩到。
///
/// 已经设过环境变量的不覆盖，尊重用户的显式配置。
///
/// # 调用时机
///
/// **必须在进程里只有主线程时调用**（即 gpui 启动之前）。`std::env::set_var`
/// 在 Rust 2024 里被标成 unsafe，原因就是别的线程可能正在读环境变量；
/// 放在这里就没有这个风险。
pub fn apply_system_proxy() {
    if std::env::var_os("HTTPS_PROXY").is_some()
        || std::env::var_os("HTTP_PROXY").is_some()
        || std::env::var_os("ALL_PROXY").is_some()
    {
        return;
    }

    let Some(proxy) = system_proxy() else {
        return;
    };

    // SAFETY: 见上面的「调用时机」—— 调用点保证此时进程里只有主线程。
    unsafe {
        std::env::set_var("HTTPS_PROXY", &proxy);
        std::env::set_var("HTTP_PROXY", &proxy);
    }
    eprintln!("[rastflow] 更新检查将走系统代理：{proxy}");
}

/// 读取系统代理地址（形如 `http://127.0.0.1:7890`）。未启用或读不到时返回 `None`。
///
/// 只处理显式配置的代理。基于 PAC（`AutoConfigURL`）的自动代理在这种情况下没法解析，
/// 于是返回 `None`，让更新退回直连 —— 最坏结果也只是静默的「检查失败」。
fn system_proxy() -> Option<String> {
    use windows::Win32::System::Registry::{
        HKEY_CURRENT_USER, RRF_RT_REG_DWORD, RRF_RT_REG_SZ, RegGetValueW,
    };
    use windows::core::w;

    /// WinINET 代理设置的注册表位置
    const KEY: windows::core::PCWSTR =
        w!("Software\\Microsoft\\Windows\\CurrentVersion\\Internet Settings");

    // ProxyEnable 不是 1 就等于没启用（读不到也算没启用）
    let mut enabled: u32 = 0;
    let mut size = std::mem::size_of::<u32>() as u32;
    let status = unsafe {
        RegGetValueW(
            HKEY_CURRENT_USER,
            KEY,
            w!("ProxyEnable"),
            RRF_RT_REG_DWORD,
            None,
            Some(&mut enabled as *mut u32 as *mut core::ffi::c_void),
            Some(&mut size),
        )
    };
    if status.is_err() || enabled != 1 {
        return None;
    }

    // ProxyServer 是宽字符串：先问需要多大，再读
    let mut size: u32 = 0;
    let status = unsafe {
        RegGetValueW(
            HKEY_CURRENT_USER,
            KEY,
            w!("ProxyServer"),
            RRF_RT_REG_SZ,
            None,
            None,
            Some(&mut size),
        )
    };
    if status.is_err() || size == 0 {
        return None;
    }

    // 长度是字节数，而缓冲区是 u16，所以要除以 2（向上取整以容纳结尾 NUL）
    let mut buf = vec![0u16; (size as usize).div_ceil(2)];
    let status = unsafe {
        RegGetValueW(
            HKEY_CURRENT_USER,
            KEY,
            w!("ProxyServer"),
            RRF_RT_REG_SZ,
            None,
            Some(buf.as_mut_ptr() as *mut core::ffi::c_void),
            Some(&mut size),
        )
    };
    if status.is_err() {
        return None;
    }

    let raw = String::from_utf16_lossy(&buf);
    normalize_proxy(raw.trim_end_matches('\0'))
}

/// 把 `ProxyServer` 的取值规整成 reqwest 能用的 URL。
///
/// 这个值有两种写法，都要认：
///
/// - `127.0.0.1:7890` —— 所有协议共用同一个代理；
/// - `http=127.0.0.1:7890;https=127.0.0.1:7890` —— 按协议分别指定，取 `https` 那条
///   （更新走 HTTPS，没有 `https` 就退回 `http`）。
fn normalize_proxy(raw: &str) -> Option<String> {
    let raw = raw.trim();
    if raw.is_empty() {
        return None;
    }

    let pick = |protocol: &str| -> Option<&str> {
        raw.split(';')
            .filter_map(|part| part.split_once('='))
            .find(|(key, _)| key.trim().eq_ignore_ascii_case(protocol))
            .map(|(_, value)| value.trim())
            .filter(|value| !value.is_empty())
    };

    let host_port = if raw.contains('=') {
        pick("https").or_else(|| pick("http"))?
    } else {
        raw
    };

    // 有的配置连协议一起写（`https://…`），有的只写 `host:port`
    if host_port.starts_with("http://") || host_port.starts_with("https://") {
        Some(host_port.to_string())
    } else {
        Some(format!("http://{host_port}"))
    }
}

// ---------- 对外类型 ----------

/// 升级流程的状态。
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum UpdateStatus {
    /// 本次运行还没查过
    Idle,
    /// 正在请求更新清单
    Checking,
    /// 已是最新
    UpToDate { version: String },
    /// 发现新版本
    Available { version: String, notes: String },
    /// 正在下载安装包
    Downloading { got: u64, total: Option<u64> },
    /// 正在交给安装器。
    ///
    /// 到了这一步进程很快会被安装器结束（见模块文档），界面只需显示这一句。
    Installing { version: String },
    /// 失败。`reason` 是给人看的（已尽量翻译过），不是原始错误类型。
    Failed { reason: String },
}

impl UpdateStatus {
    /// 是否处于「正在忙」的阶段（此时不该重复触发检查或安装）
    pub fn is_busy(&self) -> bool {
        matches!(
            self,
            Self::Checking | Self::Downloading { .. } | Self::Installing { .. }
        )
    }

    /// 是否可以开始安装
    pub fn can_install(&self) -> bool {
        matches!(self, Self::Available { .. })
    }
}

// ---------- 全局状态 ----------

struct Inner {
    status: Mutex<UpdateStatus>,
    /// 检查通过但还没安装的更新
    pending: Mutex<Option<Update>>,
    /// 是否已有检查或安装在跑（防止重复触发）
    busy: AtomicBool,
    /// 上次检查完成的时间（Unix 秒），0 表示本次运行还没查过
    last_check: AtomicU64,
}

impl Inner {
    fn status(&self) -> UpdateStatus {
        lock(&self.status).clone()
    }

    fn set_status(&self, status: UpdateStatus) {
        *lock(&self.status) = status;
    }
}

static STATE: OnceLock<Arc<Inner>> = OnceLock::new();

/// 取锁，忽略中毒：状态里没有任何不变式会因为别的线程 panic 而失效。
fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

fn now_unix() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

// ---------- 对外 API ----------

/// 当前版本号。取自 `Cargo.toml`，与 `build.rs` 写进 exe 资源的是同一个值。
pub fn current_version() -> &'static str {
    env!("CARGO_PKG_VERSION")
}

/// 初始化：起一个后台线程，延迟 [`AUTO_CHECK_DELAY`] 后做首次检查。
///
/// 只应在启动时调用一次。是否真的去查由调用方通过 [`is_due`] / [`check`] 控制，
/// 这里不做偏好判断 —— 偏好存在 `AppSettings` 里，只有主循环读得到。
pub fn init() {
    let _ = STATE.set(Arc::new(Inner {
        status: Mutex::new(UpdateStatus::Idle),
        pending: Mutex::new(None),
        busy: AtomicBool::new(false),
        last_check: AtomicU64::new(0),
    }));

    std::thread::Builder::new()
        .name("rastflow-update-init".to_string())
        .spawn(|| {
            std::thread::sleep(AUTO_CHECK_DELAY);
            check();
        })
        .ok();
}

/// 当前状态快照（界面每次渲染时读一次）
pub fn status() -> UpdateStatus {
    STATE.get().map(|inner| inner.status()).unwrap_or(UpdateStatus::Idle)
}

/// 是否到了该自动检查的时候。
///
/// 首次检查由 [`init`] 的延迟线程负责（那时 `last_check` 还是 0），
/// 所以这里在 `last_check == 0` 时返回 `false`，两次检查天然不会撞在一起。
pub fn is_due() -> bool {
    let Some(inner) = STATE.get() else {
        return false;
    };
    let last = inner.last_check.load(Ordering::SeqCst);
    last != 0 && now_unix().saturating_sub(last) >= AUTO_CHECK_INTERVAL.as_secs()
}

/// 触发一次版本检查（手动按钮与自动检查都走这里）。
///
/// 已经在检查或下载时直接忽略。函数本身立即返回，网络请求在后台线程上跑。
pub fn check() {
    let Some(inner) = STATE.get() else {
        return;
    };
    // 配置缺失属于「程序坏了」，不是网络问题，值得单独说清楚
    if PUBKEY.is_empty() {
        inner.set_status(UpdateStatus::Failed {
            reason: "未配置更新签名公钥，无法检查新版本".to_string(),
        });
        return;
    }
    if inner.busy.swap(true, Ordering::SeqCst) {
        return;
    }

    inner.set_status(UpdateStatus::Checking);

    let task = Arc::clone(inner);
    let spawned = std::thread::Builder::new()
        .name("rastflow-update-check".to_string())
        .spawn(move || {
            match fetch_update() {
                Ok(Some(update)) => {
                    let version = update.version.clone();
                    let notes = update.body.clone().unwrap_or_default();
                    *lock(&task.pending) = Some(update);
                    task.set_status(UpdateStatus::Available { version, notes });
                }
                Ok(None) => task.set_status(UpdateStatus::UpToDate {
                    version: current_version().to_string(),
                }),
                Err(reason) => {
                    eprintln!("[rastflow] 检查更新失败：{reason}");
                    task.set_status(UpdateStatus::Failed { reason });
                }
            }
            task.last_check.store(now_unix(), Ordering::SeqCst);
            task.busy.store(false, Ordering::SeqCst);
        })
        .is_ok();

    if !spawned {
        inner.busy.store(false, Ordering::SeqCst);
        inner.set_status(UpdateStatus::Failed {
            reason: "无法启动更新检查线程".to_string(),
        });
    }
}

/// 下载并安装已发现的更新。
///
/// **会结束当前进程**：安装器装完后由它负责重启（见模块文档）。
/// 因此调用前要把界面切到「正在安装」，之后不必再关心状态怎么变。
pub fn install() {
    let Some(inner) = STATE.get() else {
        return;
    };
    if inner.busy.swap(true, Ordering::SeqCst) {
        return;
    }

    let Some(update) = lock(&inner.pending).clone() else {
        inner.busy.store(false, Ordering::SeqCst);
        inner.set_status(UpdateStatus::Failed {
            reason: "没有待安装的更新，请先检查更新".to_string(),
        });
        return;
    };

    let version = update.version.clone();
    inner.set_status(UpdateStatus::Downloading {
        got: 0,
        total: None,
    });

    let task = Arc::clone(inner);
    let spawned = std::thread::Builder::new()
        .name("rastflow-update-install".to_string())
        .spawn(move || {
            let got = AtomicU64::new(0);
            let total = AtomicU64::new(0);

            let result = update.download_extended(
                |chunk, content_length| {
                    let got_now = got.fetch_add(chunk as u64, Ordering::Relaxed) + chunk as u64;
                    if let Some(len) = content_length {
                        total.store(len, Ordering::Relaxed);
                    }
                    let total_now = total.load(Ordering::Relaxed);
                    task.set_status(UpdateStatus::Downloading {
                        got: got_now,
                        total: (total_now != 0).then_some(total_now),
                    });
                },
                || {},
            );

            match result {
                Ok(bytes) => {
                    task.set_status(UpdateStatus::Installing {
                        version: version.clone(),
                    });
                    // 正常情况下走不到下一行：install 内部在拉起安装器后会 exit(0)。
                    // 只有连安装器都没能启动时才会返回错误。
                    if let Err(err) = update.install(bytes) {
                        eprintln!("[rastflow] 启动安装器失败：{err}");
                        task.set_status(UpdateStatus::Failed {
                            reason: format!("启动安装器失败：{err}"),
                        });
                        task.busy.store(false, Ordering::SeqCst);
                    }
                }
                Err(err) => {
                    let reason = describe_error(&err);
                    eprintln!("[rastflow] 下载更新失败：{reason}");
                    task.set_status(UpdateStatus::Failed { reason });
                    task.busy.store(false, Ordering::SeqCst);
                }
            }
        })
        .is_ok();

    if !spawned {
        inner.busy.store(false, Ordering::SeqCst);
        inner.set_status(UpdateStatus::Failed {
            reason: "无法启动下载线程".to_string(),
        });
    }
}

// ---------- 内部实现 ----------

/// 请求更新清单，返回「比当前版本新」的那个 Release（没有则 `None`）。
fn fetch_update() -> Result<Option<Update>, String> {
    let current = Version::parse(current_version())
        .map_err(|err| format!("本地版本号 {} 不是合法 semver：{err}", current_version()))?;

    let updater = UpdaterBuilder::new(current, updater_config())
        .timeout(CHECK_TIMEOUT)
        .build()
        .map_err(|err| describe_error(&err))?;

    updater.check().map_err(|err| describe_error(&err))
}

fn updater_config() -> Config {
    Config {
        endpoints: vec![
            ENDPOINT
                .parse()
                .expect("ENDPOINT 是编译期常量，必须能解析成 URL"),
        ],
        pubkey: PUBKEY.to_string(),
        windows: Some(WindowsConfig {
            installer_args: None,
            // 显式写死 Passive（有进度条、无需用户交互），不依赖上游默认值：
            // 它既不像 BasicUi 那样等用户点确认，也不像 Quiet 那样无法自行请求管理员权限。
            install_mode: Some(WindowsUpdateInstallMode::Passive),
        }),
    }
}

/// 把更新器的错误翻译成人话。
///
/// 大部分情形直接透传原始信息就够了；只有几种会因为「这个项目还没发布过版本」
/// 或「网络不通」而出现、且原文对用户毫无意义的，才需要改写。
fn describe_error(err: &UpdaterError) -> String {
    match err {
        // 端点返回 404 等非成功状态时，更新器最终报的就是这个。
        // 对用户来说，它的真实含义是「远端还没有可用的版本信息」。
        UpdaterError::ReleaseNotFound => "远端还没有发布任何版本".to_string(),
        UpdaterError::Network(_) => format!("网络请求失败（{err}）"),
        _ => err.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use super::*;

    /// 用 `dist/` 里真实产出的安装包与签名，走一遍升级器**完全相同**的校验流程。
    ///
    /// 守住三件一旦弄错就会静默失效的事：
    ///
    /// 1. `PUBKEY` 与发布用的私钥是同一对。贴错公钥的表现是「用户点升级才报签名失败」，
    ///    那时已经来不及发现。
    /// 2. `latest.json` 的 `signature` 字段该放什么 —— 放 `.sig` **文件原文**，
    ///    因为原文本身已经是 base64（见 cargo-packager 的 `sign_file_with_secret_key`
    ///    里的 `STANDARD.encode(signature_box.to_string())`）。升级器会再解一次 base64，
    ///    所以这里既不能重复编码，也不能改成别的包装。
    /// 3. 被签名的确实是安装包文件本身。
    ///
    /// `dist/` 下没有产物时跳过 —— 它由 `cargo packager` 生成。想本地验证就先打一次包；
    /// CI 里是先打包再跑测试，所以这条一定会执行到。
    #[test]
    fn real_installer_signature_verifies() {
        let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("dist");
        let Some(installer) = std::fs::read_dir(&dir).ok().and_then(|entries| {
            entries.flatten().map(|entry| entry.path()).find(|path| {
                path.file_name()
                    .and_then(|name| name.to_str())
                    .is_some_and(|name| name.ends_with("-setup.exe"))
            })
        }) else {
            eprintln!("跳过：{dir:?} 下没有安装包，先跑一次 cargo packager");
            return;
        };

        let sig_path = format!("{}.sig", installer.display());
        let sig_text = std::fs::read_to_string(&sig_path)
            .unwrap_or_else(|err| panic!("读取 {sig_path} 失败：{err}"));
        let package = std::fs::read(&installer).expect("读取安装包失败");

        verify_like_updater(&package, &sig_text).unwrap_or_else(|err| {
            panic!(
                "签名校验失败（{err}）：PUBKEY 与签名用的私钥不匹配，\
                 或签的不是这个安装包 —— 这两件事都会让用户点「立即升级」时才失败"
            )
        });
    }

    /// 对着**真实的**更新端点跑一次检查，用来人工确认整条链路。
    ///
    /// 默认被 `#[ignore]` 跳过（要联网，不适合放进常规测试）。需要时手动跑：
    ///
    /// ```text
    /// cargo test probe_real_endpoint -- --ignored --nocapture
    /// ```
    ///
    /// 在还没发布过任何 Release 时，预期输出是「远端还没有发布任何版本」——
    /// 这正好验证了「404 被翻译成人话」，而不是把 reqwest 的原始错误甩给用户。
    ///
    /// 它先调 [`apply_system_proxy`]，所以顺带验证了「系统代理被正确转成环境变量」
    /// —— 少了那一步，本机（开着代理）会在这里超时而不是报错。
    #[test]
    #[ignore = "需要联网，手动运行"]
    fn probe_real_endpoint() {
        apply_system_proxy();
        match fetch_update() {
            Ok(Some(update)) => println!(
                "发现新版本 {}（当前 {}），说明：{}",
                update.version,
                current_version(),
                update.body.clone().unwrap_or_default()
            ),
            Ok(None) => println!("已是最新版本 {}", current_version()),
            Err(reason) => println!("检查失败：{reason}"),
        }
    }

    /// 复刻 `cargo_packager_updater` 的 `verify_signature`，用来校验真实产物。
    ///
    /// 有意保留这份副本：这条流程上散落着几处「多一层 / 少一层 base64」的陷阱，
    /// 比起相信文档，不如让测试真跑一遍同样的步骤。
    fn verify_like_updater(package: &[u8], signature_field: &str) -> Result<(), String> {
        use base64::Engine as _;

        let decode = |text: &str| -> Result<String, String> {
            let bytes = base64::engine::general_purpose::STANDARD
                .decode(text)
                .map_err(|err| format!("不是合法 base64（是不是多了换行或空白？）：{err}"))?;
            String::from_utf8(bytes).map_err(|err| format!("base64 解出来不是 UTF-8：{err}"))
        };

        let public_key = minisign_verify::PublicKey::decode(&decode(PUBKEY)?)
            .map_err(|err| format!("公钥解析失败：{err}"))?;
        let signature = minisign_verify::Signature::decode(&decode(signature_field)?)
            .map_err(|err| format!("签名解析失败：{err}"))?;

        public_key
            .verify(package, &signature, true)
            .map(|_| ())
            .map_err(|err| err.to_string())
    }

    /// 校验发布流水线真正会传上去的那套东西：`dist/latest.json` + 它指向的安装包。
    ///
    /// 上一条测试直接读 `.sig` 文件，守住的是「密钥对与签名算法」；
    /// 这条走的是客户端实际会解析的清单，能揪出清单生成侧的错：
    /// signature 多套了一层 base64、带了尾部换行、文件带了 BOM、
    /// 或者 url 指向了一个不存在的文件名。这些错在打包阶段全都一声不响，
    /// 只会在用户点「立即升级」时变成「签名校验失败」。
    ///
    /// 清单不存在时跳过（它由 `scripts/gen-update-manifest.ps1` 生成）。
    #[test]
    fn update_manifest_is_accepted_by_updater() {
        let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("dist");
        let manifest_path = dir.join("latest.json");
        let Ok(text) = std::fs::read_to_string(&manifest_path) else {
            eprintln!("跳过：{manifest_path:?} 不存在，先生成一次清单");
            return;
        };

        // 文件带 BOM 时 serde_json 会直接失败 —— 这正是要守住的点之一
        let manifest: serde_json::Value =
            serde_json::from_str(&text).expect("latest.json 必须是合法 JSON（注意别写成带 BOM 的）");

        assert_eq!(
            manifest["version"].as_str(),
            Some(current_version()),
            "清单里的 version 必须与 Cargo.toml 一致，否则客户端会误判有无更新"
        );
        assert_eq!(
            manifest["format"].as_str(),
            Some("nsis"),
            "NSIS 安装包必须标成 nsis，写错客户端会直接报不支持的格式"
        );

        let url = manifest["url"].as_str().expect("清单缺少 url");
        assert!(url.starts_with("https://"), "下载地址必须走 HTTPS：{url}");

        let signature = manifest["signature"].as_str().expect("清单缺少 signature");
        assert_eq!(
            signature,
            signature.trim(),
            "signature 不能带首尾空白：base64 的标准引擎不容忍空白字符"
        );

        // url 指向的文件必须真的就在 dist/ 里（文件名拼错是常见事故）
        let file_name = url.rsplit('/').next().expect("url 里没有文件名");
        let installer = dir.join(file_name);
        assert!(
            installer.is_file(),
            "url 指向的 {} 在 dist/ 里不存在",
            installer.display()
        );

        let package = std::fs::read(&installer).expect("读取安装包失败");
        verify_like_updater(&package, signature).unwrap_or_else(|err| {
            panic!("清单里的签名通不过校验（{err}）：用户点「立即升级」时就会失败")
        });
    }

    /// `ProxyServer` 的两种写法都要认得。
    ///
    /// 这段是纯函数，正好能顺手守住几种常见配法 —— 弄错的表现是
    /// 「配了代理却还是不生效」，而环境变量本身看着是设上了的，很难查。
    #[test]
    fn normalize_proxy_handles_common_forms() {
        // 最常见：只写 host:port，所有协议共用
        assert_eq!(
            normalize_proxy("127.0.0.1:7890").as_deref(),
            Some("http://127.0.0.1:7890")
        );
        assert_eq!(
            normalize_proxy("  127.0.0.1:7890  ").as_deref(),
            Some("http://127.0.0.1:7890"),
            "两侧空白应当被忽略"
        );

        // 分协议指定：更新走 HTTPS，所以要挑 https 那条
        assert_eq!(
            normalize_proxy("http=1.2.3.4:8080;https=5.6.7.8:9090").as_deref(),
            Some("http://5.6.7.8:9090")
        );
        assert_eq!(
            normalize_proxy("HTTPS=5.6.7.8:9090").as_deref(),
            Some("http://5.6.7.8:9090"),
            "协议名不区分大小写"
        );
        assert_eq!(
            normalize_proxy("http=1.2.3.4:8080").as_deref(),
            Some("http://1.2.3.4:8080"),
            "没有 https 条目时退回 http"
        );

        // 已经带协议的就原样留用
        assert_eq!(
            normalize_proxy("https://already.example:443").as_deref(),
            Some("https://already.example:443")
        );

        // 认不出来的一律当「没有代理」，而不是拼出一个错误地址
        assert_eq!(normalize_proxy(""), None);
        assert_eq!(normalize_proxy("   "), None);
        assert_eq!(normalize_proxy("ftp=1.2.3.4:21"), None);
    }

    /// 公钥必须看起来是合法的 base64：非空、长度是 4 的倍数、字符都在字母表内。
    ///
    /// 这串是手工从 `*.key.pub` 里贴进源码的，最容易出的错就是漏字符或被换行截断
    /// —— 而那种错误要到「用户点升级」时才会以「签名验证失败」的面目暴露出来，
    /// 排查成本极高。放在单测里守住。
    #[test]
    fn pubkey_looks_like_base64() {
        assert!(!PUBKEY.is_empty(), "必须配置公钥，否则无法校验更新签名");
        assert_eq!(
            PUBKEY.len() % 4,
            0,
            "base64 长度必须是 4 的倍数，多半是粘贴时被截断了"
        );
        assert!(
            PUBKEY
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '+' || c == '/' || c == '='),
            "含有 base64 字母表之外的字符，多半是混进了换行或引号"
        );
    }

    /// 本地版本号必须能被解析成 semver，否则检查会在一开始就失败。
    #[test]
    fn current_version_is_semver() {
        Version::parse(current_version())
            .unwrap_or_else(|err| panic!("{} 不是合法 semver：{err}", current_version()));
    }

    /// 端点必须是合法的 HTTPS URL（明文 HTTP 会让整条链路失去意义）。
    #[test]
    fn endpoint_is_https() {
        let url: cargo_packager_updater::url::Url =
            ENDPOINT.parse().expect("ENDPOINT 必须能解析成 URL");
        assert_eq!(url.scheme(), "https", "更新清单必须走 HTTPS");
    }

    /// 还没初始化时读状态也不能 panic（界面可能在 `init` 之前就渲染）。
    #[test]
    fn status_before_init_is_idle() {
        assert_eq!(status(), UpdateStatus::Idle);
        assert!(!is_due(), "没有初始化时不该报告「该检查了」");
    }

    /// 状态的几个谓词要与界面按钮的启用条件一致。
    #[test]
    fn status_predicates() {
        assert!(UpdateStatus::Checking.is_busy());
        assert!(UpdateStatus::Downloading { got: 1, total: None }.is_busy());
        assert!(UpdateStatus::Installing { version: "1".into() }.is_busy());

        assert!(!UpdateStatus::Idle.is_busy());
        assert!(!UpdateStatus::UpToDate { version: "1".into() }.is_busy());
        assert!(!UpdateStatus::Failed { reason: "x".into() }.is_busy());

        assert!(UpdateStatus::Available {
            version: "1".into(),
            notes: String::new()
        }
        .can_install());

        // 只有「检查到新版本」才允许点安装；正在下载时不该重复点
        assert!(!UpdateStatus::Downloading { got: 1, total: None }.can_install());
    }

    /// 「还没有发布过版本」要翻译成人话，而不是把原始错误甩给用户。
    #[test]
    fn release_not_found_is_explained() {
        assert_eq!(
            describe_error(&UpdaterError::ReleaseNotFound),
            "远端还没有发布任何版本"
        );
    }

    /// 配置组装必须带上显式的 Windows 安装模式。
    #[test]
    fn config_uses_passive_install_mode() {
        let config = updater_config();
        assert_eq!(config.endpoints.len(), 1);
        assert_eq!(config.pubkey, PUBKEY);
        let windows = config.windows.expect("必须显式配置 Windows 安装模式");
        assert!(
            matches!(
                windows.install_mode,
                Some(WindowsUpdateInstallMode::Passive)
            ),
            "Passive 才能既无人值守、又允许安装器自行请求管理员权限"
        );
    }

    /// `now_unix` 的合理性（进程时间正常时不该是 0）
    #[test]
    fn now_unix_is_sane() {
        assert!(now_unix() > 1_600_000_000, "系统时间似乎不正常");
    }
}
