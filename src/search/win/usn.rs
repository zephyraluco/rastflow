//! USN 记录格式、FRN 编码与 MFT 记录号。
//!
//! 偏移量对照 `ntfs.h` 的 `USN_RECORD_V2`：
//!
//! ```c
//! typedef struct {
//!     DWORD         RecordLength;              //  0
//!     WORD          MajorVersion;              //  4
//!     WORD          MinorVersion;              //  6
//!     DWORDLONG     FileReferenceNumber;       //  8
//!     DWORDLONG     ParentFileReferenceNumber; // 16
//!     USN           Usn;                       // 24  (LONGLONG)
//!     LARGE_INTEGER TimeStamp;                 // 32
//!     DWORD         Reason;                    // 40
//!     DWORD         SourceInfo;                // 44
//!     DWORD         SecurityId;                // 48
//!     DWORD         FileAttributes;            // 52
//!     WORD          FileNameLength;            // 56  (字节数)
//!     WORD          FileNameOffset;            // 58  (相对记录起始的字节偏移)
//!     WCHAR         FileName[1];               // 60
//! } USN_RECORD_V2;
//! ```

/// 记录头长度（到 `FileName` 为止）
pub const RECORD_HEADER_LEN: usize = 60;

/// `DeviceIoControl` 返回缓冲区开头的 `USN`（下一个起始 USN）长度
pub const LEADING_USN_LEN: usize = 8;

/// `FILE_ATTRIBUTE_DIRECTORY`
pub const FILE_ATTRIBUTE_DIRECTORY: u32 = 0x10;

// ── USN_REASON_*（来自 winioctl.h）──────────────────────────────────────
pub const USN_REASON_FILE_CREATE: u32 = 0x0000_0100;
pub const USN_REASON_FILE_DELETE: u32 = 0x0000_0200;
pub const USN_REASON_RENAME_OLD_NAME: u32 = 0x0000_1000;
pub const USN_REASON_RENAME_NEW_NAME: u32 = 0x0000_2000;
pub const USN_REASON_CLOSE: u32 = 0x8000_0000;

/// 一条解析好的 USN 记录（借用了缓冲区里的文件名字节）
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct UsnRecord<'a> {
    /// 记录总长度（含文件名与对齐填充）
    pub record_length: u32,
    /// 文件/目录的 FRN
    pub frn: u64,
    /// 父目录的 FRN
    pub parent_frn: u64,
    /// 该次变更的 USN
    pub usn: i64,
    /// 变更时间（FILETIME）
    pub timestamp: i64,
    /// `USN_REASON_*` 位掩码
    pub reason: u32,
    /// `FILE_ATTRIBUTE_*` 位掩码
    pub attributes: u32,
    /// 文件名的原始字节（little-endian UTF-16，长度是偶数）
    pub name_bytes: &'a [u8],
}

impl<'a> UsnRecord<'a> {
    /// 从缓冲区 `offset` 处解析一条记录；越界或字段异常返回 `None`。
    pub fn parse(buf: &'a [u8], offset: usize) -> Option<Self> {
        let record_length = read_u32(buf, offset)?;
        // 长度必须至少放下头部，且整条记录都在缓冲区内
        if (record_length as usize) < RECORD_HEADER_LEN {
            return None;
        }
        let record_end = offset.checked_add(record_length as usize)?;
        if record_end > buf.len() {
            return None;
        }

        let name_length = read_u16(buf, offset + 56)? as usize;
        let name_offset = read_u16(buf, offset + 58)? as usize;
        // 文件名必须落在记录内部，且不覆盖头部
        if name_offset < RECORD_HEADER_LEN {
            return None;
        }
        let name_start = offset.checked_add(name_offset)?;
        let name_end = name_start.checked_add(name_length)?;
        if name_end > record_end || !name_length.is_multiple_of(2) {
            return None;
        }

        Some(Self {
            record_length,
            frn: read_u64(buf, offset + 8)?,
            parent_frn: read_u64(buf, offset + 16)?,
            usn: read_i64(buf, offset + 24)?,
            timestamp: read_i64(buf, offset + 32)?,
            reason: read_u32(buf, offset + 40)?,
            attributes: read_u32(buf, offset + 52)?,
            name_bytes: &buf[name_start..name_end],
        })
    }

    /// 文件名的 UTF-16 码元迭代器（不分配内存）
    pub fn name_units(&self) -> impl Iterator<Item = u16> + '_ {
        self.name_bytes
            .as_chunks::<2>()
            .0
            .iter()
            .map(|pair| u16::from_le_bytes(*pair))
    }

    /// 文件名，非法 UTF-16 用替换字符代替（等价于 `String::from_utf16_lossy`）
    pub fn name_lossy(&self) -> String {
        char::decode_utf16(self.name_units())
            .map(|r| r.unwrap_or(char::REPLACEMENT_CHARACTER))
            .collect()
    }

    /// 是否是目录
    pub fn is_directory(&self) -> bool {
        self.attributes & FILE_ATTRIBUTE_DIRECTORY != 0
    }

    /// 是否发生了重命名（旧名或新名任一）
    pub fn is_rename(&self) -> bool {
        self.reason & (USN_REASON_RENAME_OLD_NAME | USN_REASON_RENAME_NEW_NAME) != 0
    }
}

