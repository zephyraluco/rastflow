# rastflow

`rastflow` 是一个基于 Rust + GPUI 的 Windows 桌面快速启动器

## 主要功能

- 程序搜索与快速启动
- 系统托盘常驻（显示/隐藏、退出）
- 全局快捷键唤出（默认 `Alt + Space`）
- 自定义程序管理
- Everything 模式（通过 `Tab` 切换）
- 基础设置持久化（主题、语言、开机自启、热键等）

## 功能实现

- [x] 应用搜索与启动
- [x] 全局快捷键设置
- [ ] Everything 集成（进行中）

## 运行项目

```bash
cargo run
```

首次运行后，应用会进入托盘，可通过快捷键或托盘菜单唤出窗口

## 快捷操作

- `Alt + Space`：显示/隐藏启动器（默认，可在设置中修改）
- `Tab`：在应用启动器模式与 Everything 模式之间切换
- `Enter`：确认（启动应用，或在 Everything 中搜索）
- `Esc`：退出 Everything 模式或隐藏窗口
