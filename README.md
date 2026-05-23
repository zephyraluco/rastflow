# rastflow

`rastflow` 是一个基于 Rust + GPUI 的桌面快速启动器（当前以 Windows 使用场景为主）

它提供类似 Spotlight/Launcher 的搜索启动体验，并集成了托盘常驻、全局快捷键唤出以及 AI 对话模式

## 主要功能

- 程序搜索与快速启动
- 系统托盘常驻（显示/隐藏、退出）
- 全局快捷键唤出（默认 `Alt + Space`）
- 启动频次排序（常用应用优先）
- 内置 AI 对话模式（可配置 API Key、Base URL、Model）
- 基础设置持久化（主题、语言、热键、AI 配置等）

## 快速开始

### 1. 环境要求

- Rust 工具链（建议最新 stable）
- Windows 桌面环境

### 2. 运行项目

```bash
cargo run
```

首次运行后，应用会进入托盘，可通过快捷键或托盘菜单唤出窗口。

## 快捷操作

- `Alt + Space`：显示/隐藏启动器（默认，可在设置中修改）
- `Tab`：在启动器模式与 AI 对话模式之间切换
- `Enter`：确认（启动应用或发送 AI 消息）
- `Esc`：退出 AI 模式或隐藏窗口

## AI 配置说明

可在设置中填写以下参数：

- API Key
- Base URL（留空时使用 Anthropic 官方地址）
- Model（默认 `claude-opus-4-5`）

也支持通过环境变量提供 Key：

- `ANTHROPIC_API_KEY`

