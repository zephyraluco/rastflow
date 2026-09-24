//! 常驻内存的文件索引：数据结构、快照编解码，以及从 MFT 建它。
//!
//! ```text
//! names   : Vec<u8>    名称池，所有名字的 UTF-8 字节首尾相接
//! entries : Vec<Entry> 定长 24 字节的记录数组，全部盘混在一个数组里
//! tables  : 盘 → (MFT 记录号 → 槽位)
//! ```
//!
//! 每个条目只存自己的名字与父目录的 MFT 记录号，完整路径由 [`FileIndex::path_into`]
//! 沿途拼出。快照是单文件，`save` 时顺带压实（丢掉已删槽位、重排名称池）。
//!
//! | 部分 | 需要管理员？ |
//! | --- | --- |
//! | [`FileIndex`] 与它的操作 | 不需要 |
//! | 快照（[`FileIndex::save`] / [`FileIndex::load`]） | 不需要 |
//! | [`rebuild_all`] / [`rebuild_disk`] | **需要** —— 要读 `\\.\X:` 枚举 MFT |

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::Instant;

use super::pathutil::{self, IgnoreRules};
use super::priority::{DIR_PRIORITY, PriorityTable};
use super::win::disk;
use super::win::usn::{is_root, mft_index, UsnRecord, ROOT_MFT_INDEX};
use super::win::volume::Volume;
use super::{CoreError, MAX_PATH_DEPTH, Result};

/// 「没有父」：这个条目是某个盘的根（名字形如 `C:`）
pub const NO_PARENT: u32 = u32::MAX;

/// 查找表里的空槽
const NO_SLOT: u32 = u32::MAX;

/// `Entry::flags` 的位
const FLAG_DEAD: u16 = 1 << 1;
/// 盘符编码（`'A'` → 0）占 `flags` 的高 5 位
const DISK_SHIFT: u16 = 8;
const DISK_MASK: u16 = 0x1F;

fn disk_code(disk: char) -> u16 {
    let letter = disk.to_ascii_uppercase();
    debug_assert!(letter.is_ascii_uppercase(), "盘符必须是 A-Z");
    u16::from(letter as u8 - b'A')
}

fn disk_letter(code: u16) -> char {
    char::from(b'A' + (code & DISK_MASK) as u8)
}

/// 查找表的最大长度（= MFT 记录号上限）。超过它的记录号跳过登记，
/// 避免损坏的 FRN 让 `slots` 去申请 TB 级内存。
const MAX_MFT_INDEX: usize = 64 * 1024 * 1024;

/// 快照文件里的魔数
const MAGIC: &[u8; 8] = b"RASTIDX1";

/// 快照格式版本。`Entry` 布局或段顺序变了就要加一（旧文件会被丢弃重建）
pub const SNAPSHOT_VERSION: u32 = 1;

/// 快照文件名（放在索引目录下，是**唯一**的索引文件）
pub const SNAPSHOT_FILE: &str = "index.dat";

/// 一条索引记录。定长 24 字节，无填充。
///
/// `#[repr(C)]` + 全定长整数字段 ⇒ 可整体按 24 字节读写快照（有 `size_of` 断言）。
#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Entry {
    /// 名字在名称池里的起始偏移
    pub name_off: u32,
    /// 名字的字节长度（UTF-8）
    pub name_len: u16,
    /// [`FLAG_DEAD`] 与盘符编码
    pub flags: u16,
    /// **父目录的 MFT 记录号**；[`NO_PARENT`] 表示这是盘根
    pub parent: u32,
    /// 后缀优先级；目录恒为 [`DIR_PRIORITY`]
    pub priority: i32,
    /// 文件引用号 `(序列号 << 48) | 记录号`，用于识别 MFT 记录复用
    pub frn: u64,
}

const _: () = assert!(std::mem::size_of::<Entry>() == 24);
const _: () = assert!(std::mem::align_of::<Entry>() == 8);

impl Entry {
    pub fn is_dead(&self) -> bool {
        self.flags & FLAG_DEAD != 0
    }

    fn is_live(&self) -> bool {
        !self.is_dead()
    }

    /// 这条记录属于哪个盘
    pub fn disk(&self) -> char {
        disk_letter((self.flags >> DISK_SHIFT) & DISK_MASK)
    }
}

/// 一个盘的查找表
struct DiskTable {
    /// 下标是 MFT 记录号，值是 `entries` 里的槽位；[`NO_SLOT`] 表示没有
    slots: Vec<u32>,
    /// 盘根条目（名字形如 `C:`）的槽位
    root: u32,
}

/// 一个盘的 USN 位点
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct JournalMark {
    /// 已经处理到的 USN（不含）
    pub last_usn: i64,
    /// 当时的 USN 日志 id
    pub journal_id: u64,
}

/// 路径拼接用的临时缓冲，避免热路径上反复分配
#[derive(Default)]
pub struct PathScratch {
    segments: Vec<u32>,
}

impl PathScratch {
    pub fn new() -> Self {
        Self {
            segments: Vec::with_capacity(32),
        }
    }
}

/// 常驻内存的文件索引
pub struct FileIndex {
    names: Vec<u8>,
    entries: Vec<Entry>,
    /// 可复用的槽位（已删除条目的位置）
    free: Vec<u32>,
    /// 存活条目数
    live: u32,
    tables: HashMap<char, DiskTable>,
}

impl Default for FileIndex {
    fn default() -> Self {
        Self::new()
    }
}

impl FileIndex {
    pub fn new() -> Self {
        Self {
            names: Vec::new(),
            entries: Vec::new(),
            free: Vec::new(),
            live: 0,
            tables: HashMap::new(),
        }
    }

    pub fn is_empty(&self) -> bool {
        self.live == 0
    }

