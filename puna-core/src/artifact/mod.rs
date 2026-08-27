//! Generation artifacts: the zip an Archipelago generation produces, and what Puna reads out of it.

pub mod ingest;
pub mod names;
pub mod patch;
pub mod storage;

pub use ingest::{
    GenerationMeta, IngestError, SlotEntry, SlotKind, inspect, load_refusal, seed_refusal,
};
pub use names::{NameTables, from_seed as seed_names};
pub use patch::{Credential, PatchError, embed_server};
pub use storage::{GenerationPaths, Promotion, StorageError, promote};
