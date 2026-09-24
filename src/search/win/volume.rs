//! 卷句柄与 USN 日志 IOCTL。一个 [`Volume`] 就是 `\\.\X:` 的句柄，MFT 枚举与
//! USN 日志的查询 / 创建 / 读取都从这里发出。
//!
//! `CreateFileW(r"\\.\C:")` 请求 `GENERIC_READ | GENERIC_WRITE`，**需要管理员权限**。
//!
//! 两个关键参数：`MFT_ENUM_DATA_V0.HighUsn = ujd.NextUsn`（缓冲区 `sizeof(USN) + 1MB`）；
//! `BytesToWaitFor = 1` 把 `FSCTL_READ_USN_JOURNAL` 变成阻塞调用。

use std::ffi::c_void;
use std::mem::size_of;

use windows::Win32::Foundation::{CloseHandle, GENERIC_READ, GENERIC_WRITE, HANDLE};
use windows::Win32::Storage::FileSystem::{
    CreateFileW, FILE_FLAGS_AND_ATTRIBUTES, FILE_SHARE_MODE, FILE_SHARE_READ, FILE_SHARE_WRITE,
    OPEN_EXISTING,
};
use windows::Win32::System::IO::DeviceIoControl;
use windows::Win32::System::Ioctl::{
    CREATE_USN_JOURNAL_DATA, FSCTL_CREATE_USN_JOURNAL, FSCTL_ENUM_USN_DATA,
    FSCTL_GET_NTFS_VOLUME_DATA, FSCTL_QUERY_USN_JOURNAL, FSCTL_READ_USN_JOURNAL,
    MFT_ENUM_DATA_V0, NTFS_VOLUME_DATA_BUFFER, READ_USN_JOURNAL_DATA_V0, USN_JOURNAL_DATA_V0,
};
use windows::core::PCWSTR;

use super::super::{CoreError, MFT_BUFFER_LEN, Result, USN_BUFFER_LEN};
use super::to_wide;
use super::usn::{self, UsnRecord};

/// 一个已打开的卷（`\\.\X:`）
pub struct Volume {
    handle: HANDLE,
}

impl Volume {
    /// 打开卷并要求读写权限（需要管理员）
    pub fn open(letter: char) -> Result<Self> {
        let letter = letter.to_ascii_uppercase();
        let device = format!(r"\\.\{letter}:");
        let wide = to_wide(&device);
        let handle = unsafe {
            CreateFileW(
                PCWSTR(wide.as_ptr()),
                GENERIC_READ.0 | GENERIC_WRITE.0,
                // 共享读写，但不共享删除：我们在读这个卷时别让它被删掉
                FILE_SHARE_MODE(FILE_SHARE_READ.0 | FILE_SHARE_WRITE.0),
                None,
                OPEN_EXISTING,
                FILE_FLAGS_AND_ATTRIBUTES(0),
                None,
            )
        }
        .map_err(|e| CoreError::win32("CreateFileW(\\.\\<盘符>:)", e))?;

        Ok(Self { handle })
    }

    // ── USN 日志 ────────────────────────────────────────────────────────

    /// `FSCTL_QUERY_USN_JOURNAL`
    pub fn query_journal(&self) -> Result<USN_JOURNAL_DATA_V0> {
        let mut data = USN_JOURNAL_DATA_V0::default();
        ioctl_raw(
            self.handle,
            FSCTL_QUERY_USN_JOURNAL,
            None,
            0,
            &mut data as *mut _ as *mut c_void,
            size_of::<USN_JOURNAL_DATA_V0>() as u32,
            "DeviceIoControl(FSCTL_QUERY_USN_JOURNAL)",
        )?;
        Ok(data)
    }

    /// `FSCTL_CREATE_USN_JOURNAL`
    pub fn create_journal(&self) -> Result<()> {
        let input = CREATE_USN_JOURNAL_DATA {
            MaximumSize: 0,
            AllocationDelta: 0,
        };
        ioctl_raw(
            self.handle,
            FSCTL_CREATE_USN_JOURNAL,
            Some(&input as *const _ as *const c_void),
            size_of::<CREATE_USN_JOURNAL_DATA>() as u32,
            std::ptr::null_mut(),
            0,
            "DeviceIoControl(FSCTL_CREATE_USN_JOURNAL)",
        )?;
        Ok(())
    }

    /// 取出日志信息；查询失败就说明这个卷还没启用日志，创建后再查一次
    pub fn ensure_journal(&self) -> Result<USN_JOURNAL_DATA_V0> {
        match self.query_journal() {
            Ok(data) => Ok(data),
            Err(_) => {
                self.create_journal()?;
                self.query_journal()
            }
        }
    }

    /// 用 `FSCTL_GET_NTFS_VOLUME_DATA` 确认这确实是 NTFS 卷
    ///
    /// 非 NTFS 上前面的 USN IOCTL 会以各种含糊的错误码失败，提前确认能给出清楚的错误。
    pub fn verify_ntfs(&self) -> Result<()> {
        let mut data = NTFS_VOLUME_DATA_BUFFER::default();
        ioctl_raw(
            self.handle,
            FSCTL_GET_NTFS_VOLUME_DATA,
            None,
            0,
            &mut data as *mut _ as *mut c_void,
            size_of::<NTFS_VOLUME_DATA_BUFFER>() as u32,
            "DeviceIoControl(FSCTL_GET_NTFS_VOLUME_DATA)",
        )?;
        Ok(())
    }

