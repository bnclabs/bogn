//! Package **Rdms** provide a collection of algorithms for indexing
//! data either _in memory_ or _in disk_ or _both_. Rdms indexes are
//! optimized for document databases and bigdata.
//!
//! Features:
//!
//! * Provide CRUD API, Create, Read, Update, Delete.
//! * Parameterized over a key-type (**K**) and a value-type (**V**).
//! * Parameterized over Memory-index and Disk-index.
//! * Memory index suitable for daaa ingestion and caching frequently
//!   accessed key.
//! * Concurrent reads, with single concurrent write.
//! * Concurrent writes (_Work in progress_).
//! * Version control, centralised.
//! * Version control, distributed (_Work in progress_).
//! * Log Structured Merge for multi-level indexing.
//!
//! **Seqno**, each index will carry a sequence-number as the count
//! of mutations ingested by the index. For every successful mutation,
//! the sequence-number will be incremented and corresponding entry
//! shall be tagged with that sequence-number.
//!
//! **Log-Structured-Merge [LSM]**, is a common technique used in managing
//! heterogenous data-structures that are transparent to the index. In
//! case of Rdms, in-memory structures are different from on-disk
//! structures, and LSM technique is used to maintain consistency between
//! them.
//!
//! **CAS**, a.k.a compare-and-set, can be specified by applications
//! that need consistency gaurantees for a single index-entry. In API
//! context CAS is same as _sequence-number_.
//!
//! **Piece-wise full-table scanning**, in many cases long running scans
//! are bad for indexes using locks and/or multi-version-concurrency-control.
//! And there _will_ be situations where a full table scan is required
//! on such index while handling live read/write operations. Piece-wise
//! scanning can help in those situations, provided the index is configured
//! for LSM.
//!
//!
//! [LSM]: https://en.wikipedia.org/wiki/Log-structured_merge-tree
//!

#![feature(bind_by_move_pattern_guards)]
#![feature(drain_filter)]
#![feature(maybe_uninit_ref)]

// core modules
pub mod core;
pub mod error;
mod panic;
pub mod spinlock;
pub mod sync;
mod sync_writer;
pub mod types;
mod util;
mod vlog;

// support modules
pub mod lsm;
pub mod scans;
pub mod wal;

// mem index
pub mod llrb;
mod llrb_node;
pub mod mvcc;
// disk index
// pub mod dgm; TODO
pub mod nodisk;
pub mod robt;
mod robt_entry;
mod robt_index;

// bloom filters.
pub mod croaring;
pub mod nobitmap;

pub mod rdms;
pub use crate::rdms::Rdms;
