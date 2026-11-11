//! The agent terminal, ported from syro (crates/core/src/term.rs + crates/app/src/pane/terminal.rs).

mod session;
mod view;

pub use session::{TermSession, TermSize, TermSpec};
pub use view::{TerminalEvent, TerminalView};