    // ── 读取 ───────────────────────────────────────────────────────────

    /// 枚举整个 MFT：`FSCTL_ENUM_USN_DATA` 翻页直到没有更多记录，返回记录条数。
    ///
    /// 缓冲区开头的 8 字节是**下一页的起始 FRN**（不是 USN），用作翻页游标。
    pub fn enum_usn_records<F>(&self, max_usn: i64, mut on_record: F) -> Result<u64>
    where
        F: FnMut(UsnRecord<'_>),
    {
        let mut buf = vec![0u8; MFT_BUFFER_LEN];
        let mut cursor = 0u64;
        let mut total = 0u64;

        loop {
            let (next, count) =
                self.enum_usn_page(cursor, max_usn, &mut buf, &mut |record| on_record(record))?;
            total += u64::from(count);
            // 游标没有前进说明到底了；同时防住异常情况下死循环
            if next == cursor {
                break;
            }
            cursor = next;
        }

        Ok(total)
    }

    /// 只发**一次** `FSCTL_ENUM_USN_DATA`，返回 `(下一页起始 FRN, 本页记录数)`。
    ///
    /// 从 `start_frn` 开始枚举时，返回的**第一条记录就是 `start_frn` 自己**，
    /// 所以带上 `start_frn` 也能只查一个 FRN。
    pub fn enum_usn_page(
        &self,
        start_frn: u64,
        max_usn: i64,
        buf: &mut Vec<u8>,
        on_record: &mut dyn FnMut(UsnRecord<'_>),
    ) -> Result<(u64, u32)> {
        if buf.len() < MFT_BUFFER_LEN {
            buf.resize(MFT_BUFFER_LEN, 0);
        }
        let input = MFT_ENUM_DATA_V0 {
            StartFileReferenceNumber: start_frn,
            LowUsn: 0,
            HighUsn: max_usn,
        };
        let mut filled = 0u32;
        let result = unsafe {
            DeviceIoControl(
                self.handle,
                FSCTL_ENUM_USN_DATA,
                Some(&input as *const MFT_ENUM_DATA_V0 as *const c_void),
                size_of::<MFT_ENUM_DATA_V0>() as u32,
                Some(buf.as_mut_ptr() as *mut c_void),
                buf.len() as u32,
                Some(&mut filled),
                None,
            )
        };
        // 失败即枚举结束（正常的结束是 ERROR_HANDLE_EOF），不是错误
        if result.is_err() {
            return Ok((start_frn, 0));
        }
        let filled = filled as usize;
        if filled < usn::LEADING_USN_LEN {
            return Ok((start_frn, 0));
        }

        let chunk = &buf[..filled];
        let mut count = 0u32;
        for record in usn::iterate(chunk) {
            count += 1;
            on_record(record);
        }
        // 位模式与 DWORDLONG 相同，直接按 u64 用
        let next = usn::leading_usn(chunk).map(|v| v as u64).unwrap_or(start_frn);
        Ok((next, count))
    }

    /// 读 USN 日志：`FSCTL_READ_USN_JOURNAL`。
    ///
    /// `wait = true` 时（`BytesToWaitFor = 1`）调用会**阻塞**到有新记录为止；
    /// 返回 `(下一个起始 USN, 本次读到的记录数)`。
    pub fn read_usn_records<F>(
        &self,
        journal_id: u64,
        start_usn: i64,
        wait: bool,
        mut on_record: F,
    ) -> Result<(i64, u32)>
    where
        F: FnMut(UsnRecord<'_>),
    {
        let input = READ_USN_JOURNAL_DATA_V0 {
            StartUsn: start_usn,
            ReasonMask: 0xFFFF_FFFF, // 所有变更原因
            ReturnOnlyOnClose: 0,    // 不等 close，每条都返回
            Timeout: 0,              // 不超时
            BytesToWaitFor: if wait { 1 } else { 0 },
            UsnJournalID: journal_id,
        };
        let mut buf = vec![0u8; USN_BUFFER_LEN];
        let filled = ioctl_raw(
            self.handle,
            FSCTL_READ_USN_JOURNAL,
            Some(&input as *const _ as *const c_void),
            size_of::<READ_USN_JOURNAL_DATA_V0>() as u32,
            buf.as_mut_ptr() as *mut c_void,
            buf.len() as u32,
            "DeviceIoControl(FSCTL_READ_USN_JOURNAL)",
        )? as usize;

        let chunk = &buf[..filled];
        let next_usn = usn::leading_usn(chunk).unwrap_or(start_usn);
        let mut count = 0u32;
        for record in usn::iterate(chunk) {
            count += 1;
            on_record(record);
        }
        Ok((next_usn, count))
    }
}

impl Drop for Volume {
    fn drop(&mut self) {
        let _ = unsafe { CloseHandle(self.handle) };
    }
}

/// 调一次 `DeviceIoControl`，输入输出都用裸指针（读/写返回整块缓冲区时用）
fn ioctl_raw(
    handle: HANDLE,
    code: u32,
    input: Option<*const c_void>,
    input_size: u32,
    output: *mut c_void,
    output_size: u32,
    api: &'static str,
) -> Result<u32> {
    let mut filled = 0u32;
    unsafe {
        DeviceIoControl(
            handle,
            code,
            input,
            input_size,
            if output.is_null() { None } else { Some(output) },
            output_size,
            Some(&mut filled),
            None,
        )
    }
    .map_err(|e| CoreError::win32(api, e))?;
    Ok(filled)
}