    /// 已索引的盘
    pub fn disks(&self) -> impl Iterator<Item = char> + '_ {
        self.tables.keys().copied()
    }

    // ── 名字池 ─────────────────────────────────────────────────────────

    fn name_range(&self, entry: &Entry) -> Option<(usize, usize)> {
        let start = entry.name_off as usize;
        let end = start.checked_add(entry.name_len as usize)?;
        if end > self.names.len() {
            return None;
        }
        Some((start, end))
    }

    /// 某个槽位的名字原始字节（UTF-8）。匹配热路径用它，避免 UTF-8 校验
    pub fn name_bytes(&self, slot: u32) -> Option<&[u8]> {
        let entry = self.entries.get(slot as usize)?;
        let (start, end) = self.name_range(entry)?;
        Some(&self.names[start..end])
    }

    /// 把名字追加进池，返回 `(偏移, 长度)`
    fn push_name(&mut self, name: &str) -> (u32, u16) {
        let offset = self.names.len() as u32;
        self.names.extend_from_slice(name.as_bytes());
        // 单个名字不可能超过 65535 字节（NTFS 文件名上限 255 个字符）
        let len = name.len().min(u16::MAX as usize) as u16;
        (offset, len)
    }

    // ── 槽位 ───────────────────────────────────────────────────────────

    fn alloc_slot(&mut self, entry: Entry) -> u32 {
        match self.free.pop() {
            Some(slot) => {
                self.entries[slot as usize] = entry;
                self.live += 1;
                slot
            }
            None => {
                self.entries.push(entry);
                self.live += 1;
                self.entries.len() as u32 - 1
            }
        }
    }

    /// 标记某个槽位为死，可被后续插入复用
    fn free_slot(&mut self, slot: u32) -> bool {
        let Some(entry) = self.entries.get_mut(slot as usize) else {
            return false;
        };
        if entry.is_dead() {
            return false;
        }
        entry.flags |= FLAG_DEAD;
        // 名字池里的字节先留着（无法回收单条），保存快照时会整体重排
        self.free.push(slot);
        self.live -= 1;
        true
    }

    /// 某个盘的查找表，不存在就建
    fn table_mut(&mut self, disk: char) -> &mut DiskTable {
        let disk = disk.to_ascii_uppercase();
        let root_slot = NO_SLOT;
        self.tables.entry(disk).or_insert_with(|| DiskTable {
            slots: Vec::new(),
            root: root_slot,
        })
    }

    /// 记录 `MFT 记录号 → 槽位`
    fn set_slot(&mut self, disk: char, frn: u64, slot: u32) {
        let index = mft_index(frn) as usize;
        if index >= MAX_MFT_INDEX {
            return; // 见 [`MAX_MFT_INDEX`] 的说明：宁可搜不到，也不能把内存申请爆
        }
        let table = self.table_mut(disk);
        if index >= table.slots.len() {
            // 按需扩张：记录号是顺序分配的，不会出现天文数字的下标（上面那道上限挡着）
            table.slots.resize(index + 1, NO_SLOT);
        }
        table.slots[index] = slot;
    }

    /// 按 MFT 记录号取槽位
    fn slot_of(&self, disk: char, mft: u32) -> Option<u32> {
        let table = self.tables.get(&disk.to_ascii_uppercase())?;
        let slot = *table.slots.get(mft as usize)?;
        (slot != NO_SLOT).then_some(slot)
    }

    /// 按 FRN 取槽位。
    ///
    /// 会核对条目里存的完整 FRN：MFT 记录被回收给新文件后，序列号会变，
    /// 只比记录号会把新文件当成老文件。
    pub fn lookup(&self, disk: char, frn: u64) -> Option<u32> {
        let disk = disk.to_ascii_uppercase();
        let slot = if is_root(frn) {
            self.root_slot(disk)?
        } else {
            self.slot_of(disk, mft_index(frn) as u32)?
        };
        let entry = self.entries.get(slot as usize)?;
        if !entry.is_live() || entry.disk() != disk {
            return None;
        }
        // 盘根条目记的是第一次见到它时的 FRN，序列号可能与当下不同，所以不比
        if is_root(frn) || entry.frn == frn {
            Some(slot)
        } else {
            None
        }
    }

    /// 盘根槽位
    pub fn root_slot(&self, disk: char) -> Option<u32> {
        let table = self.tables.get(&disk.to_ascii_uppercase())?;
        (table.root != NO_SLOT).then_some(table.root)
    }

    /// 取某个条目的父槽位。
    ///
    /// ⚠️ 必须查**条目自己的盘**：每个盘的盘根记录号都是 5，跨盘查会命中另一个盘的根。
    pub fn parent_of(&self, entry: &Entry) -> Option<u32> {
        let table = self.tables.get(&entry.disk())?;
        let slot = *table.slots.get(entry.parent as usize)?;
        if slot == NO_SLOT {
            return None;
        }
        match self.entries.get(slot as usize) {
            Some(parent) if parent.is_live() => Some(slot),
            _ => None,
        }
    }

    /// 建出（或取出）盘根条目。根的名字就是 `C:` 这种形式，所以拼路径不需要特殊分支。
    pub fn ensure_root(&mut self, disk: char) -> u32 {
        let disk = disk.to_ascii_uppercase();
        if let Some(slot) = self.root_slot(disk) {
            return slot;
        }
        let (name_off, name_len) = self.push_name(&format!("{disk}:"));
        let slot = self.alloc_slot(Entry {
            name_off,
            name_len,
            flags: disk_code(disk) << DISK_SHIFT,
            parent: NO_PARENT,
            priority: DIR_PRIORITY,
            frn: ROOT_MFT_INDEX,
        });
        let table = self.table_mut(disk);
        table.root = slot;
        // 盘根也要登记进查找表：别人的父是它
        self.set_slot(disk, ROOT_MFT_INDEX, slot);
        slot
    }

    // ── 增删 ───────────────────────────────────────────────────────────

    /// 插入一条记录，返回槽位。
    ///
    /// - `parent_frn` 是父目录的 FRN；
    /// - `is_dir` 只用来定优先级（目录恒为 [`DIR_PRIORITY`]）；
    /// - 同名同 FRN 重复插入是幂等的（就地更新）；
    /// - 名字为空返回 `None`。
    ///
    /// 父目录不在索引里**不影响插入** —— 是否可用于拼路径由 `path_into` 决定。
    pub fn insert(
        &mut self,
        disk: char,
        frn: u64,
        parent_frn: u64,
        name: &str,
        is_dir: bool,
        priority: i32,
    ) -> Option<u32> {
        if name.is_empty() {
            return None;
        }
        let disk = disk.to_ascii_uppercase();
        let record = mft_index(frn) as u32;

        // 同一个 FRN 再插一次要就地更新，否则一次重命名会留下新旧两份记录
        let existing = self.slot_of(disk, record).filter(|slot| {
            self.entries
                .get(*slot as usize)
                .map(|entry| entry.frn == frn)
                .unwrap_or(false)
        });

        let (name_off, name_len) = self.push_name(name);
        let flags = disk_code(disk) << DISK_SHIFT;
        let entry = Entry {
            name_off,
            name_len,
            flags,
            // 父目录的 MFT 记录号。⚠️ 盘根的子项存的也是 5，
            // `NO_PARENT` 只留给盘根条目本身
            parent: mft_index(parent_frn) as u32,
            priority: if is_dir { DIR_PRIORITY } else { priority },
            frn,
        };

        match existing {
            Some(slot) => {
                // 记录已经在（live 数不变），只换内容
                self.entries[slot as usize] = entry;
                Some(slot)
            }
            None => {
                // MFT 记录被回收：同一个记录号上已经是另一个文件的 FRN，先清掉旧的
                if let Some(old) = self.slot_of(disk, record) {
                    self.free_slot(old);
                }
                let slot = self.alloc_slot(entry);
                self.set_slot(disk, frn, slot);
                Some(slot)
            }
        }
    }

    /// 删除一条记录（按 FRN 定位）
    pub fn remove(&mut self, disk: char, frn: u64) -> bool {
        let Some(slot) = self.lookup(disk, frn) else {
            return false;
        };
        self.discard(slot)
    }

    /// 删除一条记录（按槽位）。
    ///
    /// 只做 O(1) 标记与回收，**不清查找表里的引用** —— 那会让批量删除退化成平方
    /// 复杂度。陈旧引用是安全的：`lookup` 会核对条目存活状态与完整 FRN。
    pub fn discard(&mut self, slot: u32) -> bool {
        self.free_slot(slot)
    }

    /// 清掉某个盘的全部条目（重建这个盘的索引前调用）
    pub fn clear_disk(&mut self, disk: char) {
        let disk = disk.to_ascii_uppercase();
        let Some(table) = self.tables.remove(&disk) else {
            return;
        };
        for slot in &table.slots {
            if *slot != NO_SLOT {
                self.free_slot(*slot);
            }
        }
        if table.root != NO_SLOT {
            self.free_slot(table.root);
        }
    }

    // ── 路径 ───────────────────────────────────────────────────────────

    /// 把某个槽位的完整路径写进 `buf`（复用缓冲，不分配）。
    ///
    /// 返回 `false` 表示链条断了（祖先被删或其 MFT 记录被回收）—— 宁可放弃也不要
    /// 拼出错路径。层数超过 [`MAX_PATH_DEPTH`] 也返回 `false`。
    pub fn path_into(&self, slot: u32, scratch: &mut PathScratch, buf: &mut String) -> bool {
        scratch.segments.clear();
        let mut current = slot;

        // 先向上走到盘根，沿途把名字记下来
        let mut reached_root = false;
        for _ in 0..MAX_PATH_DEPTH {
            let Some(entry) = self.entries.get(current as usize) else {
                return false;
            };
            if !entry.is_live() {
                return false; // 撞到已删除的祖先
            }
            if entry.parent == NO_PARENT {
                let Some((start, end)) = self.name_range(entry) else {
                    return false;
                };
                buf.clear();
                buf.push_str(&String::from_utf8_lossy(&self.names[start..end]));
                reached_root = true;
                break;
            }
            scratch.segments.push(current);
            match self.parent_of(entry) {
                Some(parent) => current = parent,
                None => return false,
            }
        }
        if !reached_root {
            return false; // 层数超限
        }

        for slot in scratch.segments.iter().rev() {
            let Some(entry) = self.entries.get(*slot as usize) else {
                return false;
            };
            let Some((start, end)) = self.name_range(entry) else {
                return false;
            };
            buf.push('\\');
            buf.push_str(&String::from_utf8_lossy(&self.names[start..end]));
        }
        true
    }

    /// 某个槽位的完整路径（便捷版，会分配）
    pub fn path_of(&self, slot: u32) -> Option<String> {
        let mut scratch = PathScratch::new();
        let mut buf = String::new();
        self.path_into(slot, &mut scratch, &mut buf).then_some(buf)
    }

    // ── 批量维护 ───────────────────────────────────────────────────────

    /// 遍历所有存活条目：`(槽位, 条目)`
    pub fn iter_live(&self) -> impl Iterator<Item = (u32, &Entry)> {
        self.entries
            .iter()
            .enumerate()
            .filter(|(_, entry)| entry.is_live())
            .map(|(slot, entry)| (slot as u32, entry))
    }

    /// 把路径命中忽略规则的条目标记为已删除，返回清掉几条。
    ///
    /// 先整体写入再按路径过滤：路径解析要求祖先已经在索引里，而 MFT 的记录顺序
    /// 并不保证「父先于子」。
    pub fn drop_ignored(&mut self, disk: char, rules: &IgnoreRules) -> usize {
        if rules.is_empty() {
            return 0;
        }
        let disk = disk.to_ascii_uppercase();
        let slots: Vec<u32> = {
            let mut scratch = PathScratch::new();
            let mut buf = String::new();
            let mut dead = Vec::new();
            for slot in self.slots_of_disk(disk) {
                buf.clear();
                if !self.path_into(slot, &mut scratch, &mut buf) {
                    continue;
                }
                if rules.matches(&buf) {
                    dead.push(slot);
                }
            }
            dead
        };
        for slot in &slots {
            self.discard(*slot);
        }
        slots.len()
    }

    /// 某个盘的全部存活槽位（已排序、去重）
    fn slots_of_disk(&self, disk: char) -> Vec<u32> {
        let disk = disk.to_ascii_uppercase();
        let Some(table) = self.tables.get(&disk) else {
            return Vec::new();
        };
        let mut out: Vec<u32> = table
            .slots
            .iter()
            .copied()
            .filter(|slot| *slot != NO_SLOT)
            .filter(|slot| self.is_live_of(*slot, disk))
            .collect();
        if table.root != NO_SLOT {
            out.push(table.root);
        }
        out.sort_unstable();
        out.dedup();
        out
    }

    /// 槽位是否存活、且确实属于指定盘
    fn is_live_of(&self, slot: u32, disk: char) -> bool {
        self.entries
            .get(slot as usize)
            .map(|entry| entry.is_live() && entry.disk() == disk)
            .unwrap_or(false)
    }

    /// 某个盘当前的存活条目数（含盘根）
    pub fn count_disk(&self, disk: char) -> usize {
        let disk = disk.to_ascii_uppercase();
        let Some(table) = self.tables.get(&disk) else {
            return 0;
        };
        // 盘根也在 `slots` 里，所以不用另外加一
        table
            .slots
            .iter()
            .copied()
            .filter(|slot| *slot != NO_SLOT && self.is_live_of(*slot, disk))
            .count()
    }

    /// 清理某个盘里「链条断了」的孤儿条目，返回清掉几条。
    ///
    /// 正常不会产生孤儿（删目录时 Windows 会为每个后代各发一条 USN 记录），
    /// 但「建索引时某个祖先没枚举到」或「监控漏了事件」会留下。
    pub fn prune_orphans_of(&mut self, disk: char) -> usize {
        let mut scratch = PathScratch::new();
        let mut buf = String::new();
        let mut dead: Vec<u32> = Vec::new();
        for slot in self.slots_of_disk(disk.to_ascii_uppercase()) {
            let Some(entry) = self.entries.get(slot as usize) else {
                continue;
            };
            if entry.parent == NO_PARENT {
                continue; // 盘根永远有效
            }
            buf.clear();
            if !self.path_into(slot, &mut scratch, &mut buf) {
                dead.push(slot);
            }
        }
        for slot in &dead {
            self.discard(*slot);
        }
        dead.len()
    }

    /// 清理所有盘的孤儿条目（保存快照前顺手做一次）
    pub fn prune_orphans(&mut self) -> usize {
        let disks: Vec<char> = self.tables.keys().copied().collect();
        disks.iter().map(|disk| self.prune_orphans_of(*disk)).sum()
    }

    /// 名称池里有多少字节是「已删除条目的残留」
    fn wasted_name_bytes(&self) -> usize {
        let live: usize = self
            .entries
            .iter()
            .filter(|entry| entry.is_live())
            .map(|entry| entry.name_len as usize)
            .sum();
        self.names.len().saturating_sub(live)
    }

    /// 重排名称池，把已删除名字占的字节收回来（单条名字无法单独回收）。
    ///
    /// 返回是否真的重排过。垃圾小于四分之一时不动。
    pub fn compact_names(&mut self) -> bool {
        let waste = self.wasted_name_bytes();
        if self.names.is_empty() || waste * 4 < self.names.len() {
            return false;
        }

        let mut names = Vec::with_capacity(self.names.len() - waste);
        for entry in self.entries.iter_mut() {
            if entry.is_dead() {
                continue;
            }
            let start = entry.name_off as usize;
            let end = start + entry.name_len as usize;
            let Some(bytes) = self.names.get(start..end) else {
                continue;
            };
            entry.name_off = names.len() as u32;
            names.extend_from_slice(bytes);
        }
        self.names = names;
        true
    }

    // ── 快照 ───────────────────────────────────────────────────────────

    /// 写快照（单文件）。先压实（丢掉已删槽位、重排名称池），再写临时文件后改名。
    pub fn save(
        &mut self,
        path: &Path,
        priorities: &PriorityTable,
        marks: &HashMap<char, JournalMark>,
    ) -> Result<SaveStats> {
        self.compact_names();

        // 槽位重映射：只保留存活条目，顺带把名称池压紧写出去
        let mut remap = vec![NO_SLOT; self.entries.len()];
        let mut out_entries: Vec<Entry> = Vec::with_capacity(self.live as usize);
        let mut names: Vec<u8> = Vec::with_capacity(self.names.len());
        for (slot, entry) in self.entries.iter().enumerate() {
            if entry.is_dead() {
                continue;
            }
            let Some((start, end)) = self.name_range(entry) else {
                continue;
            };
            remap[slot] = out_entries.len() as u32;
            names.extend_from_slice(&self.names[start..end]);
            out_entries.push(Entry {
                name_off: (names.len() - entry.name_len as usize) as u32,
                ..*entry
            });
        }

        let mut buf: Vec<u8> = Vec::with_capacity(
            64 + names.len() + out_entries.len() * std::mem::size_of::<Entry>(),
        );
        buf.extend_from_slice(MAGIC);
        put_u32(&mut buf, SNAPSHOT_VERSION);
        put_u32(&mut buf, out_entries.len() as u32);
        put_u32(&mut buf, names.len() as u32);

        // ── 每个盘的查找表 ──
        let mut disks: Vec<char> = self.tables.keys().copied().collect();
        disks.sort_unstable();
        put_u32(&mut buf, disks.len() as u32);
        for disk in &disks {
            let table = &self.tables[disk];
            let root = if table.root == NO_SLOT {
                NO_SLOT
            } else {
                remap[table.root as usize]
            };
            put_u8(&mut buf, *disk as u8);
            put_u32(&mut buf, root);

            // 「可用」= 指向的条目还活着、且确实属于这个盘。
            // 槽位是全局复用的，而 `discard` 不清查找表里的引用。
            let usable = |slot: u32| -> bool {
                if slot == NO_SLOT {
                    return false;
                }
                match self.entries.get(slot as usize) {
                    Some(entry) => entry.is_live() && entry.disk() == *disk,
                    None => false,
                }
            };
            let mut len = table.slots.len();
            while len > 0 && !usable(table.slots[len - 1]) {
                len -= 1; // 尾巴上的空洞（含陈旧引用）不用写出去
            }
            put_u32(&mut buf, len as u32);
            for index in 0..len {
                let slot = table.slots[index];
                let value = if usable(slot) {
                    remap[slot as usize]
                } else {
                    NO_SLOT
                };
                put_u32(&mut buf, value);
            }
        }

        // ── 名称池 + 记录数组（两块大块数据，顺序写）──
        buf.extend_from_slice(&names);
        for entry in &out_entries {
            put_u32(&mut buf, entry.name_off);
            put_u16(&mut buf, entry.name_len);
            put_u16(&mut buf, entry.flags);
            put_u32(&mut buf, entry.parent);
            put_i32(&mut buf, entry.priority);
            put_u64(&mut buf, entry.frn);
        }

        // ── 后缀优先级 ──
        let priority_pairs: Vec<(&str, i32)> = priorities.iter().collect();
        put_u32(&mut buf, priority_pairs.len() as u32);
        for (suffix, priority) in priority_pairs {
            put_u32(&mut buf, suffix.len() as u32);
            buf.extend_from_slice(suffix.as_bytes());
            put_i32(&mut buf, priority);
        }

        // ── 每个盘的 USN 位点 ──
        let mut mark_list: Vec<(char, JournalMark)> =
            marks.iter().map(|(disk, mark)| (*disk, *mark)).collect();
        mark_list.sort_by_key(|(disk, _)| *disk);
        put_u32(&mut buf, mark_list.len() as u32);
        for (disk, mark) in mark_list {
            put_u8(&mut buf, disk as u8);
            put_u64(&mut buf, mark.journal_id);
            put_i64(&mut buf, mark.last_usn);
        }

        // 校验和放在最后，覆盖前面全部字节
        let checksum = fnv1a(&buf);
        put_u64(&mut buf, checksum);

        let stats = SaveStats {
            entries: out_entries.len(),
            names_bytes: names.len(),
            file_bytes: buf.len() as u64,
        };

        let temp = temp_path(path);
        std::fs::write(&temp, &buf)?;
        // 改名是原子的：要么还是旧快照，要么就是完整的这一份
        std::fs::rename(&temp, path)?;
        Ok(stats)
    }

    /// 读快照。文件不存在、版本不对、校验和不符都返回 `Ok(None)` ——
    /// 调用方按「没有快照」处理，重扫 MFT 即可，快照本来就只是加速手段。
    pub fn load(path: &Path) -> Result<Option<LoadedIndex>> {
        let Ok(buf) = std::fs::read(path) else {
            return Ok(None);
        };
        let Ok(loaded) = Self::decode(&buf) else {
            return Ok(None);
        };
        Ok(Some(loaded))
    }

    fn decode(buf: &[u8]) -> Result<LoadedIndex> {
        let corrupt = || CoreError::InvalidArgument("索引快照已损坏".to_string());

        let mut cursor = Cursor::new(buf);
        if cursor.take(8)? != MAGIC {
            return Err(corrupt());
        }
        if cursor.u32()? != SNAPSHOT_VERSION {
            return Err(corrupt());
        }
        let entry_count = cursor.u32()? as usize;
        let names_len = cursor.u32()? as usize;

        let disk_count = cursor.u32()? as usize;
        let mut raw_tables: Vec<(char, u32, Vec<u32>)> = Vec::with_capacity(disk_count);
        for _ in 0..disk_count {
            let letter = cursor.u8()? as char;
            let root = cursor.u32()?;
            let len = cursor.u32()? as usize;
            let mut slots = Vec::with_capacity(len);
            for _ in 0..len {
                slots.push(cursor.u32()?);
            }
            raw_tables.push((letter, root, slots));
        }

        let names = cursor.take(names_len)?.to_vec();
        let mut entries = Vec::with_capacity(entry_count);
        for _ in 0..entry_count {
            entries.push(Entry {
                name_off: cursor.u32()?,
                name_len: cursor.u16()?,
                flags: cursor.u16()?,
                parent: cursor.u32()?,
                priority: cursor.i32()?,
                frn: cursor.u64()?,
            });
        }

        let priority_count = cursor.u32()? as usize;
        let mut priorities = Vec::with_capacity(priority_count);
        for _ in 0..priority_count {
            let len = cursor.u32()? as usize;
            let suffix = std::str::from_utf8(cursor.take(len)?)
                .map_err(|_| corrupt())?
                .to_string();
            priorities.push((suffix, cursor.i32()?));
        }

        let mark_count = cursor.u32()? as usize;
        let mut marks = HashMap::with_capacity(mark_count);
        for _ in 0..mark_count {
            let letter = cursor.u8()? as char;
            let journal_id = cursor.u64()?;
            let last_usn = cursor.i64()?;
            marks.insert(
                letter,
                JournalMark {
                    last_usn,
                    journal_id,
                },
            );
        }

        let checksum = cursor.u64()?;
        if cursor.pos != buf.len() || fnv1a(&buf[..buf.len() - 8]) != checksum {
            return Err(corrupt());
        }

        let mut tables = HashMap::with_capacity(raw_tables.len());
        for (disk, root, slots) in raw_tables {
            tables.insert(disk, DiskTable { slots, root });
        }
        let live = entries.iter().filter(|entry| entry.is_live()).count() as u32;

        Ok(LoadedIndex {
            index: FileIndex {
                names,
                entries,
                free: Vec::new(),
                live,
                tables,
            },
            priorities: PriorityTable::from_pairs(priorities),
            marks,
        })
    }
}

