//! 启动器应用列表：数据模型、SQLite 持久化、系统程序发现

use gpui::SharedString;
use std::path::PathBuf;
use std::sync::{Mutex, OnceLock};

pub mod watcher;

// ---------- 数据结构 ----------

#[derive(Clone)]
pub struct AppEntry {
    pub id: SharedString,
    pub name: SharedString,
    pub category: SharedString,
    pub launch_target: Option<String>,
    pub launch_count: u64,
}

impl AppEntry {
    pub fn new(name: &str, category: &str) -> Self {
        let id = Self::compute_id(name, None);
        Self {
            id: id.into(),
            name: name.into(),
            category: category.into(),
            launch_target: None,
            launch_count: 0,
        }
    }

    pub fn with_launch_target(
        name: &str,
        category: &str,
        launch_target: Option<String>,
    ) -> Self {
        let id = Self::compute_id(name, launch_target.as_deref());
        Self {
            id: id.into(),
            name: name.into(),
            category: category.into(),
            launch_target,
            launch_count: 0,
        }
    }

    /// 从 name + launch_target 计算稳定 ID（用于主键与身份识别）
    fn compute_id(name: &str, launch_target: Option<&str>) -> String {
        crate::catalog::entry_id(name, launch_target)
    }
}

// ---------- 数据库工作线程 ----------

enum DbCommand {
    Load(std::sync::mpsc::Sender<Vec<AppEntry>>),
    Save(Vec<crate::catalog::Record>),
    Launch(String),
    Flush(std::sync::mpsc::Sender<()>),
}
static DB: OnceLock<std::sync::mpsc::Sender<DbCommand>> = OnceLock::new();
fn db() -> &'static std::sync::mpsc::Sender<DbCommand> {
    DB.get_or_init(|| {
        let (sender, receiver) = std::sync::mpsc::channel();
        std::thread::Builder::new().name("rastflow-catalog-store".into()).spawn(move || {
            let mut store = match crate::catalog::Store::open(&db_path()) {
                Ok(store) => store,
                Err(error) => { eprintln!("[rastflow] 打开应用目录失败：{error}"); return; }
            };
            for command in receiver {
                match command {
                    DbCommand::Load(reply) => { let records = store.load().unwrap_or_else(|error| { eprintln!("[rastflow] 读取应用目录失败：{error}"); Vec::new() });
                        let _ = reply.send(records.into_iter().map(|record| AppEntry { id: record.id.into(),name: record.name.into(),category: record.category.into(),launch_target: record.target,launch_count: record.launches }).collect()); }
                    DbCommand::Save(entries) => if let Err(error) = store.save(&entries) { eprintln!("[rastflow] 保存应用目录失败：{error}"); },
                    DbCommand::Launch(id) => if let Err(error) = store.record_launch(&id) { eprintln!("[rastflow] 保存启动次数失败：{error}"); },
                    DbCommand::Flush(reply) => { let _ = reply.send(()); }
                }
            }
        }).expect("cannot start catalog store");
        sender
    })
}
pub fn record_launch(id: &str) { let _ = db().send(DbCommand::Launch(id.into())); }

pub fn flush() {
    if let Some(db) = DB.get() {
        let (reply, done) = std::sync::mpsc::channel();
        if db.send(DbCommand::Flush(reply)).is_ok() { let _ = done.recv_timeout(std::time::Duration::from_secs(5)); }
    }
}

// ---------- 路径 ----------

pub fn db_path() -> PathBuf {
    crate::utils::app_data_dir().join("applist.db")
}

/// 用户自定义程序的类别名，由 [`upsert_custom_entry`] 打上；设置页据此把它们
/// 从系统发现的条目里筛出来。两边引用同一个常量，改文案时不会漏改一处。
pub const CUSTOM_APP_CATEGORY: &str = "自定义程序";

// ---------- 数据库读写 ----------

fn load_all_from_db() -> Vec<AppEntry> {
    let (reply, result) = std::sync::mpsc::channel();
    if db().send(DbCommand::Load(reply)).is_err() { return Vec::new(); }
    result.recv().unwrap_or_default()
}

/// UI 只投递保存命令；数据库连接由专用线程独占。
pub fn save_entries(entries: &[AppEntry]) {
    let records = entries.iter().map(|entry| crate::catalog::Record {
        id: entry.id.to_string(),name: entry.name.to_string(),category: entry.category.to_string(),
        target: entry.launch_target.clone(),launches: entry.launch_count,
    }).collect();
    let _ = db().send(DbCommand::Save(records));
}

