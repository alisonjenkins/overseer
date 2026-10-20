//! Reads Claude Code's on-disk session store and turns it into `ovs` events.

mod record;

pub use record::{FileState, LineContext, LineError, parse_line};
