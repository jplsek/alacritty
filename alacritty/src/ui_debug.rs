//! Opt-in UI automation over the existing local IPC socket.
//!
//! Actions run on the UI thread against real windows, PTYs, and the renderer. This module is
//! excluded from release builds, even when the Cargo feature is enabled.

use clap::{Args, Subcommand};
use serde::{Deserialize, Serialize};

#[derive(Args, Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
pub struct UiDebugOptions {
    /// Native window ID. May be omitted only when exactly one window exists.
    #[clap(long)]
    pub window_id: Option<u64>,

    #[clap(subcommand)]
    pub action: UiDebugAction,
}

#[derive(Subcommand, Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
pub enum UiDebugAction {
    /// Read geometry, tab state, visible text, and completed frame count.
    State,
    /// Create and select a tab using the normal tab action.
    CreateTab,
    /// Select a tab by its zero-based index.
    SelectTab { index: usize },
    /// Request a native resize in physical pixels. Poll state for completion.
    Resize { width: u32, height: u32 },
    /// Feed a complete UTF-8/ANSI sequence into the active terminal, as if received from its PTY.
    Output { data: String },
    /// Process queued events and render one frame, including partial damage.
    Redraw,
}

#[derive(Serialize, Deserialize, Debug)]
pub struct WindowState {
    pub window_id: u64,
    pub active_tab: usize,
    pub native_width: u32,
    pub native_height: u32,
    pub surface_width: u32,
    pub surface_height: u32,
    pub columns: usize,
    pub screen_lines: usize,
    pub rendered_frames: u64,
    pub occluded: bool,
    pub tabs: Vec<TabState>,
    pub text: Vec<String>,
}

#[derive(Serialize, Deserialize, Debug)]
pub struct TabState {
    pub id: u64,
    pub title: String,
    pub columns: usize,
    pub screen_lines: usize,
    pub cursor_line: i32,
    pub cursor_column: usize,
    pub queued_events: usize,
}
