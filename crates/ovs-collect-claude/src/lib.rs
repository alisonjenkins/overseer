//! Reads Claude Code's on-disk session store and turns it into `ovs` events.

mod record;
mod registry;

pub use record::{FileState, LineContext, LineError, parse_line};
pub use registry::{LiveEntry, RegistryError, diff_registry, pid_alive, read_registry};
