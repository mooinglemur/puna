//! Turning a room row into the Kubernetes objects that serve it.
//!
//! Pure functions over `puna-core` types: nothing here talks to a cluster, which is what lets the
//! whole lifecycle be tested against `FakeCluster`.

pub mod secret;
