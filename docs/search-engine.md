# 文件搜索核心

> `src/search/` 的内部实现。总体架构见 [architecture.md](./architecture.md) §10。

## 1. 为什么自建索引

目标：在启动器里按文件名秒级搜全盘，**不引入任何外部依赖**。

遍历目录树不可行（首次要几分钟）。可行的是直接读 NTFS 的元数据：

| 手段 | 用途 | 代价 |
| --- | --- | --- |
| `FSCTL_ENUM_USN_DATA` 枚举 MFT | 建全量索引，不碰文件内容、不遍历目录 | 需管理员；首次几十秒（取决于文件数） |
| `FSCTL_READ_USN_JOURNAL` 读 USN 日志 | 维护增量，实时反映增删改 | 需管理员 |

**索引常驻内存，磁盘上只有一份快照**——快照的唯一作用是「下次启动免重扫 MFT」，
丢了也能重建。这是理解后面所有取舍的前提。

### 固有代价：需要管理员，但只在小进程里

`CreateFileW(r"\\.\C:")` 请求的是 `GENERIC_READ | GENERIC_WRITE`，所以枚举 MFT 与读
USN 日志都要管理员。**这个代价被隔离在一个只做 MFT/USN 的子进程里**（同一个 exe +
`--index-helper`，用 `runas` 拉起）：

```mermaid
flowchart LR
    subgraph M["主进程（普通用户，常驻托盘）"]
        E[Engine: 索引 + 快照 + 搜索]
        P[索引泵线程]
        E --- P
    end
    subgraph H["索引子进程（提权）"]
        V[Volume / MFT / USN]
    end
    H -- "记录 / 事件（命名管道）" --> P
    P -- "Scan / CatchUp / Monitor / Stop" --> H
```

- 子进程**不持有索引**，只输出「某个盘上哪个 FRN 叫什么、父目录是谁」；
- 索引、快照、匹配、排序全在主进程，所以搜索延迟与提权无关；
- 提权失败不是死路：已有索引照常可搜，只是拿不到离线补课与实时增量。

## 2. 模块划分

| 模块 | 职责 | 进程 |
| --- | --- | --- |
| `win/` | 全部 Win32 交互：卷句柄与 IOCTL、USN 记录与 FRN、盘类型、已知文件夹、命名管道、提权 | 两侧 |
| `pathutil` | 路径纯函数与忽略规则 | 主 |
| `priority` | 后缀优先级表 | 主 |
| `index` | 内存索引、快照编解码、MFT 记录结构与扫描汇 | 主（`MftEntry`/`scan_disk` 两侧共用） |
| `matcher` | 查询解析与匹配规则 | 主 |
| `engine` | 索引生命周期与搜索调度 | 主 |
| `helper` | 协议编解码 + 拉起子进程 + 会话 | 主 |
| `helper_main` | 子进程：扫描、重放、实时跟随 | 子 |
| `monitor` | 日志重放与跟随，按批产出事件 | 子 |

**`unsafe` 只允许出现在 `win/` 里**，其余模块是纯逻辑或纯数据结构，因此可以脱离真机测试。

## 3. 数据布局

```text
names   : Vec<u8>     名称池，所有名字的 UTF-8 字节首尾相接
entries : Vec<Entry>  定长 24 字节的记录数组，全部盘混在一个数组里
free    : Vec<u32>    已删除条目的槽位，供复用
live    : u32         存活条目数
tables  : 盘 → DiskTable {
            slots: MFT记录号 → 主槽位,
            links: MFT记录号 → [额外的硬链接槽位],
            root : 盘根槽位
          }
```

### 3.1 记录（`Entry`）

`#[repr(C)]` + 全定长整数字段，配 `size_of == 24` / `align_of == 8` 的编译期断言，
因此可以按 24 字节整块读写快照。

| 字段 | 类型 | 含义 |
| --- | --- | --- |
| `name_off` | `u32` | 名字在名称池里的起始偏移 |
| `name_len` | `u16` | 名字字节长度（NTFS 文件名上限 255 字符，绝不会溢出） |
| `flags` | `u16` | 位 1 = `FLAG_DEAD`；位 8..13 = 盘符编码 |
| `parent` | `u32` | **父目录的 MFT 记录号**；`NO_PARENT`（`u32::MAX`）表示盘根 |
| `priority` | `i32` | 后缀优先级；目录恒为 `DIR_PRIORITY` |
| `frn` | `u64` | 完整文件引用号，用于识别 MFT 记录复用 |