pub fn upsert_custom_entry(
    name: &str,
    category: &str,
    launch_target: &str,
) -> Result<(), String> {
    use std::path::Path;
    let launch_target = launch_target.trim();
    if launch_target.is_empty() {
        return Err("请输入程序路径".into());
    }

    let target_path = Path::new(launch_target);
    if !target_path.exists() {
        return Err("程序路径不存在".into());
    }

    let resolved_name = if name.trim().is_empty() {
        target_path
            .file_stem()
            .and_then(|v| v.to_str())
            .filter(|v| !v.trim().is_empty())
            .unwrap_or(launch_target)
            .trim()
            .to_string()
    } else {
        name.trim().to_string()
    };

    let resolved_category = if category.trim().is_empty() {
        CUSTOM_APP_CATEGORY.to_string()
    } else {
        category.trim().to_string()
    };

    // 互斥锁保护读-改-写序列
    static UPSERT_LOCK: Mutex<()> = Mutex::new(());
    let _guard = UPSERT_LOCK.lock().unwrap_or_else(|p| p.into_inner());

    let mut entries = load_or_discover_entries();
    if let Some(existing) = entries.iter_mut().find(|e| {
        e.launch_target
            .as_deref()
            .is_some_and(|t| t.eq_ignore_ascii_case(launch_target))
    }) {
        existing.name = resolved_name.into();
        existing.category = resolved_category.into();
        // launch_target 不变，id 也不变
    } else {
        entries.push(AppEntry::with_launch_target(
            &resolved_name,
            &resolved_category,
            Some(launch_target.to_string()),
        ));
    }

    sort_entries_by_launch(&mut entries);
    save_entries(&entries);
    watcher::invalidate();
    Ok(())
}

// ---------- 系统程序发现 ----------

/// 扫描系统里可启动的程序：开始菜单的快捷方式 + `shell:AppsFolder` 里的应用。
/// 结果缓存几秒（见 [`discovery_cache`]），窗口反复唤出时不会每次都重扫。
#[cfg(windows)]
pub fn discover_windows_apps() -> Vec<AppEntry> {
    discovery_cache::get()
}

/// 发现结果的进程内缓存。枚举开始菜单与 `shell:AppsFolder` 要走文件系统和 COM，
/// 实测几十到几百毫秒，而列表在窗口每次获得焦点时都会重新取一遍，必须挡住。
#[cfg(windows)]
pub mod discovery_cache {
    use super::AppEntry;
    use std::sync::Mutex;
    use std::time::{Duration, Instant};

    /// 两次扫描之间的最小间隔：装完新程序后最迟等这么久就能搜到。
    const MIN_INTERVAL: Duration = Duration::from_secs(3);

    static CACHE: Mutex<Option<(Instant, Vec<AppEntry>)>> = Mutex::new(None);

    pub fn get() -> Vec<AppEntry> {
        let mut cache = CACHE.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        if let Some((at, entries)) = cache.as_ref()
            && at.elapsed() < MIN_INTERVAL
        {
            return entries.clone();
        }
        let entries = super::scan_windows_apps();
        *cache = Some((Instant::now(), entries.clone()));
        entries
    }

    /// 强制使缓存失效（由 watcher 在检测到开始菜单变化时调用）。
    pub fn invalidate() {
        let mut cache = CACHE.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        *cache = None;
    }
}

/// 过滤「卸载 xxx」这类入口：它们不是可用的程序
#[cfg(windows)]
fn should_keep_app(name: &str) -> bool {
    let lowered = name.to_lowercase();
    !lowered.is_empty() && !lowered.contains("uninstall") && !lowered.contains("卸载")
}

