//! Generation artifacts: the zip an Archipelago generation produces, and what Puna reads out of it.

pub mod ingest;
pub mod names;
pub mod patch;
pub mod storage;

pub use ingest::{
    GenerationMeta, IngestError, MAX_MEMBER_BYTES, MAX_MULTIDATA_BYTES, MAX_MULTIDATA_RATIO,
    MAX_PRECOLLECTED_ITEMS, SlotEntry, SlotKind, inspect, load_refusal, parse_multidata,
    seed_refusal,
};
pub use names::{NameTables, from_seed as seed_names};
pub use patch::{Credential, PatchError, embed_server};
pub use storage::{GenerationPaths, Promotion, StorageError, promote};