**只存自己的名字 + 父目录的记录号**，完整路径由 `FileIndex::path_into` 沿途拼出。
这样一条记录 24 字节，百万级文件也只要几十 MB。

⚠️ 盘根的名字直接存成 `C:` 这种形式，所以拼路径不需要特殊分支。
⚠️ 每个盘的盘根记录号都是 5（`ROOT_MFT_INDEX`），所以 `parent_of` **必须查条目自己的盘**，
否则跨盘会命中另一个盘的根。

### 3.2 槽位复用与陈旧引用

删除只做 O(1) 标记 + 回收，**不清查找表里的引用**——清了会让批量删除退化成 O(n²)。

陈旧引用是安全的，由三道检查挡住：

1. `lookup` 核对条目的存活状态；
2. `lookup` 核对**完整 FRN**（不只记录号）——MFT 记录被回收给新文件后序列号会变，
   只比记录号会把新文件当成老文件；
3. `parent_of` 核对父条目确实属于**同一个盘**（槽位是全局复用的，别的盘的条目
   可能占用了这个槽位）。

盘根条目是例外：它记的是首次见到时的 FRN，序列号可能与当下不同，所以不比。

插入是**按 `(FRN, 名字)` 幂等**的：同 FRN 同名字再插一次就地更新；同 FRN 但名字不同
则登记成同一记录号上的另一条链接（见 §3.3）；若该 MFT 记录号上已经是另一个 FRN，
先清掉这条记录号上的旧条目与全部链接。

### 3.3 硬链接：一个 FRN 多个名字

NTFS 的一个文件可以有多个名字（硬链接），枚举 MFT 时会为**每个名字各返回一条记录**，
FRN 与父目录都可能不同。`System32\zh-CN\*.mui`、`CatRoot` 里的 `.cat`、cargo
`target/.../incremental` 下的 `.o` 都是这种形态 —— 只按 FRN 存一条，其余名字会彻底
搜不到（实测本机 136k 个文件里有 14.5k 个属于这种情况，占 10.7%）。

因此：

- `slots[记录号]` 只放**主槽位**，多出来的名字放进 `links[记录号]`（单链接文件不占内存）；
- `lookup(frn)` 只给其中一条，按名字定位用 `lookup_link(frn, name)`；
- 删除事件带 `name`，`remove_link` 只摘掉那一条 —— 不带名字会把同一文件的其它
  链接一起删掉；
- 重命名（`Added.renamed`）在记录号上只有一条链接时**就地改名**，否则只能追加
  （旧名字留给搜索时的懒删除）。

快照只写主槽位映射，`links` 在 `load` 时按「同一 (盘, 记录号) 上的其它存活条目」重建；
主槽位被删掉的情况会把剩下的链接**提升为新的主槽位**。

### 3.4 上限保护

`MAX_MFT_INDEX = 64 M`：超过它的记录号跳过登记，避免损坏的 FRN 让 `slots` 申请 TB 级内存。
宁可搜不到，也不能把内存申请爆。

`MAX_PATH_DEPTH = 256`：异常的 FRN 链会让回溯不收敛，有上限就不会死循环
（`path_into` 超限返回 `false`，宁可不给结果也不拼出错路径）。

## 4. 快照格式

单文件（`index.dat`），大端无关的顺序写入：

```text
magic      : 8 字节  "RASTIDX1"
version    : u32     SNAPSHOT_VERSION
entry_count: u32
names_len  : u32
disk_count : u32
  每个盘：盘符 u8 | root 槽位 u32 | 表长 u32 | 表内容 u32[]
names      : bytes
entries    : 24 字节 × entry_count
priority   : 条数 u32 | (后缀长 u32 | 后缀 bytes | 优先级 i32)[]
marks      : 条数 u32 | (盘符 u8 | journal_id u64 | last_usn i64)[]
checksum   : u64     FNV-1a，覆盖前面全部字节
```

| 设计点 | 理由 |
| --- | --- |
| 写临时文件再改名 | 中途崩溃不会留下半个快照 |
| 删除的槽位不写出去 | 槽位重映射，只保留存活条目 |
| 硬链接槽位不单独写 | 只写主槽位映射，`load` 时按条目里的 FRN 反推，省一段格式 |
| 表尾空洞不写出去 | 尾部连续无用的槽位直接截断 |
| 校验和 | 损坏的快照会导致索引错乱，宁可丢弃 |
| 版本号 | `Entry` 布局、段顺序或**语义**一变就加一，旧文件直接丢弃重建（v2：保留硬链接的每个名字） |