/// [`FileIndex::load`] 的返回值
pub struct LoadedIndex {
    pub index: FileIndex,
    pub priorities: PriorityTable,
    pub marks: HashMap<char, JournalMark>,
}

/// [`FileIndex::save`] 的统计
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct SaveStats {
    pub entries: usize,
    pub names_bytes: usize,
    pub file_bytes: u64,
}

/// 快照路径
pub fn snapshot_path(dir: &Path) -> PathBuf {
    dir.join(SNAPSHOT_FILE)
}

fn temp_path(path: &Path) -> PathBuf {
    let mut name = path.file_name().unwrap_or_default().to_os_string();
    name.push(".tmp");
    path.with_file_name(name)
}

// ── 小工具 ─────────────────────────────────────────────────────────────

fn put_u8(buf: &mut Vec<u8>, value: u8) {
    buf.push(value);
}

fn put_u16(buf: &mut Vec<u8>, value: u16) {
    buf.extend_from_slice(&value.to_le_bytes());
}

fn put_u32(buf: &mut Vec<u8>, value: u32) {
    buf.extend_from_slice(&value.to_le_bytes());
}

fn put_i32(buf: &mut Vec<u8>, value: i32) {
    buf.extend_from_slice(&value.to_le_bytes());
}

fn put_u64(buf: &mut Vec<u8>, value: u64) {
    buf.extend_from_slice(&value.to_le_bytes());
}

