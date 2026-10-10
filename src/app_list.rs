//! 启动器应用列表：数据模型、SQLite 持久化、系统程序发现

use gpui::SharedString;
use rusqlite::{Connection, params};
use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};
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
        let mut hasher = DefaultHasher::new();
        name.to_lowercase().hash(&mut hasher);
        launch_target.unwrap_or("").to_lowercase().hash(&mut hasher);
        format!("{:016x}", hasher.finish())
    }
}

// ---------- 数据库连接（单连接，互斥保护）----------

static DB: OnceLock<Mutex<Option<Connection>>> = OnceLock::new();

fn db() -> &'static Mutex<Option<Connection>> {
    DB.get_or_init(|| {
        let conn = match Connection::open(db_path()) {
            Ok(c) => c,
            Err(e) => {
                eprintln!("[rastflow] 无法打开 SQLite 数据库: {e}");
                return Mutex::new(None);
            }
        };
        if let Err(e) = init_schema(&conn) {
            eprintln!("[rastflow] 无法初始化数据库表: {e}");
            return Mutex::new(None);
        }
        Mutex::new(Some(conn))
    })
}

fn init_schema(conn: &Connection) -> rusqlite::Result<()> {
    // 检查旧表是否存在但没有 id 列（从 sqlx 版本迁移）
    let has_id: bool = conn
        .prepare("PRAGMA table_info(app_entries)")?
        .query_map([], |row| row.get::<_, String>(1))?
        .any(|name| name.as_deref() == Ok("id"));

    if !has_id {
        // 旧表结构：备份数据，重建表
        conn.execute_batch(
            "
            CREATE TABLE IF NOT EXISTS app_entries_new (
                id TEXT PRIMARY KEY,
                name TEXT NOT NULL,
                category TEXT NOT NULL DEFAULT '',
                launch_target TEXT,
                launch_count INTEGER NOT NULL DEFAULT 0
            );
            INSERT OR IGNORE INTO app_entries_new (id, name, category, launch_target, launch_count)
                SELECT
                    lower(hex(randomblob(8))),
                    name,
                    category,
                    launch_target,
                    launch_count
                FROM app_entries;
            DROP TABLE IF EXISTS app_entries;
            ALTER TABLE app_entries_new RENAME TO app_entries;
            ",
        )?;
    } else {
        conn.execute_batch(
            "
            CREATE TABLE IF NOT EXISTS app_entries (
                id TEXT PRIMARY KEY,
                name TEXT NOT NULL,
                category TEXT NOT NULL DEFAULT '',
                launch_target TEXT,
                launch_count INTEGER NOT NULL DEFAULT 0
            );
            ",
        )?;
    }
    Ok(())
}

// ---------- 路径 ----------

pub fn db_path() -> PathBuf {
    crate::utils::app_data_dir().join("applist.db")
}

/// 用户自定义程序的类别名。
///
/// [`upsert_custom_entry`] 用它标记亲手添加的条目；设置页据此把它们从
/// 系统发现的条目里筛出来。两边引用同一个常量，改文案时不会漏改一处。
pub const CUSTOM_APP_CATEGORY: &str = "自定义程序";

// ---------- 回退数据（仅 debug 构建）----------

#[cfg(debug_assertions)]
fn fallback_entries() -> Vec<AppEntry> {
    vec![
        AppEntry::new("Visual Studio Code", "开发工具"),
        AppEntry::new("Firefox", "浏览器"),
        AppEntry::new("Chrome", "浏览器"),
        AppEntry::new("Terminal", "系统"),
        AppEntry::new("Finder", "系统"),
        AppEntry::new("Spotify", "娱乐"),
        AppEntry::new("Slack", "通讯"),
        AppEntry::new("Notion", "效率"),
        AppEntry::new("Figma", "设计"),
        AppEntry::new("Postman", "开发工具"),
        AppEntry::new("Docker", "开发工具"),
        AppEntry::new("iTerm2", "系统"),
        AppEntry::new("Obsidian", "效率"),
        AppEntry::new("Discord", "通讯"),
        AppEntry::new("Xcode", "开发工具"),
    ]
}

// ---------- 数据库读写 ----------

fn load_all_from_db() -> Vec<AppEntry> {
    let guard = match db().lock() {
        Ok(g) => g,
        Err(poisoned) => poisoned.into_inner(),
    };
    let conn = match guard.as_ref() {
        Some(c) => c,
        None => return Vec::new(),
    };

    let mut stmt = match conn.prepare(
        "SELECT id, name, category, launch_target, launch_count FROM app_entries",
    ) {
        Ok(s) => s,
        Err(_) => return Vec::new(),
    };

    let rows = stmt.query_map([], |row| {
        Ok(AppEntry {
            id: row.get::<_, String>(0)?.into(),
            name: row.get::<_, String>(1)?.into(),
            category: row.get::<_, String>(2)?.into(),
            launch_target: row.get(3)?,
            launch_count: row.get::<_, i64>(4)? as u64,
        })
    });

    match rows {
        Ok(mapped) => mapped.filter_map(|r| r.ok()).collect(),
        Err(_) => Vec::new(),
    }
}

