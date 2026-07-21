//! Thin re-export so binary-crate code can `use crate::events::AppEvent`
//! without every call site needing to know it lives in `riptune-types`.
//! The type itself is defined once, in `riptune-types`, and shared with
//! `riptune-mpris` and `riptune-niri`.

// Not yet consumed — app.rs doesn't have an event loop wired up until the TUI
// is implemented. Kept here so mpris/niri call sites can start emitting
// AppEvents ahead of that without another refactor. Remove this allow once
// app::run() dispatches on it.
#[allow(unused_imports)]
pub use riptune_types::AppEvent;
