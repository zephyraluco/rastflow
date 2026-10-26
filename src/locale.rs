use gpui::App;

use crate::settings::AppSettings;

/// 返回当前语言下的翻译字符串。
/// key 未命中时返回 key 本身，避免崩溃。
pub fn t(key: &str, cx: &App) -> &'static str {
    let lang = cx.global::<AppSettings>().language.clone();
    match lang.as_ref() {
        "en" => en(key),
        _ => zh(key),
    }
}

/// 建索引的进度文案：`正在扫描 C: 盘，已索引 1234 个文件`。
///
/// 数字与盘符插进句子里，语序随语言变化，所以单独成为一个函数，
/// 而不是让调用方去拼 [`t`] 返回的片段。
pub fn scanning_detail(disk: char, written: u64, cx: &App) -> String {
    let lang = cx.global::<AppSettings>().language.clone();
    match lang.as_ref() {
        "en" => format!("Scanning {disk}: · {written} files indexed"),
        _ => format!("正在扫描 {disk}: 盘，已索引 {written} 个文件"),
    }
}

/// 「发现新版本」提示框正文：两者版本号都要露出来，语序随语言变化。
pub fn update_available(new: &str, current: &str, cx: &App) -> String {
    let lang = cx.global::<AppSettings>().language.clone();
    match lang.as_ref() {
        "en" => format!(
            "Version {new} is available.\nYou are running {current}.\n\nDownload and install it now?"
        ),
        _ => format!(
            "发现新版本 {new}。\n当前版本 {current}。\n\n是否立即下载并安装？"
        ),
    }
}

/// 「更新已安装」提示框正文：新版已就位，重启后生效。
pub fn update_staged(version: &str, cx: &App) -> String {
    let lang = cx.global::<AppSettings>().language.clone();
    match lang.as_ref() {
        "en" => format!("Version {version} has been installed.\nIt takes effect after restarting rastflow."),
        _ => format!("新版本 {version} 已下载并安装。\n重启 rastflow 后生效。"),
    }
}

// ---------- 中文 ----------

fn zh(key: &str) -> &'static str {
    match key {
        // 启动器主界面
        "search.placeholder" => "搜索应用程序...",
        "search.empty"       => "没有找到匹配的应用程序",
        // 键帽由 Kbd 组件渲染，这里只放说明文字
        "hint.select"        => "选择",
        "hint.launch"        => "启动",
        "hint.close"         => "关闭",
        "settings.open"      => "设置",

        // 文件搜索
        "filesearch.mode"              => "文件搜索",
        "filesearch.index_badge"       => "本地索引",
        "filesearch.init"              => "正在初始化文件索引...",
        "filesearch.indexing"          => "正在建立文件索引",
        "filesearch.indexing.preparing" => "准备中...",
        "filesearch.indexing.hint"     => "索引只需建立一次，之后启动即可直接搜索",
        "filesearch.unavailable"       => "文件索引不可用",
        "filesearch.needs_admin"       => "读取 NTFS 主文件表需要管理员权限；安装一次索引服务即可永久免提权",
        "filesearch.elevate"           => "安装索引服务",
        "filesearch.retry"             => "重试",
        "filesearch.searching"         => "正在搜索本地索引...",
        "filesearch.no_results"        => "未找到匹配文件",
        "filesearch.column.name"       => "名称",
        "filesearch.column.path"       => "路径",
        "filesearch.column.size"       => "大小",
        "filesearch.column.modified"   => "修改时间",

        // 设置 - 外观
        "page.appearance"            => "外观",
        "group.theme_language"       => "主题与语言",
        "item.color_theme"           => "配色主题",
        "item.color_theme.desc"      => "选择应用程序的配色主题",
        "opt.theme.system"           => "跟随系统",
        "opt.theme.light"            => "浅色",
        "opt.theme.dark"             => "深色",
        "item.language"              => "界面语言",
        "item.language.desc"         => "选择界面显示语言",
        "opt.lang.zh"                => "简体中文",
        "opt.lang.en"                => "English",
        // 设置 - 行为
        "page.behavior"              => "行为",
        "group.startup"              => "启动",
        "item.auto_launch"           => "开机自动启动",
        "item.auto_launch.desc"      => "系统启动时自动运行程序启动器",
        // 设置 - 快捷键
        "page.hotkey"                => "快捷键",
        "group.global_hotkey"        => "全局快捷键",
        "item.show_hotkey"           => "唤出界面",
        "item.show_hotkey.desc"      => "按下此快捷键可随时从任意窗口唤出启动器界面",

        // 设置 - 自定义程序
        "page.custom_programs"       => "自定义程序",
        "custom.empty"               => "暂未添加自定义程序",
        "custom.add"                 => "添加程序",

        // 设置窗口标题
        "settings.title"             => "设置",

        // 检查更新（托盘菜单 → 原生提示框）
        "update.title"               => "检查更新",
        "update.up_to_date"          => "当前已是最新版本。",
        "update.failed"              => "检测更新失败",

        _ => "",
    }
}