/// 迭代 `DeviceIoControl` 返回缓冲区里的所有记录（自动跳过开头的 `USN`）。
pub fn iterate(buf: &[u8]) -> Records<'_> {
    Records {
        buf,
        offset: LEADING_USN_LEN,
        finished: false,
    }
}

/// 读取缓冲区开头的「下一个起始 USN」
pub fn leading_usn(buf: &[u8]) -> Option<i64> {
    read_i64(buf, 0)
}

pub struct Records<'a> {
    buf: &'a [u8],
    offset: usize,
    finished: bool,
}

impl<'a> Iterator for Records<'a> {
    type Item = UsnRecord<'a>;

    fn next(&mut self) -> Option<Self::Item> {
        if self.finished {
            return None;
        }
        match UsnRecord::parse(self.buf, self.offset) {
            Some(record) => {
                self.offset += record.record_length as usize;
                Some(record)
            }
            // 解析失败即认为缓冲区已结束（避免因坏记录陷入死循环）
            None => {
                self.finished = true;
                None
            }
        }
    }
}

fn read_u16(buf: &[u8], offset: usize) -> Option<u16> {
    let bytes = buf.get(offset..offset + 2)?;
    Some(u16::from_le_bytes([bytes[0], bytes[1]]))
}

fn read_u32(buf: &[u8], offset: usize) -> Option<u32> {
    let bytes = buf.get(offset..offset + 4)?;
    Some(u32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]))
}

fn read_u64(buf: &[u8], offset: usize) -> Option<u64> {
    let bytes = buf.get(offset..offset + 8)?;
    Some(u64::from_le_bytes(bytes.try_into().ok()?))
}

fn read_i64(buf: &[u8], offset: usize) -> Option<i64> {
    read_u64(buf, offset).map(|v| v as i64)
}

// ── FRN 编码 ────────────────────────────────────────────────────────────
//
// `FileReferenceNumber` = `(序列号 << 48) | 记录号`，不是裸记录号：
// 认盘根要比低 48 位（真实根 FRN 形如 `0x0005_0000_0000_0005`），
// 比对要连序列号一起比（MFT 记录被回收给新文件时只有序列号变）。

/// 根目录的 **MFT 记录号**
pub const ROOT_MFT_INDEX: u64 = 5;

/// 取出 FRN 低 48 位的 MFT 记录号
pub const fn mft_index(frn: u64) -> u64 {
    frn & 0x0000_FFFF_FFFF_FFFF
}

