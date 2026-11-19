//! pSTART Host.
//!
//! GDI overlay, native EDIT, hotkey and single-instance lifecycle are
//! implemented in M6. No UI capability is claimed until it is tested.

#![forbid(unsafe_code)]

pub mod action;
pub mod apps;
pub mod calculator;
#[cfg(windows)]
pub mod client;
pub mod expression;
pub mod history;
pub mod hotkey;
pub mod open;
pub mod paths;
pub mod query;
pub mod selection;
pub mod skin;

/// Default global hotkey text shown in settings.
pub const DEFAULT_HOTKEY: &str = "Double Ctrl";

pub use action::action_allowed;
pub use apps::{
    is_app_shortcut, item_for_path, merge_results, search_apps, start_menu_roots, AppCatalog,
};
pub use calculator::Calculator;
#[cfg(windows)]
pub use client::{Client, ClientError, ClientResponse, StatusResponse};
pub use expression::{
    evaluate as evaluate_expression, evaluate_decimal, format_decimal, ExpressionError,
};
pub use history::{HistoryEntry, HistoryError, HistoryStore, MAX_HISTORY_RESULTS};
pub use open::{validate_open_path, validate_plugin_action_path, OpenError};
pub use paths::retain_existing_paths;
pub use query::QueryScheduler;
pub use skin::{Skin, SkinError};