fn put_i64(buf: &mut Vec<u8>, value: i64) {
    buf.extend_from_slice(&value.to_le_bytes());
}

/// FNV-1a 64：不追求抗碰撞，只用来发现截断/位翻转这类损坏
fn fnv1a(bytes: &[u8]) -> u64 {
    let mut hash = 0xcbf2_9ce4_8422_2325u64;
    for byte in bytes {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    hash
}

/// 带边界检查的只读游标
struct Cursor<'a> {
    buf: &'a [u8],
    pos: usize,
}

impl<'a> Cursor<'a> {
    fn new(buf: &'a [u8]) -> Self {
        Self { buf, pos: 0 }
    }

    fn take(&mut self, len: usize) -> Result<&'a [u8]> {
        let end = self
            .pos
            .checked_add(len)
            .ok_or_else(|| CoreError::InvalidArgument("索引快照越界".to_string()))?;
        let slice = self
            .buf
            .get(self.pos..end)
            .ok_or_else(|| CoreError::InvalidArgument("索引快照被截断".to_string()))?;
        self.pos = end;
        Ok(slice)
    }

    fn u8(&mut self) -> Result<u8> {
        Ok(self.take(1)?[0])
    }

    fn u16(&mut self) -> Result<u16> {
        let bytes = self.take(2)?;
        Ok(u16::from_le_bytes([bytes[0], bytes[1]]))
    }

    fn u32(&mut self) -> Result<u32> {
        let bytes = self.take(4)?;
        Ok(u32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]))
    }

    fn i32(&mut self) -> Result<i32> {
        Ok(self.u32()? as i32)
    }

    fn u64(&mut self) -> Result<u64> {
        let bytes = self.take(8)?;
        Ok(u64::from_le_bytes([
            bytes[0], bytes[1], bytes[2], bytes[3], bytes[4], bytes[5], bytes[6], bytes[7],
        ]))
    }

    fn i64(&mut self) -> Result<i64> {
        Ok(self.u64()? as i64)
    }
}

