slint::include_modules!();

pub mod actions;
mod app;
pub mod compose;
pub mod format;
pub mod image_cache;
pub mod list_model;
pub mod memory;
mod notify;
pub mod sync_bridge;
mod tray;

pub use app::run;