`load` 在**文件不存在、版本不符、校验和不符**时都返回 `Ok(None)`——调用方按「没有快照」
处理，重扫 MFT 即可。快照本来就只是加速手段，不是真相来源。

### 压实

`save` 前先做两件事：

- `prune_orphans`：清掉「祖先链断了」的孤儿。正常不会产生（删目录时 Windows 会为每个
  后代各发一条 USN 记录），但「建库时某个祖先没枚举到」或「监控漏了事件」会留下；
- `compact_names`：重排名称池回收已删名字占的字节（单条名字无法单独回收）。
  垃圾占比不足 1/4 时不动，避免为了省一点内存反复重排。

## 5. 建库：枚举 MFT

```mermaid
flowchart LR
    A["Volume::open<br/>以读写方式打开卷"] --> B["FSCTL_GET_NTFS_VOLUME_DATA<br/>确认 NTFS"]
    B --> C["FSCTL_QUERY_USN_JOURNAL<br/>没有则 CREATE"]
    C --> D["记下位点<br/>mark = (journal_id, NextUsn)"]
    D --> E["FSCTL_ENUM_USN_DATA<br/>HighUsn = NextUsn"]
    E --> F["每 2048 条一批<br/>管道 → 主进程 insert"]
    F --> G["枚举完后<br/>drop_ignored 按忽略规则清理"]
```

关键参数：

- `MFT_ENUM_DATA_V0.HighUsn = ujd.NextUsn`——只枚举「到此刻为止」的记录，
  之后的变更由 USN 日志负责，两边不重不漏；
- 缓冲区 `MFT_BUFFER_LEN = 8 + 1 MB`；
- **先整体写入再按路径过滤**：路径解析要求祖先已在索引里，而 MFT 的记录顺序并不保证
  「父先于子」。所以忽略规则（`drop_ignored`）只能在全部写完后再过一遍。
- 两趟分别在两个进程：枚举在子进程，过滤与收尾在主进程（收到 `ScanFinished` 时）。

⚠️ **位点取的是「枚举开始时」的 `NextUsn`，不是枚举结束后的**。两者之间产生的文件既不在这
次枚举里，也不会被重放；早期实现就踩过这个坑（扫描结束后重新查一次 `NextUsn`）。

## 6. 增量：读 USN 日志

两种用法，同一套翻译（`record_to_event`）：

- **重放**（`replay_disk`）：从快照里的位点补到当前末尾，补完返回新位点；
- **跟随**（`MonitorHandle`）：每盘一条常驻线程，阻塞在 `FSCTL_READ_USN_JOURNAL` 上
  （`BytesToWaitFor = 1` 把它变成阻塞调用）。缓冲区 `USN_BUFFER_LEN = 512 KB`。

⚠️ **跟随必须从「上一个处理到的位点」接着读**，而不是从当前的 `NextUsn` 开始。
重放/扫描结束到线程真正开始读之间总有几百微秒，从 `NextUsn` 起读会把这期间的变更
整段丢掉。`MonitorConfig::start` 就是干这个的。

### 事件过滤

| 情况 | 处理 |
| --- | --- |
| `FILE_CREATE \| FILE_DELETE` 同时出现 | 跳过（临时文件，建了又删） |
| 删除 | 要求 `FILE_DELETE \| CLOSE` 同时成立 |
| 重命名 | 只采用「新名」那条记录（`RENAME_NEW_NAME`） |

### 事件形态

事件是**结构化的，不带路径**，并且**按批交付**（一次 `FSCTL_READ_USN_JOURNAL` 返回多少条
就是一批）：跨进程发送与落索引都有固定开销，逐条交付会把这些开销放大几万倍。

```rust
Added   { disk, frn, parent, name, is_dir, renamed }  // 新增/改名，renamed 区分二者
Removed { disk, frn, name }                           // 只删掉 name 这一条链接
DiskWarning { disk, message }                         // 盘出问题，线程继续重试
DiskStopped(disk)                                     // 线程已退出
```

内存索引按 FRN 组织，有这几个字段就能直接落到索引上，不需要先拼路径。

`Added` 落到索引上是**幂等 upsert**：同一 `(FRN, 名字)` 再收到一次只是刷新父目录与
优先级。`Removed` 必须带名字：硬链接文件删掉一个名字不代表整个文件没了。

### 停止

