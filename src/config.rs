use gpui::SharedString;
use sqlx::{Row, SqlitePool, sqlite::SqliteConnectOptions};
use std::path::PathBuf;
use std::sync::OnceLock;

// ---------- 数据结构 ----------

#[derive(Clone)]
pub struct AppEntry {
    pub name: SharedString,
    pub category: SharedString,
    pub launch_target: Option<String>,
    pub launch_count: u64,
}

impl AppEntry {
    pub fn new(name: &str, category: &str) -> Self {
        Self {
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
        Self {
            name: name.into(),
            category: category.into(),
            launch_target,
            launch_count: 0,
        }
    }

}

// ---------- 运行时 & 连接池（单连接，无线程池）----------

static RT: OnceLock<tokio::runtime::Runtime> = OnceLock::new();
static POOL: OnceLock<SqlitePool> = OnceLock::new();

fn runtime() -> &'static tokio::runtime::Runtime {
    RT.get_or_init(|| {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("failed to build tokio runtime")
    })
}

fn pool() -> &'static SqlitePool {
    POOL.get_or_init(|| {
        runtime().block_on(async {
            let opts = SqliteConnectOptions::new()
                .filename(db_path())
                .create_if_missing(true);
            let pool = sqlx::sqlite::SqlitePoolOptions::new()
                .max_connections(1)
                .connect_with(opts)
                .await
                .expect("failed to open sqlite db");
            sqlx::query(
                "CREATE TABLE IF NOT EXISTS app_entries (
                    name         TEXT    NOT NULL,
                    category     TEXT    NOT NULL DEFAULT '',
                    launch_target TEXT,
                    launch_count INTEGER NOT NULL DEFAULT 0
                )",
            )
            .execute(&pool)
            .await
            .expect("failed to create app_entries table");
            pool
        })
    })
}

// ---------- 路径 ----------

pub fn db_path() -> PathBuf {
    std::env::current_dir()
        .unwrap_or_else(|_| PathBuf::from("."))
        .join("applist.db")
}

// ---------- 默认数据 ----------

fn default_test_entries() -> Vec<AppEntry> {
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
    let p = pool(); // 必须在 block_on 外部获取，避免嵌套 block_on panic
    runtime().block_on(async move {
        let rows = sqlx::query(
            "SELECT name, category, launch_target, launch_count FROM app_entries",
        )
        .fetch_all(p)
        .await
        .unwrap_or_default();

        rows.into_iter()
            .map(|row| AppEntry {
                name: row.get::<String, _>("name").into(),
                category: row.get::<String, _>("category").into(),
                launch_target: row.get("launch_target"),
                launch_count: row.get::<i64, _>("launch_count") as u64,
            })
            .collect()
    })
}

pub fn save_entries(entries: &[AppEntry]) {
    let p = pool(); // 必须在 block_on 外部获取，避免嵌套 block_on panic
    let owned: Vec<AppEntry> = entries.to_vec();
    runtime().block_on(async move {
        let mut tx = p.begin().await.expect("begin transaction");
        sqlx::query("DELETE FROM app_entries")
            .execute(&mut *tx)
            .await
            .expect("delete entries");
        for e in &owned {
            sqlx::query(
                "INSERT INTO app_entries (name, category, launch_target, launch_count)
                 VALUES (?, ?, ?, ?)",
            )
            .bind(e.name.as_ref())
            .bind(e.category.as_ref())
            .bind(e.launch_target.as_deref())
            .bind(e.launch_count as i64)
            .execute(&mut *tx)
            .await
            .expect("insert entry");
        }
        tx.commit().await.expect("commit transaction");
    });
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
        "自定义程序".to_string()
    } else {
        category.trim().to_string()
    };

    let mut entries = load_or_discover_entries();
    if let Some(existing) = entries.iter_mut().find(|e| {
        e.launch_target
            .as_deref()
            .is_some_and(|t| t.eq_ignore_ascii_case(launch_target))
    }) {
        existing.name = resolved_name.into();
        existing.category = resolved_category.into();
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

#[cfg(windows)]
pub fn discover_windows_apps() -> Vec<AppEntry> {
    use std::collections::HashSet;
    use std::fs;
    use std::path::Path;

    fn should_keep(name: &str) -> bool {
        let lowered = name.to_lowercase();
        !lowered.is_empty() && !lowered.contains("uninstall") && !lowered.contains("卸载")
    }

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
                if !should_keep(name) {
                    continue;
                }
                let key = name.to_lowercase();
                if seen.insert(key) {
                    out.push(AppEntry::with_launch_target(
                        name,
                        "系统程序",
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
    out.sort_by(|a, b| a.name.cmp(&b.name));
    out
}

#[cfg(not(windows))]
pub fn discover_windows_apps() -> Vec<AppEntry> {
    Vec::new()
}

// ---------- 加载入口 ----------

/// 仅从数据库加载已保存的条目，不触发系统发现。
pub fn load_entries_from_file() -> Vec<AppEntry> {
    let entries = load_all_from_db();
    if entries.is_empty() {
        return Vec::new();
    }
    entries
}

pub fn load_or_discover_entries() -> Vec<AppEntry> {
    let entries = load_all_from_db();
    if !entries.is_empty() {
        #[cfg(windows)]
        {
            let has_launch_target = entries.iter().any(|e| e.launch_target.is_some());
            if !has_launch_target {
                let discovered = discover_windows_apps();
                if !discovered.is_empty() {
                    save_entries(&discovered);
                    return discovered;
                }
            }
        }
        let mut entries = entries;
        sort_entries_by_launch(&mut entries);
        return entries;
    }

    let mut entries = discover_windows_apps();
    if entries.is_empty() {
        entries = default_test_entries();
    }

    sort_entries_by_launch(&mut entries);
    save_entries(&entries);
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

pub fn entry_identity(entry: &AppEntry) -> (String, String) {
    (
        entry.name.to_string(),
        entry.launch_target.clone().unwrap_or_default(),
    )
}
