//! SQLite persistence for the overseer task orchestrator.

mod error;
mod store;

pub use error::StoreError;
pub use store::Store;