`CancelSynchronousIo` 打断阻塞中的读取（`win::cancel_synchronous_io`）。
线程不能等它自己超时返回，因为阻塞时长不确定。

## 7. 搜索

```mermaid
flowchart TD
    Q["SearchQuery::parse<br/>按 ; 拆词，AND"] --> P{"查询合法？<br/>非空 且 ≤300 字节"}
    P -->|否| END[结束]
    P -->|是| S1["第一阶段<br/>预扫优先目录"]
    S1 --> S2["第二阶段<br/>顺序扫内存索引"]
    S2 --> S3["按后缀优先级分档<br/>每档最多 max 条"]
    S3 --> S4["按档发布：拼路径 + 校验存在"]
    S4 -->|存在| OUT["push 结果"]
    S4 -->|不存在| GH["记为 ghost"]
    GH --> S5["读锁释放后<br/>拿写锁摘除"]
```

### 7.1 两阶段

**第一阶段：预扫优先目录**——桌面、开始菜单、用户额外指定的目录，直接遍历文件系统
（深度上限 `scan_depth`，默认 4）。这些位置「几乎一定要用」，先扫能让常用程序秒出。

**第二阶段：顺序扫内存索引**——不建倒排索引，就是顺序扫一遍，只比文件名。
实测 120 万条约 25 ms，足够快，换来的是插入/删除都是 O(1) 且实现简单。

### 7.2 分档发布

一次扫描里同时收进多个桶（每个后缀优先级一个桶），扫完再按档从高到低发布。
所以优先级**只影响先后，不影响漏不漏**。

- 目录固定 `DIR_PRIORITY = -1`，永远最后；
- 默认表：`exe`/`lnk` = 30，`url` = 20，`bat`/`cmd`/`com`/`ps1` = 10，其余 = 0；
- 表里查不到的优先级落到最后一档——**排得靠后可以，漏结果不行**。

### 7.3 懒删除

发布每条结果前顺手校验存在性（`win::path_exists`）。**确实不存在**的才记为 ghost，
扫描期间只收集，**等读锁释放后再拿写锁摘除**。

⚠️ 不能用 `Path::exists()`：它把「打不开」也当成「不存在」，长路径（≥260 个 UTF-16 单元）
与权限不足都会走到这里。照单全收会把文件**永久**从索引里摘掉，所以校验是三态的：

| 结果 | 含义 | 处理 |
| --- | --- | --- |
| `Some(true)` | 存在 | 发布 |
| `Some(false)` | `ERROR_FILE_NOT_FOUND` / `ERROR_PATH_NOT_FOUND`（长路径会再加 `\\?\` 前缀复核） | 记为 ghost |
| `None` | 权限不足、共享冲突等判断不了 | 当作存在发布，不动索引 |

> ⚠️ `RwLock` 不可重入，持读锁时不能申请写锁，否则死锁。

### 7.4 匹配规则

查询按 `;` 拆成多个关键字，**全部命中**才算匹配（AND），大小写不敏感。

| 关键字形态 | 判定 | 匹配入口 | 成本 |
| --- | --- | --- | --- |
| 不含 `\` / `/` | 文件名关键字 | `matches_name`（只比字节，不分配） | 低 |
| 含 `\` 或 `/` | 路径关键字 | `matches_parts`（需要完整路径） | **高** |

用哪个由 `needs_dir_for_match()` 统一判定，避免每次比较都判断。

路径关键字要拼完整路径（走一遍祖先链）。实测 120 万条扫一遍：纯文件名约 25 ms，
带路径关键字约 **400 ms**（慢 16 倍）。所以：

- 别拿它去扫整个索引；
- 也不必为它换写法（比如预建路径索引）——用户很少在启动器里敲反斜杠。

### 7.5 会话模型

`Engine::search` 是**发射后不管**的：起一条线程渐进式发布结果，返回 `SearchSession`，
句柄由调用方持有。

| 方法 | 用途 |
| --- | --- |
| `snapshot()` | 取当前已发布的结果（可反复取，结果会增长） |
| `is_done()` | 扫完了没 |
| `wait(timeout)` | 阻塞等到完成或超时 |
| `cancel()` / `cancel_handle()` | 取消 |

不保留 `JoinHandle`：会话销毁时 `Drop` 置取消位，扫描线程很快自行退出，
没有任何调用方需要 join 它。

调用侧（`layout/filesearch.rs`）另加两层限制：搜索超时 5 秒（超时返回已找到的部分）、
结果上限 200 条。

## 8. 索引生命周期

```mermaid
stateDiagram-v2
    [*] --> Open: Engine::open（只读快照）
    Open --> CatchUp: 有快照且每个盘都有位点
    Open --> Scan: 无快照 / 有盘缺位点
    CatchUp --> Monitoring: 重放完毕 → 发 Monitor
    CatchUp --> Scan: 位点失效（NeedsRebuild）
    CatchUp --> Monitoring: 打不开卷（Unavailable，沿用旧索引）
    Scan --> Monitoring: 所有盘 ScanFinished → 发 Monitor
    Monitoring --> Monitoring: 累计变更后落快照
    Monitoring --> [*]: 子进程退出 / 管道断开
