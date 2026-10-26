mod global;
mod pages;
mod persist;
mod view;

pub use global::AppSettings;
pub use persist::{load_settings, save_settings, flush};
pub use view::SettingsView;
