use gpui::{AppContext, Entity};
use gpui_updater::{EngineConfig, GitHubSource, Updater, Version};

pub type UpdateEntity = Entity<Updater>;

pub fn create(cx: &mut gpui::App) -> UpdateEntity {
    let source = GitHubSource::new("zephyraluco", "rastflow")
        .asset_contains(".msi");
    let config = EngineConfig::new(
        Version::parse(env!("CARGO_PKG_VERSION")).expect("Cargo package version must be valid semver"),
    );
    cx.new(|cx| Updater::new(source, config, cx))
}

