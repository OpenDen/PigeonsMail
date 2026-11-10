slint::include_modules!();

pub mod actions;
mod app;
pub mod format;
pub mod list_model;
pub mod sync_bridge;

pub use app::run;