/// 全量写回条目。写失败只记日志、不 panic：列表本身已经在内存里可用，
/// 下次启动会再写一遍；数据库被另一个实例占用时尤其容易失败。
pub fn save_entries(entries: &[AppEntry]) {
    let guard = match db().lock() {
        Ok(g) => g,
        Err(poisoned) => poisoned.into_inner(),
    };
    let conn = match guard.as_ref() {
        Some(c) => c,
        None => return,
    };

    let result = conn.execute_batch("BEGIN");
    if let Err(e) = result {
        eprintln!("[rastflow] 保存应用列表失败（BEGIN）: {e}");
        return;
    }

    let mut stmt = match conn.prepare(
        "INSERT OR REPLACE INTO app_entries (id, name, category, launch_target, launch_count)
         VALUES (?1, ?2, ?3, ?4, ?5)",
    ) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("[rastflow] 保存应用列表失败（prepare）: {e}");
            let _ = conn.execute_batch("ROLLBACK");
            return;
        }
    };

    for e in entries {
        if let Err(err) = stmt.execute(params![
            e.id.as_ref(),
            e.name.as_ref(),
            e.category.as_ref(),
            e.launch_target.as_deref(),
            e.launch_count as i64,
        ]) {
            eprintln!("[rastflow] 保存应用列表失败（insert）: {err}");
            let _ = conn.execute_batch("ROLLBACK");
            return;
        }
    }

    if let Err(e) = conn.execute_batch("COMMIT") {
        eprintln!("[rastflow] 保存应用列表失败（COMMIT）: {e}");
    }
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
    Ok(())
}

// ---------- 系统程序发现 ----------

/// 扫描系统里可启动的程序：开始菜单的快捷方式 + `shell:AppsFolder` 里的应用。
///
/// 结果会缓存几秒（见 [`discovery_cache`]），窗口反复唤出时不会每次都重扫。
#[cfg(windows)]
pub fn discover_windows_apps() -> Vec<AppEntry> {
    discovery_cache::get()
}

/// 发现结果的进程内缓存。
///
/// 枚举开始菜单与 `shell:AppsFolder` 要走文件系统和 COM，实测几十到几百毫秒，
/// 而列表在窗口每次获得焦点时都会重新取一遍，必须挡住这种重复开销。
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
                let key = name.to_lowercase();
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
    let mut names: HashSet<String> = out.iter().map(|e| e.name.to_lowercase()).collect();
    for entry in discover_apps_folder() {
        if names.insert(entry.name.to_lowercase()) {
            out.push(entry);
        }
    }

    out.sort_by(|a, b| a.name.cmp(&b.name));
    out
}

/// 枚举 `shell:AppsFolder` 里的应用。
///
/// 这类应用在开始菜单目录里没有 `.lnk`，只能通过 Shell 命名空间拿到：显示名取
/// `SIGDN_NORMALDISPLAY`（已本地化，如「计算器」），启动目标存成
/// `shell:AppsFolder\<AppID>`，由启动逻辑交给 `explorer.exe` 解析。
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
        Some(target) => target.to_lowercase(),
        None => format!("\0name:{}", entry.name.to_lowercase()),
    }
}

/// 用最新一次系统扫描的结果刷新列表：
///
/// - 新装/新发现的程序补进来；
/// - 已卸载的（扫描结果里没有的「系统程序」）被摘掉；
/// - 已存在的条目沿用原来的 `launch_count`；
/// - 用户自定义的条目（其它类别）原样保留。
#[cfg(windows)]
fn merge_discovered(stored: Vec<AppEntry>, discovered: Vec<AppEntry>) -> Vec<AppEntry> {
    use std::collections::{HashMap, HashSet};

    let mut counts: HashMap<String, u64> = HashMap::new();
    for entry in &stored {
        counts.insert(entry_key(entry), entry.launch_count);
    }

    let mut merged: Vec<AppEntry> = discovered
        .into_iter()
        .map(|mut entry| {
            if let Some(count) = counts.get(&entry_key(&entry)) {
                entry.launch_count = *count;
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
    #[cfg(debug_assertions)]
    if entries.is_empty() {
        entries = fallback_entries();
    }

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
