# rastflow

`rastflow` 是一个基于 Rust + GPUI 的 Windows 桌面快速启动器

## 主要功能

- 程序搜索与快速启动
- 系统托盘常驻（显示/隐藏、退出）
- 全局快捷键唤出（默认 `Alt + Space`）
- 自定义程序管理
- 文件搜索模式（通过 `Tab` 切换）：自建索引，**不依赖 Everything**
- 基础设置持久化（主题、语言、开机自启、热键等）

## 文件索引

文件搜索使用 `src/search` 里的自研索引：直接枚举 NTFS 主文件表（MFT）建立全量索引，
再用 USN 日志维护增量，不遍历目录树，也不依赖 Any 第三方搜索工具。

需要留意三点：

- **首次建立索引需要管理员权限**（读取卷主文件表要求写权限）。界面上的
  「安装索引服务」会以管理员身份注册一个 Windows 服务（`rastflow-index`），
  之后每次启动都不再弹 UAC —— 和 Everything 的做法一样。
- 不装服务也能用：主程序会退化成「每次启动拉起一个提权子进程」，代价是每次都要
  过一次 UAC。拒绝提权时，已有索引仍可搜索，只是拿不到实时增量。
- 首次启动需要等索引建完（几十秒量级，取决于磁盘文件数），之后启动即可直接搜索。

手动安装/卸载服务（需要管理员终端）：

```powershell
rastflow.exe --install-service
rastflow.exe --uninstall-service
```

> 用 MSI 安装时不需要这步 —— 安装程序会自己注册并启动服务，卸载时也会自动停掉并注销。

## 打包安装

```bash
cargo packager --release
```

产物：`dist/rastflow_0.1.0_x64_zh-CN.msi`。首次执行会自动下载 WiX 3.11 工具链
（到 `%LOCALAPPDATA%\.cargo-packager\WixTools`，需联网），安装包本体与
索引服务的注册动作定义在 `packaging/rastflow-service.wxs`。

几点行为：

- **perMachine 安装**，默认装到 `C:\Program Files\rastflow`，安装过程需要过一次 UAC。
- 安装/修复时会自动`rastflow.exe --install-service`；卸载前会 `--uninstall-service`
  先停掉服务，否则进程占着 exe 删不掉。
- 卸载**不删** `%LOCALAPPDATA%\rastflow` 里的配置与索引快照，重装后可继续用。
- 已装过旧的可执行版升级时，服务会被停止 → 重建 → 重启，不需要手工干预。

## 功能实现

- [x] 应用搜索与启动
- [x] 全局快捷键设置
- [x] 文件搜索与自建索引（MFT + USN）

## 运行项目

```bash
cargo run
```

首次运行后，应用会进入托盘，可通过快捷键或托盘菜单唤出窗口

## 快捷操作

- `Alt + Space`：显示/隐藏启动器（默认，可在设置中修改）
- `Tab`：在应用启动器模式与文件搜索模式之间切换
- `Enter`：确认（启动应用，或打开选中的文件）
- `Esc`：退出文件搜索模式或隐藏窗口

## 文档

- [架构文档](./docs/architecture.md)：模块划分、线程模型、状态与持久化、平台集成、设计决策
- [文件搜索核心](./docs/search-engine.md)：索引布局、MFT 建库、USN 增量、快照格式、并发约束
