//! GPUI 位图缓存，容量和重试策略由纯逻辑缓存实现。
pub type IconCache = crate::cache::IconCache<std::sync::Arc<gpui::RenderImage>>;
