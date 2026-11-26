//! Integrated terminal (T01/T02): PTY sessions, shell integration and directory follow.

mod follow;
mod keys;
mod osc;
mod session;
mod shell;
mod snapshot;

pub use follow::{FollowController, FollowMode, FollowOutput, PromptState};
pub use keys::{KeyMods, encode_key};
pub use osc::ShellEvent;
pub use session::{SCROLLBACK_LINES, Session, SessionConfig};
pub use shell::{Integration, ShellKind};
pub use snapshot::{CellStyle, SnapshotCell, TerminalSnapshot};
