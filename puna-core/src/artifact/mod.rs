//! Generation artifacts: the zip an Archipelago generation produces, and what Puna reads out of it.

pub mod ingest;
pub mod storage;

pub use ingest::{GenerationMeta, IngestError, SlotEntry, SlotKind, inspect};
pub use storage::{GenerationPaths, Promotion, StorageError, promote};