/// 这个 FRN 是不是盘根
pub fn is_root(frn: u64) -> bool {
    mft_index(frn) == ROOT_MFT_INDEX
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 按 ntfs.h 的布局构造一条记录
    fn build_record(frn: u64, parent: u64, name: &str, reason: u32, attrs: u32) -> Vec<u8> {
        let name_utf16: Vec<u16> = name.encode_utf16().collect();
        let name_len = name_utf16.len() * 2;
        let total = (RECORD_HEADER_LEN + name_len + 7) & !7usize; // 8 字节对齐
        let mut buf = vec![0u8; total];
        buf[0..4].copy_from_slice(&(total as u32).to_le_bytes());
        buf[4..6].copy_from_slice(&2u16.to_le_bytes()); // MajorVersion
        buf[6..8].copy_from_slice(&0u16.to_le_bytes());
        buf[8..16].copy_from_slice(&frn.to_le_bytes());
        buf[16..24].copy_from_slice(&parent.to_le_bytes());
        buf[24..32].copy_from_slice(&1_000i64.to_le_bytes()); // Usn
        buf[32..40].copy_from_slice(&133_000_000_000_000_000i64.to_le_bytes());
        buf[40..44].copy_from_slice(&reason.to_le_bytes());
        buf[52..56].copy_from_slice(&attrs.to_le_bytes());
        buf[56..58].copy_from_slice(&(name_len as u16).to_le_bytes());
        buf[58..60].copy_from_slice(&(RECORD_HEADER_LEN as u16).to_le_bytes());
        for (i, unit) in name_utf16.iter().enumerate() {
            let at = RECORD_HEADER_LEN + i * 2;
            buf[at..at + 2].copy_from_slice(&unit.to_le_bytes());
        }
        buf
    }

    /// 拼成 DeviceIoControl 的返回缓冲区：开头 8 字节 USN + 若干记录
    fn build_buffer(next_usn: i64, records: &[Vec<u8>]) -> Vec<u8> {
        let mut buf = Vec::new();
        buf.extend_from_slice(&next_usn.to_le_bytes());
        for record in records {
            buf.extend_from_slice(record);
        }
        buf
    }

    #[test]
    fn parses_all_fields_of_one_record() {
        let record = build_record(
            0x0001_0000_0000_1234,
            // 父 FRN 同样带序列号：低 48 位是记录号，高 16 位是序列号
            0x0005_0000_0000_0005,
            "notepad.exe",
            USN_REASON_FILE_CREATE | USN_REASON_CLOSE,
            0x20,
        );
        let buf = build_buffer(9999, &[record]);

        let parsed = UsnRecord::parse(&buf, LEADING_USN_LEN).expect("应能解析");
        assert_eq!(parsed.frn, 0x0001_0000_0000_1234);
        assert_eq!(parsed.parent_frn, 0x0005_0000_0000_0005);
        assert_eq!(parsed.usn, 1000);
        assert_eq!(parsed.name_lossy(), "notepad.exe");
        assert!(!parsed.is_directory());
        assert!(!parsed.is_rename());
    }

    #[test]
    fn iterates_records_and_reads_next_usn() {
        let records = vec![
            build_record(1, 5, "a.txt", USN_REASON_FILE_CREATE, 0),
            build_record(2, 5, "b", USN_REASON_FILE_CREATE, FILE_ATTRIBUTE_DIRECTORY),
            build_record(3, 2, "c.txt", USN_REASON_FILE_CREATE, 0),
        ];
        let buf = build_buffer(4242, &records);

        assert_eq!(leading_usn(&buf), Some(4242));
        let names: Vec<String> = iterate(&buf).map(|r| r.name_lossy()).collect();
        assert_eq!(names, vec!["a.txt", "b", "c.txt"]);

        let second = iterate(&buf).nth(1).unwrap();
        assert!(second.is_directory());
    }

    #[test]
    fn restores_cjk_and_emoji_names() {
        for name in ["中文名.txt", "emoji😀.png", "mixed中英"] {
            let buf = build_buffer(0, &[build_record(7, 5, name, 0, 0)]);
            let parsed = iterate(&buf).next().expect("应能解析");
            assert_eq!(parsed.name_lossy(), name);
            // 码元数与编码结果一致
            assert_eq!(
                parsed.name_units().count(),
                name.encode_utf16().count()
            );
        }
    }

    #[test]
    fn out_of_range_and_bad_records_do_not_panic() {
        // 空缓冲区
        assert!(iterate(&[]).next().is_none());
        // 只有开头 8 字节
        assert!(iterate(&build_buffer(1, &[])).next().is_none());
        // 记录长度超出缓冲区 → 直接结束，不死循环
        let mut truncated = build_buffer(1, &[build_record(1, 5, "abc", 0, 0)]);
        truncated.truncate(LEADING_USN_LEN + RECORD_HEADER_LEN + 2);
        assert!(iterate(&truncated).next().is_none());
        // 长度字段为 0
        let mut zero_len = build_buffer(1, &[build_record(1, 5, "abc", 0, 0)]);
        zero_len[LEADING_USN_LEN..LEADING_USN_LEN + 4].copy_from_slice(&0u32.to_le_bytes());
        assert!(iterate(&zero_len).next().is_none());
    }

    #[test]
    fn reason_bit_checks() {
        let rename = build_record(1, 5, "x", USN_REASON_RENAME_OLD_NAME, 0);
        let buf = build_buffer(0, &[rename]);
        assert!(iterate(&buf).next().unwrap().is_rename());

        let delete_close = build_record(1, 5, "x", USN_REASON_FILE_DELETE | USN_REASON_CLOSE, 0);
        let buf = build_buffer(0, &[delete_close]);
        let parsed = iterate(&buf).next().unwrap();
        assert_eq!(
            parsed.reason & USN_REASON_FILE_DELETE,
            USN_REASON_FILE_DELETE
        );
        assert_eq!(parsed.reason & USN_REASON_CLOSE, USN_REASON_CLOSE);
    }

    #[test]
    fn root_frn_with_sequence_number_is_recognized() {
        // 真实 USN 记录里的 FRN 带 16 位序列号：根是 0x0005_0000_0000_0005
        const ROOT: u64 = 0x0005_0000_0000_0005;
        assert!(is_root(ROOT));
        assert!(is_root(ROOT_MFT_INDEX));
        assert_eq!(mft_index(ROOT), ROOT_MFT_INDEX);
        assert!(!is_root(100));

        // 直接跟 5 比会漏掉前者 —— 这就是必须有这个函数的原因
        assert_ne!(ROOT, ROOT_MFT_INDEX);
    }

    #[test]
    fn same_record_number_different_generation_is_distinguished() {
        // MFT 记录被回收给新文件后只有序列号变
        let first = (1u64 << 48) | 100;
        let second = (2u64 << 48) | 100;
        assert_ne!(first, second);
        assert_eq!(mft_index(first), mft_index(second));
    }

    #[test]
    fn usn_record_fields_locate_parent() {
        // 从字节流里读出的父 FRN 要能直接拆成盘根记录号
        let record_bytes = build_record(1234, ROOT_MFT_INDEX, "ab中c", USN_REASON_FILE_CREATE, 0x10);
        let buf = build_buffer(0, &[record_bytes]);
        let record = iterate(&buf).next().expect("应能解析");
        assert_eq!(record.name_lossy(), "ab中c");
        assert!(record.is_directory());
        assert_eq!(mft_index(record.parent_frn), ROOT_MFT_INDEX);
    }
}