// ---------- English ----------

fn en(key: &str) -> &'static str {
    match key {
        // Launcher
        "search.placeholder" => "Search apps...",
        "search.empty"       => "No matching apps found",
        // Keys are rendered by the Kbd component; only the caption lives here.
        "hint.select"        => "Select",
        "hint.launch"        => "Launch",
        "hint.close"         => "Close",
        "settings.open"      => "Settings",

        // File search
        "filesearch.mode"              => "File search",
        "filesearch.index_badge"       => "Local index",
        "filesearch.init"              => "Initializing file index...",
        "filesearch.indexing"          => "Building file index",
        "filesearch.indexing.preparing" => "Preparing...",
        "filesearch.indexing.hint"     => "The index is built once; later launches search instantly",
        "filesearch.unavailable"       => "File index unavailable",
        "filesearch.needs_admin"       => "Reading the NTFS master file table requires administrator rights; install the index service once to avoid future prompts",
        "filesearch.elevate"           => "Install index service",
        "filesearch.retry"             => "Retry",
        "filesearch.searching"         => "Searching local index...",
        "filesearch.no_results"        => "No matching files found",
        "filesearch.column.name"       => "Name",
        "filesearch.column.path"       => "Path",
        "filesearch.column.size"       => "Size",
        "filesearch.column.modified"   => "Modified",

        // Settings - Appearance
        "page.appearance"            => "Appearance",
        "group.theme_language"       => "Theme & Language",
        "item.color_theme"           => "Color Theme",
        "item.color_theme.desc"      => "Choose the color theme for the application",
        "opt.theme.system"           => "Follow System",
        "opt.theme.light"            => "Light",
        "opt.theme.dark"             => "Dark",
        "item.language"              => "Language",
        "item.language.desc"         => "Select the display language",
        "opt.lang.zh"                => "简体中文",
        "opt.lang.en"                => "English",
        // Settings - Behavior
        "page.behavior"              => "Behavior",
        "group.startup"              => "Startup",
        "item.auto_launch"           => "Launch at Login",
        "item.auto_launch.desc"      => "Automatically start the launcher when the system boots",
        // Settings - Hotkey
        "page.hotkey"                => "Hotkeys",
        "group.global_hotkey"        => "Global Hotkeys",
        "item.show_hotkey"           => "Show Launcher",
        "item.show_hotkey.desc"      => "Press this hotkey to open the launcher from anywhere",

        // Settings - Custom Programs
        "page.custom_programs"       => "Custom Apps",
        "custom.empty"               => "No custom apps added yet",
        "custom.add"                 => "Add App",

        // Settings window title
        "settings.title"             => "Settings",

        // Check for updates (tray menu → native message box)
        "update.title"               => "Check for Updates",
        "update.up_to_date"          => "You are running the latest version.",
        "update.failed"              => "Failed to check for updates",

        _ => "",
    }
}