// ══════════════════════════════════════════════════════════════════════
//  从 MFT 建立索引（需要管理员权限）
// ══════════════════════════════════════════════════════════════════════

/// 每处理这么多条就回报一次进度
const PROGRESS_INTERVAL: u64 = 100_000;

/// 建索引的参数
#[derive(Debug, Clone)]
pub struct IndexConfig {
    /// 要索引的盘符
    pub disks: Vec<char>,
    /// 忽略的目录（忽略整棵子树）
    pub ignore_paths: Vec<String>,
    /// 是否先清空原有记录
    pub drop_previous: bool,
}

impl IndexConfig {
    pub fn new(disks: Vec<char>) -> Self {
        Self {
            disks,
            ignore_paths: Vec::new(),
            drop_previous: true,
        }
    }

    pub fn with_ignore_paths(mut self, paths: impl IntoIterator<Item = String>) -> Self {
        self.ignore_paths = paths
            .into_iter()
            .filter(|path| !path.trim().is_empty())
            .collect();
        self
    }
}

/// 进度回报
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct IndexProgress {
    pub disk: char,
    /// 已从 MFT 读到的记录数
    pub scanned: u64,
    /// 已写进索引的条数
    pub written: u64,
}

/// 单个盘的结果
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct DiskReport {
    pub disk: char,
    /// MFT 里读到的记录数
    pub records: u64,
    /// 最终留在索引里的条数
    pub written: u64,
    /// 因命中忽略目录而丢掉的条数
    pub ignored: u64,
    /// 因父目录不在索引里（链条断裂）而收掉的条数
    pub broken: u64,
    pub elapsed_ms: u128,
}

/// 整体结果
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct IndexReport {
    pub disks: Vec<DiskReport>,
    pub elapsed_ms: u128,
}

impl IndexReport {
    pub fn total_written(&self) -> u64 {
        self.disks.iter().map(|disk| disk.written).sum()
    }
}

/// 把一条 USN 记录翻译成索引条目，返回是否真的插进去了。
///
/// 独立成函数以便脱开卷句柄单测。
fn insert_mft_record(
    index: &mut FileIndex,
    disk: char,
    priorities: &PriorityTable,
    record: &UsnRecord<'_>,
) -> bool {
    // 盘根那条记录没有名字，必须跳过
    if record.name_bytes.is_empty() {
        return false;
    }
    let name = record.name_lossy();
    let is_dir = record.is_directory();
    let priority = if is_dir {
        DIR_PRIORITY
    } else {
        priorities.of_suffix(pathutil::suffix_str(&name))
    };
    index
        .insert(disk, record.frn, record.parent_frn, &name, is_dir, priority)
        .is_some()
}

