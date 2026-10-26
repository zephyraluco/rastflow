//! 桌面与服务共享的文件系统变更模型。
use super::index::JournalMark;

/// 监控线程抛出的事件。内容结构化（见模块文档），落到索引上是幂等 upsert。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MonitorEvent {
    /// 新增，或重命名/移动之后的**新样子**
    Added {
        disk: char,
        /// 文件引用号 `(序列号 << 48) | 记录号`
        frn: u64,
        /// 父目录的 FRN
        parent: u64,
        /// 文件名（不含路径）
        name: String,
        is_dir: bool,
        /// 是重命名/移动（同一 FRN 换了名字），而不是新建或新增硬链接。
        /// 索引据此决定「就地改名」还是「再加一条链接」。
        renamed: bool,
    },
    /// 已删除（或已移出本盘）。
    ///
    /// 带 `name` 是必须的：NTFS 硬链接的同一个 FRN 有多个名字，删除事件删掉的
    /// 只是其中一个名字，不带名字会把同一文件的其它链接一起删掉。
    Removed {
        disk: char,
        frn: u64,
        /// 被删掉的那个名字；空串表示事件里没有名字（调用方退化成按 FRN 删）
        name: String,
    },
    /// 某个盘出问题（打不开卷、日志不可用等），线程会继续重试
    DiskWarning { disk: char, message: String },
    /// 某个盘的跟随线程已退出
    DiskStopped(char),
    /// 同批事件成功应用后才能推进到此位点。
    Checkpoint { disk: char, mark: JournalMark },
}