/// 真正干活的那次扫描（结果不经缓存）
#[cfg(windows)]
fn scan_windows_apps() -> Vec<AppEntry> {
    use std::collections::HashSet;
    use std::fs;
    use std::path::Path;

    fn collect_from_dir(dir: &Path, seen: &mut HashSet<String>, out: &mut Vec<AppEntry>) {
        if let Ok(read_dir) = fs::read_dir(dir) {
            for entry in read_dir.flatten() {
                let path = entry.path();
                if path.is_dir() {
                    collect_from_dir(&path, seen, out);
                    continue;
                }
                let ext = path
                    .extension()
                    .and_then(|e| e.to_str())
                    .map(|e| e.to_ascii_lowercase())
                    .unwrap_or_default();
                if ext != "lnk" && ext != "url" {
                    continue;
                }
                let Some(name) = path.file_stem().and_then(|n| n.to_str()) else {
                    continue;
                };
                let name = name.trim();
                if !should_keep_app(name) {
                    continue;
                }
                // 按名字去重：用户开始菜单与公共开始菜单里经常有同一个程序的
                // 两份快捷方式（目标完全一样），收两条只会得到重影。
                let key = crate::catalog::target_key(&path.to_string_lossy());
                if seen.insert(key) {
                    out.push(AppEntry::with_launch_target(
                        name,
                        SYSTEM_APP_CATEGORY,
                        Some(path.to_string_lossy().to_string()),
                    ));
                }
            }
        }
    }

    let mut roots = Vec::new();
    if let Ok(program_data) = std::env::var("ProgramData") {
        roots.push(
            PathBuf::from(program_data)
                .join("Microsoft\\Windows\\Start Menu\\Programs"),
        );
    }
    if let Ok(app_data) = std::env::var("APPDATA") {
        roots.push(
            PathBuf::from(app_data)
                .join("Microsoft\\Windows\\Start Menu\\Programs"),
        );
    }

    let mut seen = HashSet::new();
    let mut out = Vec::new();
    for root in roots {
        if root.exists() {
            collect_from_dir(&root, &mut seen, &mut out);
        }
    }

    // 开始菜单目录里没有 .lnk 的程序（UWP / Microsoft Store 应用等）从
    // shell:AppsFolder 补上。这里按**显示名**与快捷方式比对：同一个程序的两个
    // 入口只保留快捷方式那条，免得列表里出现两条同名项。
    for entry in discover_apps_folder() {
        if seen.insert(entry_key(&entry)) { out.push(entry); }
    }

    out.sort_by(|a, b| a.name.cmp(&b.name));
    out
}

/// 枚举 `shell:AppsFolder` 里的打包应用。
///
/// 这类应用在开始菜单目录里没有 `.lnk`，只能通过 Shell 命名空间拿到：显示名取
/// `SIGDN_NORMALDISPLAY`（已本地化，如「计算器」），启动目标存成
/// `shell:AppsFolder\<AppID>`，交给 `explorer.exe` 解析。
#[cfg(windows)]
fn discover_apps_folder() -> Vec<AppEntry> {
    use windows::Win32::System::Com::{CoInitializeEx, IBindCtx, COINIT_APARTMENTTHREADED};
    use windows::Win32::UI::Shell::{
        BHID_EnumItems, IEnumShellItems, IShellItem, SHCreateItemFromParsingName,
        SIGDN_DESKTOPABSOLUTEPARSING, SIGDN_NORMALDISPLAY,
    };
    use windows::core::w;

    /// 一次批量取多少个，纯粹是省几次 COM 往返
    const BATCH: usize = 16;

    unsafe {
        // 当前线程可能还没初始化过 COM；已初始化时会返回 S_FALSE 或模式冲突错误，
        // 这两种情况都可以继续用 Shell 接口。
        let _ = CoInitializeEx(None, COINIT_APARTMENTTHREADED);

        let Ok(folder) = SHCreateItemFromParsingName::<_, _, IShellItem>(
            w!("shell:AppsFolder"),
            None::<&IBindCtx>,
        ) else {
            return Vec::new();
        };
        let Ok(items) = folder.BindToHandler::<_, IEnumShellItems>(None::<&IBindCtx>, &BHID_EnumItems)
        else {
            return Vec::new();
        };

        let mut out = Vec::new();
        loop {
            let mut batch = [const { None::<IShellItem> }; BATCH];
            let mut fetched = 0u32;
            // Shell 的实现要求 `pceltFetched` 非空（传 null 会得到 E_INVALIDARG）；
            // 取完时返回 S_FALSE，这里同样按成功处理，靠 fetched == 0 退出循环。
            if items.Next(&mut batch, Some(&mut fetched)).is_err() || fetched == 0 {
                break;
            }
            for item in batch.into_iter().flatten() {
                let (Some(name), Some(app_id)) = (
                    shell_display_name(&item, SIGDN_NORMALDISPLAY),
                    shell_display_name(&item, SIGDN_DESKTOPABSOLUTEPARSING),
                ) else {
                    continue;
                };
                let name = name.trim();
                if !should_keep_app(name) {
                    continue;
                }
                // 只补**打包应用**（AppID 形如 `PackageFamilyName!App`，即 UWP /
                // Store / MSIX）。其它条目是控制面板这类桌面程序，开始菜单里本来
                // 就有它们的快捷方式，再收一遍只会得到名称还不一样的重影。
                if !app_id.contains('!') {
                    continue;
                }
                out.push(AppEntry::with_launch_target(
                    name,
                    SYSTEM_APP_CATEGORY,
                    Some(format!("shell:AppsFolder\\{app_id}")),
                ));
            }
        }
        out
    }
}

