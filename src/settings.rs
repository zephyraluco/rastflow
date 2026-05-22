mod global;
mod pages;
mod persist;
mod view;

pub use global::AppSettings;
pub use persist::{load_settings, save_settings};
pub use view::SettingsView;
