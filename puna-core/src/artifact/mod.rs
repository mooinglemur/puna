//! Generation artifacts: the zip an Archipelago generation produces, and what Puna reads out of it.

pub mod ingest;

pub use ingest::{GenerationMeta, IngestError, SlotEntry, SlotKind, inspect};
