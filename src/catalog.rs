//! 应用目录的稳定身份与 SQLite 存储，不依赖界面。
use rusqlite::{Connection, params};

pub const SYSTEM_SOURCE: &str = "系统程序";

pub fn target_key(target: &str) -> String {
    if target.contains("://") {
        format!("uri:{target}")
    } else {
        format!("path:{}", target.replace('/', "\\").to_lowercase())
    }
}
pub fn entry_id(name: &str, target: Option<&str>) -> String {
    target
        .map(target_key)
        .unwrap_or_else(|| format!("placeholder:{}", name.to_lowercase()))
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Record {
    pub id: String,
    pub name: String,
    pub category: String,
    pub target: Option<String>,
    pub launches: u64,
}

pub struct Store(Connection);
impl Store {
    pub fn open(path: &std::path::Path) -> rusqlite::Result<Self> {
        if let Some(parent) = path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        Self::from_connection(Connection::open(path)?)
    }
    pub fn from_connection(connection: Connection) -> rusqlite::Result<Self> {
        connection.busy_timeout(std::time::Duration::from_secs(3))?;
        let exists: bool = connection.query_row(
            "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type='table' AND name='app_entries')",
            [],
            |row| row.get(0),
        )?;
        if exists {
            let has_id = connection
                .prepare("PRAGMA table_info(app_entries)")?
                .query_map([], |row| row.get::<_, String>(1))?
                .any(|column| column.as_deref() == Ok("id"));
            if !has_id {
                connection.execute_batch("BEGIN; ALTER TABLE app_entries ADD COLUMN id TEXT; UPDATE app_entries SET id=lower(hex(randomblob(16))); CREATE UNIQUE INDEX app_entries_id ON app_entries(id); COMMIT;")?;
            }
        }
        connection.execute_batch(
            "CREATE TABLE IF NOT EXISTS app_entries (
            id TEXT PRIMARY KEY, name TEXT NOT NULL, category TEXT NOT NULL DEFAULT '',
            launch_target TEXT, launch_count INTEGER NOT NULL DEFAULT 0
        ); CREATE TEMP TABLE IF NOT EXISTS incoming_ids(id TEXT PRIMARY KEY);",
        )?;
        Ok(Self(connection))
    }
    pub fn load(&self) -> rusqlite::Result<Vec<Record>> {
        self.0
            .prepare("SELECT id,name,category,launch_target,launch_count FROM app_entries")?
            .query_map([], |row| {
                Ok(Record {
                    id: row.get(0)?,
                    name: row.get(1)?,
                    category: row.get(2)?,
                    target: row.get(3)?,
                    launches: row.get::<_, i64>(4)?.max(0) as u64,
                })
            })?
            .collect()
    }
    pub fn record_launch(&mut self, id: &str) -> rusqlite::Result<()> {
        self.0.execute("UPDATE app_entries SET launch_count=MIN(launch_count+1,9223372036854775807) WHERE id=?1", [id])?;
        Ok(())
    }
    pub fn save(&mut self, entries: &[Record]) -> rusqlite::Result<()> {
        let transaction = self.0.transaction()?;
        transaction.execute("DELETE FROM incoming_ids", [])?;
        {
            let mut keep =
                transaction.prepare_cached("INSERT OR IGNORE INTO incoming_ids(id) VALUES(?1)")?;
            let mut update = transaction.prepare_cached("INSERT INTO app_entries(id,name,category,launch_target,launch_count) VALUES(?1,?2,?3,?4,?5)
                ON CONFLICT(id) DO UPDATE SET name=excluded.name,category=excluded.category,launch_target=excluded.launch_target,
                launch_count=MAX(app_entries.launch_count,excluded.launch_count)
                WHERE name!=excluded.name OR category!=excluded.category OR launch_target IS NOT excluded.launch_target OR launch_count<excluded.launch_count")?;
            for entry in entries {
                keep.execute([&entry.id])?;
                update.execute(params![
                    entry.id,
                    entry.name,
                    entry.category,
                    entry.target,
                    entry.launches.min(i64::MAX as u64) as i64
                ])?;
            }
        }
        transaction.execute(
            "DELETE FROM app_entries WHERE category=?1 AND id NOT IN (SELECT id FROM incoming_ids)",
            [SYSTEM_SOURCE],
        )?;
        transaction.commit()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn record(id: &str, category: &str, launches: u64) -> Record {
        Record {
            id: id.into(),
            name: "Editor".into(),
            category: category.into(),
            target: Some(format!(r"C:\{id}.exe")),
            launches,
        }
    }
    #[test]
    fn names_are_not_part_of_launchable_identity() {
        assert_eq!(
            entry_id("Old", Some(r"C:\Editor.exe")),
            entry_id("New", Some("c:/editor.EXE"))
        );
        assert_ne!(
            entry_id("Editor", Some(r"C:\a.exe")),
            entry_id("Editor", Some(r"D:\a.exe"))
        );
        assert_ne!(
            target_key("https://example.com/A"),
            target_key("https://example.com/a")
        );
    }
    #[test]
    fn fresh_database_and_incremental_updates_preserve_counts_and_custom_entries() {
        let mut store = Store::from_connection(Connection::open_in_memory().unwrap()).unwrap();
        assert!(store.load().unwrap().is_empty());
        store
            .save(&[
                record("system", SYSTEM_SOURCE, 7),
                record("custom", "自定义", 2),
            ])
            .unwrap();
        store.save(&[record("system", SYSTEM_SOURCE, 1)]).unwrap();
        let entries = store.load().unwrap();
        assert_eq!(
            entries
                .iter()
                .find(|entry| entry.id == "system")
                .unwrap()
                .launches,
            7
        );
        assert!(entries.iter().any(|entry| entry.id == "custom"));
        store.save(&[]).unwrap();
        assert_eq!(store.load().unwrap(), vec![record("custom", "自定义", 2)]);
    }
    #[test]
    fn migration_keeps_existing_user_records() {
        let connection = Connection::open_in_memory().unwrap();
        connection.execute_batch("CREATE TABLE app_entries(name TEXT,category TEXT,launch_target TEXT,launch_count INTEGER); INSERT INTO app_entries VALUES('Old','自定义','C:\\old.exe',4);").unwrap();
        let store = Store::from_connection(connection).unwrap();
        let entries = store.load().unwrap();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].launches, 4);
        assert!(!entries[0].id.is_empty());
    }
}
