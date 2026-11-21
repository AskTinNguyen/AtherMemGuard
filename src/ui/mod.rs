//! Custom-drawn UI: theme, canvas, widget kit, window layer, toasts and the settings window.
//! See docs/UI.md. All Win32 drawing lives here.

pub mod canvas;
pub mod geom;
pub mod theme;
pub mod commands;
pub mod kit;
pub mod settings_store;
pub mod status_source;

pub use commands::Command;
pub mod dialogs;
pub mod window;
pub mod toast;
pub mod settings_model;
pub mod settings;
pub mod ui_state;
#[cfg(any(test, feature = "ui-snapshots"))]
pub mod snapshots;
pub mod flyout;
pub mod real_store;
pub mod setup;
pub mod history;
pub mod doctor;
