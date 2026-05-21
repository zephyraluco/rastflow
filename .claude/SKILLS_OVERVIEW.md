# Skills 作用概览

## 文档与展示类

| Skill | 作用 |
|---|---|
| `github-pull-request-description` | 编写 GitHub PR 描述（150词以内），含新特性、Bug修复，若有 Breaking Changes 则单独列出并用 diff 代码块说明新旧用法 |

---

## GPUI 框架核心机制类

| Skill | 作用 |
|---|---|
| `gpui-action` | 定义 Action 和键盘快捷键，使用 `actions!` 宏、`cx.bind_keys()`、`.on_action()` 实现声明式键盘交互 |
| `gpui-async` | 处理异步操作和后台任务，区分前台任务（`cx.spawn`，可更新 UI）和后台任务（`cx.background_spawn`，CPU密集型）的使用模式 |
| `gpui-context` | 管理不同类型的上下文（`App`、`Window`、`Context<T>`、`AsyncApp`），指导在不同场景下正确使用对应的上下文类型 |
| `gpui-element` | 使用低层级 `Element` trait 实现自定义元素，提供对 layout、prepaint、paint 三个渲染阶段的精细控制，适合高性能复杂组件 |
| `gpui-entity` | 管理 Entity 状态，包括 `Entity<T>` 强引用、`WeakEntity<T>` 弱引用、跨组件状态共享和响应式更新模式 |
| `gpui-event` | 实现事件系统，包括自定义事件的定义与 emit、组件间的 observation（状态变更监听）和 subscription（事件订阅） |
| `gpui-focus-handle` | 管理焦点与键盘导航，使用 `FocusHandle` 实现 Tab/Shift-Tab 导航、焦点追踪及 `on_focus`/`on_blur` 事件处理 |
| `gpui-global` | 实现全局状态管理，通过 `Global` trait 定义跨整个 App 可访问的共享数据（如主题、配置等） |
| `gpui-layout-and-style` | 布局与样式系统，提供类 CSS 的 Flexbox 布局、`px()`/`rems()` 等尺寸单位、颜色、圆角、阴影等样式链式调用 |

---

## 组件开发规范类

| Skill | 作用 |
|---|---|
| `gpui-style-guide` | 基于 gpui-component 代码库的编码风格指南，规范组件结构、trait 实现、命名约定和 API 模式，确保代码一致性 |
| `gpui-test` | 编写 GPUI 测试，使用 `#[gpui::test]` 属性、`TestAppContext`（基础测试）和 `VisualTestContext`（窗口/渲染测试）进行确定性测试 |