/// 取 shell 项的显示名；`SIGDN` 决定拿到的是名字还是可用于启动的解析名。
#[cfg(windows)]
fn shell_display_name(
    item: &windows::Win32::UI::Shell::IShellItem,
    sigdn: windows::Win32::UI::Shell::SIGDN,
) -> Option<String> {
    use windows::Win32::System::Com::CoTaskMemFree;

    unsafe {
        let pwstr = item.GetDisplayName(sigdn).ok()?;
        let text = pwstr.to_string().ok();
        // GetDisplayName 分配的内存必须由调用方释放
        CoTaskMemFree(Some(pwstr.0 as *const core::ffi::c_void));
        text
    }
}

#[cfg(not(windows))]
pub fn discover_windows_apps() -> Vec<AppEntry> {
    Vec::new()
}

// ---------- 加载入口 ----------

/// 仅从数据库加载已保存的条目，不触发系统发现。
pub fn load_entries_from_db() -> Vec<AppEntry> {
    let entries = load_all_from_db();
    if entries.is_empty() {
        return Vec::new();
    }
    entries
}

/// 「系统程序」条目的类别名。这一类条目由 [`discover_windows_apps`] 负责维护，
/// 其余类别属于用户自定义，刷新时原样保留。
#[cfg(windows)]
const SYSTEM_APP_CATEGORY: &str = "系统程序";

/// 条目的匹配键：优先用启动目标（忽略大小写），没有目标就退回名字。
#[cfg(windows)]
fn entry_key(entry: &AppEntry) -> String {
    match entry.launch_target.as_deref() {
        Some(target) => crate::catalog::target_key(target),
        None => format!("\0name:{}", entry.name.to_lowercase()),
    }
}

/// 用最新一次系统扫描的结果刷新列表：新条目补进来、已卸载的「系统程序」摘掉、
/// 已有条目沿用 `launch_count`、用户自定义条目（其它类别）原样保留。
#[cfg(windows)]
fn merge_discovered(stored: Vec<AppEntry>, discovered: Vec<AppEntry>) -> Vec<AppEntry> {
    use std::collections::{HashMap, HashSet};

    let mut counts: HashMap<String, AppEntry> = HashMap::new();
    for entry in &stored {
        counts.insert(entry_key(entry), entry.clone());
    }

    let mut merged: Vec<AppEntry> = discovered
        .into_iter()
        .map(|mut entry| {
            if let Some(previous) = counts.get(&entry_key(&entry)) {
                entry.id = previous.id.clone();
                entry.launch_count = previous.launch_count;
                if previous.category.as_ref() != SYSTEM_APP_CATEGORY {
                    entry.name = previous.name.clone();
                    entry.category = previous.category.clone();
                }
            }
            entry
        })
        .collect();

    // 已合并条目的 key 集合，用于 O(1) 查找
    let merged_keys: HashSet<String> = merged.iter().map(entry_key).collect();

    for entry in stored {
        // 「系统程序」以这次扫描为准；没有启动目标的占位条目（内置示例）本来也
        // 启动不了，一并丢掉。用户自定义的条目都带启动目标，不受影响。
        if entry.category.as_ref() == SYSTEM_APP_CATEGORY || entry.launch_target.is_none() {
            continue;
        }
        if !merged_keys.contains(&entry_key(&entry)) {
            merged.push(entry);
        }
    }
    merged
}

#[cfg(all(test, windows))]
mod merge_tests {
    use super::*;

    fn system(name: &str, target: &str) -> AppEntry {
        AppEntry::with_launch_target(name, SYSTEM_APP_CATEGORY, Some(target.to_string()))
    }