```

| 阶段 | 谁做 | 做什么 |
| --- | --- | --- |
| `open` | 主 | 读快照（有就用）、确定盘、准备优先级表。**打不开快照不算错误** |
| `catch_up` | 子 | 从快照里的位点把日志重放到当前末尾，事件逐批发给主进程 |
| `scan` | 子 | 全量枚举 MFT，记录逐批发给主进程 |
| `apply` | 主 | `begin_scan` / `insert_scanned_batch` / `finish_scan`，或 `apply_event` |
| `monitor` | 子 | 启每盘的跟随线程，从各盘位点接着读 |
| `save_snapshot` | 主 | 压实 + 孤儿清理 + 写盘 |

「补课」的每种结局都在协议里显式建模：

| 消息 | 含义 |
| --- | --- |
| `CatchUpFinished` | 从位点补齐了这么多条变更，带上新位点 |
| `NeedsRebuild` | 位点已被日志覆盖/日志被重建，只能全量重扫 |
| `Unavailable(reason)` | 补不了（没权限打不开卷），沿用快照时的索引 |

### 落盘时机

- 建库/补课刚结束时立刻落一次（下次启动才能直接补课）；
- 之后每累计 **2000 条**变更落一次（`SNAPSHOT_EVERY`）。

用计数而不是定时器：泵线程只会在收到消息时醒，没必要为了查表再起一个心跳。
没来得及落的变更不会丢——下次启动会从更早的位点重放回来。

## 9. 并发

| 数据 | 同步原语 | 谁读 / 谁写 |
| --- | --- | --- |
| 内存索引 | `RwLock<FileIndex>` | 搜索持读锁；增量更新持写锁 |
| 优先级表 | `RwLock<PriorityTable>` | 读多写少 |
| 每盘位点 | `Mutex<HashMap<char, JournalMark>>` | 随快照落盘 |

**锁的顺序约束**（记住这两条就不会死锁）：

1. 持索引读锁时**不能**申请写锁（`RwLock` 不可重入）→ 懒删除因此分成两段；
2. 先拿读锁、用完释放，再拿写锁。

## 10. 错误处理

统一错误类型 `CoreError`，关键是**保留失败的系统调用名**——光有错误码很难定位问题：

| 变体 | 场景 |
| --- | --- |
| `Io` | 读写快照、目录遍历、管道 |
| `Win32 { api, err }` | Win32 失败，`api` 形如 `"DeviceIoControl(FSCTL_ENUM_USN_DATA)"` |
| `UnsupportedDisk { letter, reason }` | 不是「本地固定盘 + NTFS」 |
| `InvalidArgument` | 空关键字、超长、子进程参数不全等 |
| `ElevationDenied` | 用户在 UAC 上点了「否」 |
| `Helper(msg)` | 管道断开、握手失败、协议数据损坏 |

只有「本地固定盘 + NTFS」能索引（`win/disk.rs`）：可移动盘、网络盘、非 NTFS 卷都会被
明确拒绝并说明原因，而不是悄悄给一个查不到东西的索引。

## 11. 常量表

| 常量 | 值 | 作用 |
| --- | --- | --- |
| `DEFAULT_MAX_RESULTS` | 200 | 单次搜索默认上限 |
| `MAX_SEARCH_TEXT_LEN` | 300 | 超长查询直接放弃 |
| `MFT_BUFFER_LEN` | `8 + 1 MB` | MFT 枚举缓冲 |
| `USN_BUFFER_LEN` | 512 KB | USN 读取缓冲 |
| `MAX_PATH_DEPTH` | 256 | 路径回溯上限 |
| `MAX_MFT_INDEX` | 64 M | 记录号登记上限 |
| `SNAPSHOT_VERSION` | 2 | 快照格式版本 |
| `DIR_PRIORITY` | -1 | 目录优先级 |
| `RECORD_HEADER_LEN` | 60 | `USN_RECORD_V2` 到 `FileName` 为止 |
| `ROOT_MFT_INDEX` | 5 | 每个盘的根记录号 |
| `PROTOCOL_VERSION` | 1 | 管道协议版本（字段一变就加一） |

子进程侧（`search/helper_main.rs`）：

| 常量 | 值 | 作用 |
| --- | --- | --- |
| `RECORD_BATCH` | 2048 | 一批往主进程发多少条扫描记录 |
| `CONNECT_TIMEOUT` | 10 s | 连接主进程管道的超时 |

集成层（`layout/filesearch.rs`）：

| 常量 | 值 | 作用 |
| --- | --- | --- |
| `SEARCH_TIMEOUT` | 5 s | 超时返回已找到的部分 |
| `RESULT_LIMIT` | 200 | 展示上限 |
| `SNAPSHOT_EVERY` | 2000 | 累计多少条变更落一次快照 |

## 12. 测试

`unsafe` 被隔离在 `win/`，所以纯逻辑部分可以完整测试，不驻留 GUI、不接触真实磁盘。
按主题分布（共 50+ 个用例）：

| 主题 | 代表性用例 |
| --- | --- |
| 查询解析与匹配 | `splits_keywords_on_semicolon`、`multiple_keywords_are_and_on_fast_path`、`path_keyword_requires_dir_aware_matching`、`fast_and_slow_paths_agree`、`fast_path_does_not_false_match_multibyte` |
| 索引增删改 | `empty_name_and_duplicate_insert`、`remove_locates_by_frn_and_is_idempotent`、`reused_mft_record_is_distinguished_by_sequence`、`hard_link_names_are_all_indexed`、`removing_one_hard_link_keeps_the_others`、`rename_collapses_single_link_and_updates_children`、`reusing_an_mft_record_drops_all_its_links` |
| 路径拼接 | `path_is_assembled_from_ancestors`、`renaming_parent_updates_children_immediately`、`deleted_directory_yields_no_path_not_a_wrong_path`、`self_reference_and_cycles_do_not_hang`、`depth_limit_gives_up_instead_of_looping` |
| 边界保护 | `huge_mft_record_number_does_not_blow_up_memory`、`insert_does_not_require_parent_to_come_first`、`multiple_disks_are_independent` |
| 快照 | `snapshot_roundtrip`、`corrupt_snapshot_is_treated_as_absent`、`snapshot_drops_deleted_entries_and_compacts_names`、`snapshot_trims_lookup_table_tail`、`lookup_table_middle_holes_are_written_as_is`、`snapshot_roundtrip_keeps_hard_links` |
| 维护 | `orphans_are_pruned`、`compact_names_reclaims_garbage`、`ignore_rules_filter_by_path` |
| 搜索调度 | `results_are_capped_by_limit`、`results_are_deduped_and_stable`、`lazy_delete_removes_records_that_no_longer_exist`、`path_exists_tells_found_from_missing`、`path_exists_follows_long_paths_instead_of_reporting_missing`、`priority_tiers_put_directories_last`、`priority_folder_files_found_without_index`、`cancelled_session_finishes_soon`、`empty_and_oversized_queries_scan_nothing` |
| 子进程协议 | `every_message_survives_a_roundtrip`、`every_request_survives_a_roundtrip`、`garbage_payload_is_rejected`、`frames_carry_their_length`、`pipe_name_is_unique_and_current_process`、`pipe_carries_a_large_batch_between_threads`、`accept_gives_up_when_nobody_connects` |
| 启动策略 | `fresh_install_scans`、`index_with_all_marks_only_catches_up`、`a_disk_without_mark_forces_a_rescan`、`no_disks_never_catches_up` |
| 增量 | `monitor_events_land_in_index_and_can_be_removed`、`hard_linked_file_is_searchable_under_every_name`、`renaming_directory_updates_all_child_paths`、`event_with_unknown_parent_is_rejected`、`stale_entries_of_unused_disks_are_dropped_on_open` |

需要真机或大量资源的用例标了 `#[ignore]`：

```bash
# 真实规模下的搜索性能（120 万条）
cargo test --release --bin rastflow benchmark_search_at_real_scale -- --ignored --nocapture
# 真实 NTFS 卷上的建库与增量（需管理员）
cargo test --bin rastflow -- --ignored
```