/// 重建一个盘的索引。
///
/// 1. 打开卷（`\\.\X:`）并校验是 NTFS；
/// 2. 取/建 USN 日志，拿到 `NextUsn`；
/// 3. 一趟 `FSCTL_ENUM_USN_DATA` 把整个 MFT 读出来灌进内存索引；
/// 4. 再走一趟，把命中忽略规则的条目标掉，最后收一次孤儿。
///
/// 第 4 步不能并到第 3 步里：忽略规则是路径级的，而路径解析要求祖先已经在索引里。
pub fn rebuild_disk(
    disk: char,
    index: &mut FileIndex,
    priorities: &PriorityTable,
    config: &IndexConfig,
    progress: &mut dyn FnMut(IndexProgress),
) -> Result<DiskReport> {
    disk::ensure_supported(disk)?;
    let started = Instant::now();

    let volume = Volume::open(disk)?;
    // 必须确认是 NTFS，否则下面这些 IOCTL 都没有意义
    volume.verify_ntfs()?;
    let journal = volume.ensure_journal()?;

    if config.drop_previous {
        index.clear_disk(disk);
    }
    // 盘根要先建出来，路径拼接靠它收尾
    index.ensure_root(disk);

    let mut report = DiskReport {
        disk,
        ..Default::default()
    };
    let mut inserted = 0u64;

    // ── 一趟：MFT 全量灌进内存索引 ────────────────────
    let records = volume.enum_usn_records(journal.NextUsn, |record| {
        if insert_mft_record(index, disk, priorities, &record) {
            inserted += 1;
            if inserted.is_multiple_of(PROGRESS_INTERVAL) {
                progress(IndexProgress {
                    disk,
                    scanned: inserted,
                    written: inserted,
                });
            }
        }
    })?;

    report.records = records;

    // ── 第二趟：按路径应用忽略规则，再收掉孤儿 ──────────────
    let rules = IgnoreRules::new(&config.ignore_paths);
    report.ignored = index.drop_ignored(disk, &rules) as u64;
    report.broken = index.prune_orphans_of(disk) as u64;
    report.written = index.count_disk(disk) as u64;
    report.elapsed_ms = started.elapsed().as_millis();

    progress(IndexProgress {
        disk,
        scanned: records,
        written: report.written,
    });
    Ok(report)
}

/// 按 [`IndexConfig::disks`] 依次重建索引（串行，不同时持有多个卷句柄）
pub fn rebuild_all(
    index: &mut FileIndex,
    priorities: &PriorityTable,
    config: &IndexConfig,
    progress: &mut dyn FnMut(IndexProgress),
) -> Result<IndexReport> {
    let started = Instant::now();
    let mut report = IndexReport::default();

    for disk in &config.disks {
        let disk = disk.to_ascii_uppercase();
        report
            .disks
            .push(rebuild_disk(disk, index, priorities, config, progress)?);
    }

    report.elapsed_ms = started.elapsed().as_millis();
    Ok(report)
}