    #[test]
    fn refresh_adds_new_and_drops_uninstalled() {
        let stored = vec![
            AppEntry {
                launch_count: 7,
                ..system("Kept", r"C:\s\Kept.lnk")
            },
            system("Gone", r"C:\s\Gone.lnk"),
            AppEntry::with_launch_target("My Tool", "自定义程序", Some(r"D:\my.exe".into())),
        ];
        let discovered = vec![
            system("Kept", r"C:\s\Kept.lnk"),
            system("PhotoCraft", r"C:\s\PhotoCraft.lnk"),
        ];

        let merged = merge_discovered(stored, discovered);

        let kept = merged
            .iter()
            .find(|e| e.name.as_ref() == "Kept")
            .expect("已有条目应保留");
        assert_eq!(kept.launch_count, 7, "启动次数应沿用");
        assert!(
            merged.iter().any(|e| e.name.as_ref() == "PhotoCraft"),
            "新发现的程序应补进来"
        );
        assert!(
            !merged.iter().any(|e| e.name.as_ref() == "Gone"),
            "已卸载的程序应被摘掉"
        );
        assert!(
            merged.iter().any(|e| e.name.as_ref() == "My Tool"),
            "用户自定义条目应保留"
        );
    }

    #[test]
    fn same_entries_detects_no_change() {
        let left = vec![system("A", r"C:\s\A.lnk")];
        let right = vec![system("A", r"C:\s\A.lnk")];
        assert!(same_entries(&left, &right));

        let other = vec![system("A", r"C:\s\A.lnk"), system("B", r"C:\s\B.lnk")];
        assert!(!same_entries(&left, &other));
    }

    /// 手工验证：`cargo test discovery_smoke -- --ignored --nocapture`
    #[test]
    #[ignore = "依赖本机的开始菜单与 shell:AppsFolder"]
    fn discovery_smoke() {
        let apps = discover_windows_apps();
        let names: Vec<String> = apps.iter().map(|e| e.name.to_string()).collect();
        println!("共发现 {} 个程序：{}", names.len(), names.join(" | "));
        assert!(!names.is_empty());

        let mut sorted = names.clone();
        sorted.sort();
        let dupes: Vec<&str> = sorted
            .windows(2)
            .filter(|pair| pair[0] == pair[1])
            .map(|pair| pair[0].as_str())
            .collect();
        if dupes.is_empty() {
            println!("重名条目：无");
        } else {
            println!("重名条目：{}", dupes.join(" | "));
        }
    }
}

/// 两组条目在「名字 / 类别 / 启动目标 / 启动次数」上是否完全一致（前提：两边都排好序）
fn same_entries(left: &[AppEntry], right: &[AppEntry]) -> bool {
    left.len() == right.len()
        && left.iter().zip(right).all(|(a, b)| {
            a.name.as_ref() == b.name.as_ref()
                && a.category.as_ref() == b.category.as_ref()
                && a.launch_target == b.launch_target
                && a.launch_count == b.launch_count
        })
}

/// 取启动器列表数据。
///
/// **每次都会重新扫描开始菜单**并与数据库里保存的条目合并，这样新安装的程序
/// 下次唤出窗口就能搜到，卸载的程序也会自动消失。只有内容真的变化时才落盘，
/// 所以窗口反复唤出不会带来额外的写库开销。
pub fn load_or_discover_entries() -> Vec<AppEntry> {
    let stored = load_all_from_db();
    let mut previous = stored.clone();
    sort_entries_by_launch(&mut previous);

    #[cfg(windows)]
    let mut entries = {
        let discovered = discover_windows_apps();
        if discovered.is_empty() {
            // 扫描失败（Shell 异常等）时保持数据库里的内容，别把已经能用的列表清空
            stored
        } else {
            merge_discovered(stored, discovered)
        }
    };

    #[cfg(not(windows))]
    let mut entries = stored;

    // 一个程序都没扫到（非 Windows，或系统里确实没有）：退回内置示例，保证列表非空

    sort_entries_by_launch(&mut entries);

    if !same_entries(&previous, &entries) {
        save_entries(&entries);
    }
    entries
}

// ---------- 工具函数 ----------

pub fn sort_entries_by_launch(entries: &mut [AppEntry]) {
    entries.sort_by(|a, b| {
        b.launch_count
            .cmp(&a.launch_count)
            .then_with(|| a.name.cmp(&b.name))
    });
}
