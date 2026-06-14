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

// ---------- 中文 ----------

fn zh(key: &str) -> &'static str {
    match key {
        // 启动器主界面
        "search.placeholder" => "搜索应用程序...",
        "search.empty"       => "没有找到匹配的应用程序",
        "hint.select"        => "↑↓ 选择",
        "hint.launch"        => "↵ 启动",
        "hint.close"         => "Esc 关闭",

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

        _ => "",
    }
}

// ---------- English ----------

fn en(key: &str) -> &'static str {
    match key {
        // Launcher
        "search.placeholder" => "Search apps...",
        "search.empty"       => "No matching apps found",
        "hint.select"        => "↑↓ Select",
        "hint.launch"        => "↵ Launch",
        "hint.close"         => "Esc Close",

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

        _ => "",
    }
}