/// 仅测试可见：benchmark 与 e2e 要报告内存占用。
#[cfg(test)]
impl FileIndex {
    /// 独占的近似内存占用
    pub fn used_bytes(&self) -> usize {
        self.names.capacity()
            + self.entries.capacity() * std::mem::size_of::<Entry>()
            + self.free.capacity() * std::mem::size_of::<u32>()
            + self
                .tables
                .values()
                .map(|table| table.slots.capacity() * std::mem::size_of::<u32>())
                .sum::<usize>()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::search::win::usn::FILE_ATTRIBUTE_DIRECTORY;

    /// `C:\` 的根 FRN（真实形态：序列号在高端）
    const ROOT: u64 = 0x0005_0000_0000_0005;

    fn frn(index: u64) -> u64 {
        (1u64 << 48) | index
    }

    /// 取槽位的名字字符串（生产代码用 `name_bytes`，测试才需要 `String`）
    fn name_of(index: &FileIndex, slot: u32) -> String {
        let entry = &index.entries[slot as usize];
        let (start, end) = index.name_range(entry).expect("名字范围应当有效");
        std::str::from_utf8(&index.names[start..end])
            .expect("名字应当是合法 UTF-8")
            .to_string()
    }

    fn temp_dir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "rastflow-index-{tag}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// 建一棵小树：C:\Windows\System32\ntfs.sys + C:\Windows\notepad.exe
    fn sample_tree() -> FileIndex {
        let mut index = FileIndex::new();
        index.ensure_root('C');
        index.insert('C', frn(10), ROOT, "Windows", true, DIR_PRIORITY);
        index.insert('C', frn(11), ROOT, "Users", true, DIR_PRIORITY);
        index.insert('C', frn(20), frn(10), "System32", true, DIR_PRIORITY);
        index.insert('C', frn(30), frn(10), "notepad.exe", false, 30);
        index.insert('C', frn(40), frn(20), "ntfs.sys", false, 0);
        index
    }

    #[test]
    fn path_is_assembled_from_ancestors() {
        let index = sample_tree();
        let slot = index.lookup('C', frn(40)).unwrap();
        assert_eq!(index.path_of(slot).as_deref(), Some(r"C:\Windows\System32\ntfs.sys"));

        let slot = index.lookup('C', frn(30)).unwrap();
        assert_eq!(index.path_of(slot).as_deref(), Some(r"C:\Windows\notepad.exe"));

        // 盘根本身的名字就是盘符
        let root = index.root_slot('C').unwrap();
        assert_eq!(index.path_of(root).as_deref(), Some("C:"));
    }

    #[test]
    fn name_pool_stores_each_name_once() {
        let index = sample_tree();
        // 5 个条目 + 盘根 = 6 个名字，池里字节数就是它们长度之和
        let expected: usize = ["C:", "Windows", "Users", "System32", "notepad.exe", "ntfs.sys"]
            .iter()
            .map(|name| name.len())
            .sum();
        assert_eq!(index.names.len(), expected);
        assert_eq!(index.iter_live().count(), 6);
    }

    #[test]
    fn renaming_parent_updates_children_immediately() {
        // 这是层次结构相对「存完整路径」最大的好处：改一个条目，
        // 它下面成千上万个孩子的路径自动全对
        let mut index = sample_tree();
        let windows = index.lookup('C', frn(10)).unwrap();
        index.discard(windows);
        index.insert('C', frn(10), ROOT, "WinNT", true, DIR_PRIORITY);

        let slot = index.lookup('C', frn(40)).unwrap();
        assert_eq!(
            index.path_of(slot).as_deref(),
            Some(r"C:\WinNT\System32\ntfs.sys")
        );
    }

    #[test]
    fn deleted_directory_yields_no_path_not_a_wrong_path() {
        let mut index = sample_tree();
        index.remove('C', frn(20)); // 删掉 System32
        let slot = index.lookup('C', frn(40)).unwrap();
        assert_eq!(index.path_of(slot), None, "宁可放弃也不能拼出错路径");
    }

    #[test]
    fn insert_does_not_require_parent_to_come_first() {
        // MFT 的记录顺序并不保证父在子之前，所以这一点是建索引的前提
        let mut index = FileIndex::new();
        index.ensure_root('C');
        index.insert('C', frn(40), frn(20), "ntfs.sys", false, 0);
        index.insert('C', frn(20), frn(10), "System32", true, DIR_PRIORITY);
        index.insert('C', frn(10), ROOT, "Windows", true, DIR_PRIORITY);

        let slot = index.lookup('C', frn(40)).unwrap();
        assert_eq!(index.path_of(slot).as_deref(), Some(r"C:\Windows\System32\ntfs.sys"));
    }

    #[test]
    fn reused_mft_record_is_distinguished_by_sequence() {
        let mut index = FileIndex::new();
        index.ensure_root('C');
        index.insert('C', frn(50), ROOT, "old.txt", false, 0);
        // 同一个 MFT 记录号，但序列号不同 = 新文件占了旧记录
        let new_frn = (2u64 << 48) | 50;
        index.insert('C', new_frn, ROOT, "new.txt", false, 0);

        assert_eq!(index.iter_live().count(), 2, "旧条目被顶掉，新条目进来");
        assert_eq!(index.lookup('C', frn(50)), None, "旧 FRN 查不到了");
        let slot = index.lookup('C', new_frn).unwrap();
        assert_eq!(name_of(&index, slot), "new.txt");
    }

    #[test]
    fn remove_locates_by_frn_and_is_idempotent() {
        let mut index = sample_tree();
        assert!(index.remove('C', frn(30)));
        assert!(!index.remove('C', frn(30)), "删两次第二次没改动");
        assert_eq!(index.iter_live().count(), 5);
        // 删掉的槽位能被复用，复用后旧 FRN 不能再查到
        assert_eq!(index.lookup('C', frn(30)), None);
        index.insert('C', frn(99), ROOT, "reused.txt", false, 0);
        assert_eq!(index.lookup('C', frn(30)), None);
    }

    #[test]
    fn empty_name_and_duplicate_insert() {
        let mut index = FileIndex::new();
        index.ensure_root('C');
        assert_eq!(index.insert('C', frn(7), ROOT, "", false, 0), None);

        index.insert('C', frn(7), ROOT, "a.txt", false, 0);
        index.insert('C', frn(7), ROOT, "a.txt", false, 0);
        assert_eq!(index.iter_live().count(), 2, "盘根 + a.txt");
    }

    #[test]
    fn ignore_rules_filter_by_path() {
        let mut index = sample_tree();
        let dropped = index.drop_ignored(
            'C',
            &IgnoreRules::new(&[r"C:\Windows\System32".to_string()]),
        );
        // 忽略一个目录会连**它自己**和它下面的一切一起丢掉
        assert_eq!(dropped, 2, "System32 目录本身 + 它下面的 ntfs.sys");
        assert_eq!(index.iter_live().count(), 4);
        // 同名前缀但不同层级的不应被牵连
        let mut index2 = FileIndex::new();
        index2.ensure_root('C');
        index2.insert('C', frn(10), ROOT, "Windows", true, DIR_PRIORITY);
        index2.insert('C', frn(11), ROOT, "WindowsApps", true, DIR_PRIORITY);
        index2.insert('C', frn(12), frn(11), "app.exe", false, 30);
        assert_eq!(
            index2.drop_ignored('C', &IgnoreRules::new(&[r"C:\Windows".to_string()])),
            1,
            "只该丢掉 C:\\Windows 它自己"
        );
        // 前缀相同但不是目录边界的都不该被误伤
        assert!(index2.lookup('C', frn(11)).is_some(), "WindowsApps 还在");
        assert!(index2.lookup('C', frn(12)).is_some(), "它下面的文件也还在");
    }

    #[test]
    fn orphans_are_pruned() {
        let mut index = sample_tree();
        // 直接抹掉 System32 的查找表映射，模拟「监控漏了删除事件」
        index.tables.get_mut(&'C').unwrap().slots[20] = NO_SLOT;
        assert_eq!(index.prune_orphans(), 1, "ntfs.sys 成了孤儿");
        assert_eq!(index.iter_live().count(), 5);
    }

    #[test]
    fn compact_names_reclaims_garbage() {
        let mut index = FileIndex::new();
        index.ensure_root('C');
        // 造出大量垃圾名字
        for n in 0..200 {
            index.insert('C', frn(100 + n), ROOT, &format!("junk-name-{n}.tmp"), false, 0);
        }
        for n in 0..200 {
            index.remove('C', frn(100 + n));
        }
        let before = index.names.len();
        assert!(before > 1000);
        assert!(index.compact_names(), "垃圾超过四分之一就该压实");
        assert_eq!(index.names.len(), 2, "只剩盘根的名字 `C:`");
        // 压实后仍能正常取名字
        index.insert('C', frn(500), ROOT, "keep.txt", false, 0);
        let slot = index.lookup('C', frn(500)).unwrap();
        assert_eq!(name_of(&index, slot), "keep.txt");
    }

    #[test]
    fn huge_mft_record_number_does_not_blow_up_memory() {
        // 查找表拿 MFT 记录号当下标，所以一个脏 FRN 就能让进程去要 TB 级内存。
        // 这里验证它被上限挡住了（而且不影响其它条目正常工作）。
        let mut index = FileIndex::new();
        index.ensure_root('C');
        assert!(index
            .insert('C', 0x0000_FFFF_FFFF_FFFF, ROOT, "huge.txt", false, 0)
            .is_some());
        // 条目还在（能被扫描到），只是按 FRN 查不到
        assert_eq!(index.lookup('C', 0x0000_FFFF_FFFF_FFFF), None);
        assert_eq!(index.iter_live().count(), 2);

        // 正常范围内的 FRN 不受影响
        index.insert('C', frn(1234), ROOT, "normal.txt", false, 0);
        assert!(index.lookup('C', frn(1234)).is_some());
    }

    #[test]
    fn snapshot_roundtrip() {
        let dir = temp_dir("roundtrip");
        let path = snapshot_path(&dir);

        let mut index = sample_tree();
        let mut marks = HashMap::new();
        marks.insert(
            'C',
            JournalMark {
                last_usn: 12345,
                journal_id: 0xABCD,
            },
        );
        let priorities = PriorityTable::from_pairs([("exe", 30), ("sys", 7)]);

        let stats = index.save(&path, &priorities, &marks).unwrap();
        assert_eq!(stats.entries, 6);
        assert!(path.exists());

        let loaded = FileIndex::load(&path).unwrap().expect("应当能读回来");
        assert_eq!(loaded.index.iter_live().count(), 6);
        assert_eq!(loaded.marks[&'C'].last_usn, 12345);
        assert_eq!(loaded.marks[&'C'].journal_id, 0xABCD);
        assert_eq!(loaded.priorities.of_suffix("sys"), 7);

        let slot = loaded.index.lookup('C', frn(40)).unwrap();
        assert_eq!(
            loaded.index.path_of(slot).as_deref(),
            Some(r"C:\Windows\System32\ntfs.sys")
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn snapshot_drops_deleted_entries_and_compacts_names() {
        let dir = temp_dir("compact");
        let path = snapshot_path(&dir);

        let mut index = FileIndex::new();
        index.ensure_root('C');
        for n in 0..100 {
            index.insert('C', frn(200 + n), ROOT, &format!("gone-{n}.txt"), false, 0);
        }
        for n in 0..100 {
            index.remove('C', frn(200 + n));
        }
        index.insert('C', frn(400), ROOT, "kept.txt", false, 0);

        let stats = index
            .save(&path, &PriorityTable::default(), &HashMap::new())
            .unwrap();
        assert_eq!(stats.entries, 2, "只有盘根和 kept.txt");
        assert_eq!(stats.names_bytes, "C:".len() + "kept.txt".len());

        let loaded = FileIndex::load(&path).unwrap().unwrap();
        assert_eq!(loaded.index.iter_live().count(), 2);
        let slot = loaded.index.lookup('C', frn(400)).unwrap();
        assert_eq!(name_of(&loaded.index, slot), "kept.txt");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn corrupt_snapshot_is_treated_as_absent() {
        let dir = temp_dir("corrupt");
        let path = snapshot_path(&dir);

        // 根本没有文件
        assert!(FileIndex::load(&path).unwrap().is_none());

        let mut index = sample_tree();
        index
            .save(&path, &PriorityTable::default(), &HashMap::new())
            .unwrap();

        // 魔数不对
        let good = std::fs::read(&path).unwrap();
        let mut bad = good.clone();
        bad[0] = b'X';
        std::fs::write(&path, &bad).unwrap();
        assert!(FileIndex::load(&path).unwrap().is_none());

        // 位翻转 → 校验和兜住
        let mut flipped = good.clone();
        let last = flipped.len() - 12;
        flipped[last] ^= 0xFF;
        std::fs::write(&path, &flipped).unwrap();
        assert!(FileIndex::load(&path).unwrap().is_none());

        // 截断
        std::fs::write(&path, &good[..good.len() / 2]).unwrap();
        assert!(FileIndex::load(&path).unwrap().is_none());

        // 版本号不认识
        let mut older = good.clone();
        older[8..12].copy_from_slice(&(SNAPSHOT_VERSION + 1).to_le_bytes());
        std::fs::write(&path, &older).unwrap();
        assert!(FileIndex::load(&path).unwrap().is_none());

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn multiple_disks_are_independent() {
        let mut index = FileIndex::new();
        index.ensure_root('C');
        index.ensure_root('D');
        // 两个盘的同一个 MFT 记录号
        index.insert('C', frn(10), ROOT, "a.txt", false, 0);
        index.insert('D', frn(10), ROOT, "b.txt", false, 0);

        let c = index.lookup('C', frn(10)).unwrap();
        let d = index.lookup('D', frn(10)).unwrap();
        assert_ne!(c, d);
        assert_eq!(index.path_of(c).as_deref(), Some(r"C:\a.txt"));
        assert_eq!(index.path_of(d).as_deref(), Some(r"D:\b.txt"));
        assert_eq!(index.disks().count(), 2);
    }

    #[test]
    fn self_reference_and_cycles_do_not_hang() {
        let mut index = FileIndex::new();
        index.ensure_root('C');
        // 让 a 的父指向自己
        index.insert('C', frn(60), frn(60), "loop", true, DIR_PRIORITY);
        let slot = index.lookup('C', frn(60)).unwrap();
        assert_eq!(
            index.path_of(slot),
            None,
            "自引用走不到盘根，应当放弃而不是转圈"
        );
    }

    #[test]
    fn depth_limit_gives_up_instead_of_looping() {
        let mut index = FileIndex::new();
        index.ensure_root('C');
        // 造一条比 MAX_PATH_DEPTH 更长的链，且最上面的父不存在
        let mut parent = ROOT;
        for n in 0..(MAX_PATH_DEPTH as u64 + 10) {
            let current = frn(1000 + n);
            index.insert('C', current, parent, &format!("d{n}"), true, DIR_PRIORITY);
            parent = current;
        }
        let slot = index.lookup('C', frn(1000 + MAX_PATH_DEPTH as u64 + 9)).unwrap();
        assert_eq!(index.path_of(slot), None);
    }

    #[test]
    fn snapshot_trims_lookup_table_tail() {
        let dir = temp_dir("sparse");
        let path = snapshot_path(&dir);
        let mut index = FileIndex::new();
        index.ensure_root('C');
        // 把记录号推到 100 万，再把那条删掉 —— 尾部就留下一大片空洞
        index.insert('C', frn(1_000_000), ROOT, "temporary.txt", false, 0);
        index.remove('C', frn(1_000_000));

        let stats = index
            .save(&path, &PriorityTable::default(), &HashMap::new())
            .unwrap();
        // 100 万个 u32 是 4 MB；尾部空洞被裁掉后文件应该很小
        assert!(
            stats.file_bytes < 200_000,
            "尾部的空洞不该写出去，实际 {} 字节",
            stats.file_bytes
        );

        // 读回来仍然正确
        let loaded = FileIndex::load(&path).unwrap().unwrap();
        assert_eq!(loaded.index.iter_live().count(), 1, "只剩盘根");
        let root = loaded.index.root_slot('C').unwrap();
        assert_eq!(loaded.index.path_of(root).as_deref(), Some("C:"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn lookup_table_middle_holes_are_written_as_is() {
        // 只有**尾部**的连续空洞会被裁掉，中间的空洞会占满那一整段下标空间
        let dir = temp_dir("sparse-middle");
        let path = snapshot_path(&dir);
        let mut index = FileIndex::new();
        index.ensure_root('C');
        index.insert('C', frn(1_000_000), ROOT, "far.txt", false, 0);

        let stats = index
            .save(&path, &PriorityTable::default(), &HashMap::new())
            .unwrap();
        assert!(
            stats.file_bytes > 4_000_000,
            "中间的空洞会占满下标空间，实际 {} 字节",
            stats.file_bytes
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 造一条 USN 记录（只填 [`insert_mft_record`] 用得到的字段）。
    ///
    /// `name_bytes` 要求 `&'a [u8]`，所以把编码结果 leak 掉。
    fn fake_record(frn_value: u64, parent: u64, name: &str, attributes: u32) -> UsnRecord<'static> {
        let bytes: Vec<u8> = name.encode_utf16().flat_map(u16::to_le_bytes).collect();
        let name_bytes: &'static [u8] = Vec::leak(bytes);
        UsnRecord {
            record_length: 0,
            frn: frn_value,
            parent_frn: parent,
            usn: 0,
            timestamp: 0,
            reason: 0,
            attributes,
            name_bytes,
        }
    }

    #[test]
    fn mft_record_becomes_index_entry() {
        // 这一条测的是建索引时真正跑的那段翻译，不需要卷句柄
        let mut index = FileIndex::new();
        index.ensure_root('C');
        let priorities = PriorityTable::with_builtin_defaults();

        let dir = fake_record(frn(10), ROOT, "Windows", FILE_ATTRIBUTE_DIRECTORY);
        assert!(insert_mft_record(&mut index, 'C', &priorities, &dir));
        let file = fake_record(frn(30), frn(10), "notepad.exe", 0);
        assert!(insert_mft_record(&mut index, 'C', &priorities, &file));

        let slot = index.lookup('C', frn(30)).unwrap();
        assert_eq!(index.path_of(slot).as_deref(), Some(r"C:\Windows\notepad.exe"));
        // 目录固定 DIR_PRIORITY，文件按后缀查表
        assert_eq!(index.entries[slot as usize].priority, 30, "exe 是最高档");
        let dir_slot = index.lookup('C', frn(10)).unwrap();
        assert_eq!(index.entries[dir_slot as usize].priority, DIR_PRIORITY);
    }

    #[test]
    fn nameless_mft_record_is_skipped() {
        // 盘根那条记录没有名字，必须跳过；否则索引里会多出一条空名条目
        let mut index = FileIndex::new();
        index.ensure_root('C');
        let root_record = fake_record(ROOT_MFT_INDEX, ROOT_MFT_INDEX, "", 0);
        assert!(!insert_mft_record(
            &mut index,
            'C',
            &PriorityTable::default(),
            &root_record
        ));
        assert_eq!(index.iter_live().count(), 1, "只留下盘根");
    }

    #[test]
    fn config_drops_blank_ignore_paths() {
        let config = IndexConfig::new(vec!['C']);
        assert!(config.drop_previous, "重建默认丢开旧记录");
        assert!(config.ignore_paths.is_empty());

        let config =
            IndexConfig::new(vec!['C']).with_ignore_paths([r"C:\Windows".to_string(), "  ".to_string()]);
        assert_eq!(config.ignore_paths.len(), 1, "空白项会被过滤掉");
    }
}
