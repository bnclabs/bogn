//! Read Only BTree for disk based indexes.
//!
//! ROBT instances shall have an index file and an optional value-log-file,
//! refer to [Config] for more information.
//!
//! **Index-file format**:
//!
//! ```text
//! *------------------------------------------* SeekFrom::End(0)
//! |                marker-length             |
//! *------------------------------------------* SeekFrom::End(-8)
//! |                stats-length              |
//! *------------------------------------------* SeekFrom::End(-16)
//! |             app-metadata-length          |
//! *------------------------------------------* SeekFrom::End(-24)
//! |                  root-fpos               |
//! *------------------------------------------* SeekFrom::MetaBlock
//! *                 meta-blocks              *
//! *                    ...                   *
//! *------------------------------------------*
//! *                btree-blocks              *
//! *                    ...                   *
//! *                    ...                   *
//! *------------------------------------------* 0
//! ```
//!
//! Tip of the index file contain 32-byte header providing
//! following details:
//! * Index statistics
//! * Application metadata
//! * File-position for btree's root-block.
//!
//! Total length of `metadata-blocks` can be computed based on
//! `marker-length`, `stats-length`, `app-metadata-length`.
//!
//! [Config]: crate::robt::Config
//!
use lazy_static::lazy_static;

use std::{
    borrow::Borrow,
    cmp,
    convert::TryInto,
    ffi, fmt,
    fmt::Display,
    fs,
    io::Write,
    marker, mem,
    ops::{Bound, RangeBounds},
    path, result,
    str::FromStr,
    sync::{self, atomic::AtomicPtr, atomic::Ordering, mpsc, Arc},
    thread, time,
};

use crate::core::{Diff, Entry, Footprint, Result, Serialize};
use crate::core::{Index, IndexIter, Reader, Writer};
use crate::error::Error;
use crate::jsondata::{Json, Property};
use crate::util;

use crate::robt_entry::MEntry;
use crate::robt_index::{MBlock, ZBlock};

// TODO: make dir, file, path into OsString and OsStr.

include!("robt_marker.rs");

struct Levels<K, V>(AtomicPtr<Arc<Vec<Snapshot<K, V>>>>)
where
    K: Clone + Ord + Serialize,
    V: Clone + Diff + Serialize,
    <V as Diff>::D: Serialize;

impl<K, V> Levels<K, V>
where
    K: Clone + Ord + Serialize,
    V: Clone + Diff + Serialize,
    <V as Diff>::D: Serialize,
{
    fn new() -> Levels<K, V> {
        Levels(AtomicPtr::new(Box::leak(Box::new(Arc::new(vec![])))))
    }

    fn get_snapshots(&self) -> Arc<Vec<Snapshot<K, V>>> {
        unsafe { Arc::clone(self.0.load(Ordering::Relaxed).as_ref().unwrap()) }
    }

    fn compare_swap_snapshots(&self, new_snapshots: Vec<Snapshot<K, V>>) {
        let _olds = unsafe { Box::from_raw(self.0.load(Ordering::Relaxed)) };
        let new_snapshots = Box::leak(Box::new(Arc::new(new_snapshots)));
        self.0.store(new_snapshots, Ordering::Relaxed);
    }
}

pub(crate) struct Robt<K, V, M>
where
    K: 'static + Sync + Send + Clone + Ord + Serialize + Footprint,
    V: 'static + Sync + Send + Clone + Diff + Serialize + Footprint,
    <V as Diff>::D: Serialize,
    M: 'static + Sync + Send + Index<K, V>,
{
    config: Config,
    mem_ratio: f64,
    disk_ratio: f64,
    levels: Levels<K, V>,
    todisk: MemToDisk<K, V, M>,      // encapsulates a thread
    tocompact: DiskCompact<K, V, M>, // encapsulates a thread
}

// new instance of multi-level Robt indexes.
impl<K, V, M> Robt<K, V, M>
where
    K: 'static + Sync + Send + Clone + Ord + Serialize + Footprint,
    V: 'static + Sync + Send + Clone + Diff + Serialize + Footprint,
    <V as Diff>::D: Serialize,
    M: 'static + Sync + Send + Index<K, V>,
{
    const MEM_RATIO: f64 = 0.2;
    const DISK_RATIO: f64 = 0.5;

    pub(crate) fn new(config: Config) -> Robt<K, V, M> {
        Robt {
            config: config.clone(),
            mem_ratio: Self::MEM_RATIO,
            disk_ratio: Self::DISK_RATIO,
            levels: Levels::new(),
            todisk: MemToDisk::new(config.clone()),
            tocompact: DiskCompact::new(config.clone()),
        }
    }

    pub(crate) fn set_mem_ratio(mut self, ratio: f64) -> Robt<K, V, M> {
        self.mem_ratio = ratio;
        self
    }

    pub(crate) fn set_disk_ratio(mut self, ratio: f64) -> Robt<K, V, M> {
        self.disk_ratio = ratio;
        self
    }
}

// add new levels.
impl<K, V, M> Robt<K, V, M>
where
    K: 'static + Sync + Send + Clone + Ord + Serialize + Footprint,
    V: 'static + Sync + Send + Clone + Diff + Serialize + Footprint,
    <V as Diff>::D: Serialize,
    M: 'static + Sync + Send + Index<K, V>,
{
    pub(crate) fn flush_to_disk(
        &mut self,
        index: Arc<M>, // full table scan over mem-index
        app_meta: Vec<u8>,
    ) -> Result<()> {
        let _resp = self.todisk.send(Request::MemFlush {
            index,
            app_meta,
            phantom_key: marker::PhantomData,
            phantom_val: marker::PhantomData,
        })?;
        Ok(())
    }
}

enum Request<K, V, M>
where
    K: 'static + Sync + Send + Clone + Ord + Serialize + Footprint,
    V: 'static + Sync + Send + Clone + Diff + Serialize + Footprint,
    <V as Diff>::D: Serialize,
    M: 'static + Sync + Send + Index<K, V>,
{
    MemFlush {
        index: Arc<M>,
        app_meta: Vec<u8>,
        phantom_key: marker::PhantomData<K>,
        phantom_val: marker::PhantomData<V>,
    },
}

enum Response {
    Ok,
}

struct MemToDisk<K, V, M>
where
    K: 'static + Sync + Send + Clone + Ord + Serialize + Footprint,
    V: 'static + Sync + Send + Clone + Diff + Serialize + Footprint,
    <V as Diff>::D: Serialize,
    M: 'static + Sync + Send + Index<K, V>,
{
    config: Config,
    thread: thread::JoinHandle<Result<()>>,
    tx: mpsc::SyncSender<(Request<K, V, M>, mpsc::SyncSender<Response>)>,
}

impl<K, V, M> MemToDisk<K, V, M>
where
    K: 'static + Sync + Send + Clone + Ord + Serialize + Footprint,
    V: 'static + Sync + Send + Clone + Diff + Serialize + Footprint,
    <V as Diff>::D: Serialize,
    M: 'static + Sync + Send + Index<K, V>,
{
    fn new(config: Config) -> MemToDisk<K, V, M> {
        let (tx, rx) = mpsc::sync_channel(1);
        let conf = config.clone();
        let thread = thread::spawn(move || thread_mem_to_disk(conf, rx));
        MemToDisk { config, thread, tx }
    }

    fn send(&mut self, req: Request<K, V, M>) -> Result<Response> {
        let (tx, rx) = mpsc::sync_channel(0);
        self.tx.send((req, tx))?;
        Ok(rx.recv()?)
    }

    fn close_wait(self) -> Result<()> {
        mem::drop(self.tx);
        match self.thread.join() {
            Ok(res) => res,
            Err(err) => match err.downcast_ref::<String>() {
                Some(msg) => Err(Error::ThreadFail(msg.to_string())),
                None => Err(Error::ThreadFail("unknown error".to_string())),
            },
        }
    }
}

fn thread_mem_to_disk<K, V, M>(
    _config: Config,
    _rx: mpsc::Receiver<(Request<K, V, M>, mpsc::SyncSender<Response>)>,
) -> Result<()>
where
    K: 'static + Sync + Send + Clone + Ord + Serialize + Footprint,
    V: 'static + Sync + Send + Clone + Diff + Serialize + Footprint,
    <V as Diff>::D: Serialize,
    M: 'static + Sync + Send + Index<K, V>,
{
    // TBD
    Ok(())
}

struct DiskCompact<K, V, M>
where
    K: 'static + Sync + Send + Clone + Ord + Serialize + Footprint,
    V: 'static + Sync + Send + Clone + Diff + Serialize + Footprint,
    <V as Diff>::D: Serialize,
    M: 'static + Sync + Send + Index<K, V>,
{
    config: Config,
    thread: thread::JoinHandle<Result<()>>,
    tx: mpsc::SyncSender<(Request<K, V, M>, mpsc::SyncSender<Response>)>,
}

impl<K, V, M> DiskCompact<K, V, M>
where
    K: 'static + Sync + Send + Clone + Ord + Serialize + Footprint,
    V: 'static + Sync + Send + Clone + Diff + Serialize + Footprint,
    <V as Diff>::D: Serialize,
    M: 'static + Sync + Send + Index<K, V>,
{
    fn new(config: Config) -> DiskCompact<K, V, M> {
        let (tx, rx) = mpsc::sync_channel(1);
        let conf = config.clone();
        let thread = thread::spawn(move || thread_disk_compact(conf, rx));
        DiskCompact { config, thread, tx }
    }

    fn send(&mut self, req: Request<K, V, M>) -> Result<Response> {
        let (tx, rx) = mpsc::sync_channel(0);
        self.tx.send((req, tx))?;
        Ok(rx.recv()?)
    }

    fn close_wait(self) -> Result<()> {
        mem::drop(self.tx);
        match self.thread.join() {
            Ok(res) => res,
            Err(err) => match err.downcast_ref::<String>() {
                Some(msg) => Err(Error::ThreadFail(msg.to_string())),
                None => Err(Error::ThreadFail("unknown error".to_string())),
            },
        }
    }
}

fn thread_disk_compact<K, V, M>(
    _config: Config,
    _rx: mpsc::Receiver<(Request<K, V, M>, mpsc::SyncSender<Response>)>,
) -> Result<()>
where
    K: 'static + Sync + Send + Clone + Ord + Serialize + Footprint,
    V: 'static + Sync + Send + Clone + Diff + Serialize + Footprint,
    <V as Diff>::D: Serialize,
    M: 'static + Sync + Send + Index<K, V>,
{
    // TBD
    Ok(())
}

/// Configuration options for Read Only BTree.
#[derive(Clone)]
pub struct Config {
    /// Leaf block size in btree index.
    pub z_blocksize: usize,
    /// Intemediate block size in btree index.
    pub m_blocksize: usize,
    /// If deltas are indexed and/or value to be stored in separate log file.
    pub v_blocksize: usize,
    /// Tombstone purge. For LSM based index older entries can quickly bloat
    /// system. To avoid this, it is a good idea to purge older versions of
    /// an entry that are seen by all participating entities. When configured
    /// with `Some(seqno)`, all iterated entries/versions whose seqno is ``<=``
    /// purge seqno shall be removed totally from the index.
    pub tomb_purge: Option<u64>,
    /// Include delta as part of entry. Note that delta values are always
    /// stored in separate value-log file.
    pub delta_ok: bool,
    /// Optional name for value log file. If not supplied, but `delta_ok` or
    /// `value_in_vlog` is true, then value log file name will be computed
    /// based on configuration`name` and `dir`.
    pub vlog_file: Option<ffi::OsString>,
    /// If true, then value shall be persisted in value log file. Otherwise
    /// value shall be saved in the index' leaf node.
    pub value_in_vlog: bool,
    /// Flush queue size.
    pub flush_queue_size: usize,
}

impl Default for Config {
    /// New configuration with default parameters:
    ///
    /// * With ZBLOCKSIZE, MBLOCKSIZE, VBLOCKSIZE.
    /// * Values are stored in the leaf node.
    /// * LSM entries are preserved.
    /// * Deltas are persisted in default value-log-file.
    /// * Main index is persisted in default index-file.
    fn default() -> Config {
        Config {
            z_blocksize: Self::ZBLOCKSIZE,
            v_blocksize: Self::VBLOCKSIZE,
            m_blocksize: Self::MBLOCKSIZE,
            tomb_purge: Default::default(),
            delta_ok: true,
            vlog_file: Default::default(),
            value_in_vlog: false,
            flush_queue_size: Self::FLUSH_QUEUE_SIZE,
        }
    }
}

impl From<Stats> for Config {
    fn from(stats: Stats) -> Config {
        Config {
            z_blocksize: stats.z_blocksize,
            m_blocksize: stats.m_blocksize,
            v_blocksize: stats.v_blocksize,
            tomb_purge: Default::default(),
            delta_ok: stats.delta_ok,
            vlog_file: stats.vlog_file,
            value_in_vlog: stats.value_in_vlog,
            flush_queue_size: Self::FLUSH_QUEUE_SIZE,
        }
    }
}

impl Config {
    pub const ZBLOCKSIZE: usize = 4 * 1024; // 4KB leaf node
    pub const VBLOCKSIZE: usize = 4 * 1024; // ~ 4KB of blobs.
    pub const MBLOCKSIZE: usize = 4 * 1024; // 4KB intermediate node
    const MARKER_BLOCK_SIZE: usize = 1024 * 4;
    const FLUSH_QUEUE_SIZE: usize = 64;

    /// Configure differt set of block size for leaf-node, intermediate-node.
    pub fn set_blocksize(&mut self, z: usize, v: usize, m: usize) -> &mut Self {
        self.z_blocksize = z;
        self.v_blocksize = v;
        self.m_blocksize = m;
        self
    }

    /// Enable tombstone purge. Deltas and values with sequence number less
    /// than `before` shall be purged.
    pub fn set_tombstone_purge(&mut self, before: u64) -> &mut Self {
        self.tomb_purge = Some(before);
        self
    }

    /// Enable delta persistence, and configure value-log-file. To disable
    /// delta persistance, pass `vlog_file` as None.
    pub fn set_delta(&mut self, vlog_file: Option<ffi::OsString>) -> &mut Self {
        match vlog_file {
            Some(vlog_file) => {
                self.delta_ok = true;
                self.vlog_file = Some(vlog_file);
            }
            None => {
                self.delta_ok = false;
            }
        }
        self
    }

    /// Persist values in a separate file, called value-log file. To persist
    /// values along with leaf node, pass `vlog_file` as None.
    pub fn set_value_log(&mut self, file: Option<ffi::OsString>) -> &mut Self {
        match file {
            Some(vlog_file) => {
                self.value_in_vlog = true;
                self.vlog_file = Some(vlog_file);
            }
            None => {
                self.value_in_vlog = false;
            }
        }
        self
    }

    /// Set flush queue size, increasing the queue size will improve batch
    /// flushing.
    pub fn set_flush_queue_size(&mut self, size: usize) -> &mut Self {
        self.flush_queue_size = size;
        self
    }
}

impl Config {
    pub(crate) fn stitch_index_file(dir: &str, name: &str) -> ffi::OsString {
        let mut index_file = path::PathBuf::from(dir);
        index_file.push(format!("robt-{}.indx", name));
        let index_file: &ffi::OsStr = index_file.as_ref();
        index_file.to_os_string()
    }

    pub(crate) fn stitch_vlog_file(dir: &str, name: &str) -> ffi::OsString {
        let mut vlog_file = path::PathBuf::from(dir);
        vlog_file.push(format!("robt-{}.vlog", name));
        let vlog_file: &ffi::OsStr = vlog_file.as_ref();
        vlog_file.to_os_string()
    }

    pub(crate) fn compute_root_block(n: usize) -> usize {
        if (n % Config::MARKER_BLOCK_SIZE) == 0 {
            n
        } else {
            ((n / Config::MARKER_BLOCK_SIZE) + 1) * Config::MARKER_BLOCK_SIZE
        }
    }

    /// Return the index file under configured directory.
    pub fn to_index_file(&self, dir: &str, name: &str) -> ffi::OsString {
        Self::stitch_index_file(&dir, &name)
    }

    /// Return the value-log file, if enabled, under configured directory.
    pub fn to_value_log(&self, dir: &str, name: &str) -> Option<ffi::OsString> {
        match &self.vlog_file {
            Some(file) => Some(file.clone()),
            None => Some(Self::stitch_vlog_file(&dir, &name)),
        }
    }
}

/// Enumerated meta types stored in [Robt] index.
///
/// [Robt] index is a full-packed immutable [Btree] index. To interpret
/// the index a list of meta items are appended to the tip
/// of index-file.
///
/// [Robt]: crate::robt::Robt
/// [Btree]: https://en.wikipedia.org/wiki/B-tree
pub enum MetaItem {
    /// A Unique marker that confirms that index file is valid.
    Marker(Vec<u8>), // tip of the file.
    /// Contains index-statistics along with configuration values.
    Stats(String),
    /// Application supplied metadata, typically serialized and opaque
    /// to [Bogn].
    ///
    /// [Bogn]: crate::Bogn
    AppMetadata(Vec<u8>),
    /// File-position where the root block for the Btree starts.
    Root(u64),
}

// returns bytes appended to file.
pub(crate) fn write_meta_items(
    file: ffi::OsString,
    items: Vec<MetaItem>, // list of meta items, starting from Marker
) -> Result<u64> {
    let p = path::Path::new(&file);
    let mut opts = fs::OpenOptions::new();
    let mut fd = opts.append(true).open(p)?;

    let (mut hdr, mut block) = (vec![], vec![]);
    hdr.resize(32, 0);

    for (i, item) in items.into_iter().enumerate() {
        match (i, item) {
            (0, MetaItem::Root(fpos)) => {
                hdr[0..8].copy_from_slice(&fpos.to_be_bytes());
            }
            (1, MetaItem::AppMetadata(md)) => {
                hdr[8..16].copy_from_slice(&(md.len() as u64).to_be_bytes());
                block.extend_from_slice(&md);
            }
            (2, MetaItem::Stats(s)) => {
                hdr[16..24].copy_from_slice(&(s.len() as u64).to_be_bytes());
                block.extend_from_slice(s.as_bytes());
            }
            (3, MetaItem::Marker(data)) => {
                hdr[24..32].copy_from_slice(&(data.len() as u64).to_be_bytes());
                block.extend_from_slice(&data);
            }
            (i, _) => panic!("unreachable arm at {}", i),
        }
    }
    block.extend_from_slice(&hdr[..]);

    // flush / append into file.
    let n = Config::compute_root_block(block.len());
    let (shift, m) = (n - block.len(), block.len());
    block.resize(n, 0);
    block.copy_within(0..m, shift);
    let ln = block.len();
    let n = fd.write(&block)?;
    if n == ln {
        Ok(n.try_into().unwrap())
    } else {
        let msg = format!("write_meta_items: {:?} {}/{}...", &file, ln, n);
        Err(Error::PartialWrite(msg))
    }
}

/// Read meta items from [Robt] index file.
///
/// Meta-items is stored at the tip of the index file. If successful,
/// a vector of meta items. To learn more about the meta items
/// refer to [MetaItem] type.
///
/// [Robt]: crate::robt::Robt
pub fn read_meta_items(
    dir: &str,  // directory of index
    name: &str, // name of index
) -> Result<Vec<MetaItem>> {
    let index_file = Config::stitch_index_file(dir, name);
    let m = fs::metadata(&index_file)?.len();
    let mut fd = util::open_file_r(index_file.as_ref())?;

    // read header
    let hdr = util::read_buffer(&mut fd, m - 32, 32, "read root-block header")?;
    let root = u64::from_be_bytes(hdr[..8].try_into().unwrap());
    let n_md = u64::from_be_bytes(hdr[8..16].try_into().unwrap()) as usize;
    let n_stats = u64::from_be_bytes(hdr[16..24].try_into().unwrap()) as usize;
    let n_marker = u64::from_be_bytes(hdr[24..32].try_into().unwrap()) as usize;
    // read block
    let n = Config::compute_root_block(n_stats + n_md + n_marker + 32)
        .try_into()
        .unwrap();
    let block: Vec<u8> = util::read_buffer(&mut fd, m - n, n, "read root-block")?
        .into_iter()
        .collect();

    let mut meta_items: Vec<MetaItem> = vec![];
    let z = (n as usize) - 32;

    let (x, y) = (z - n_marker, z);
    let marker = block[x..y].to_vec();
    if marker.ne(&ROOT_MARKER.as_slice()) {
        let msg = format!("unexpected marker at {:?}", marker);
        return Err(Error::InvalidSnapshot(msg));
    }

    let (x, y) = (z - n_marker - n_stats, z - n_marker);
    let stats = std::str::from_utf8(&block[x..y])?.to_string();

    let (x, y) = (z - n_marker - n_stats - n_md, z - n_marker - n_stats);
    let app_data = block[x..y].to_vec();

    meta_items.push(MetaItem::Root(root));
    meta_items.push(MetaItem::AppMetadata(app_data));
    meta_items.push(MetaItem::Stats(stats));
    meta_items.push(MetaItem::Marker(marker.clone()));

    // validate and return
    if (m - n) != root {
        let msg = format!("expected root at {}, found {}", root, (m - n));
        Err(Error::InvalidSnapshot(msg))
    } else {
        Ok(meta_items)
    }
}

impl fmt::Display for MetaItem {
    fn fmt(&self, f: &mut fmt::Formatter) -> result::Result<(), fmt::Error> {
        match self {
            MetaItem::Marker(_) => write!(f, "MetaItem::Marker"),
            MetaItem::AppMetadata(_) => write!(f, "MetaItem::AppMetadata"),
            MetaItem::Stats(_) => write!(f, "MetaItem::Stats"),
            MetaItem::Root(_) => write!(f, "MetaItem::Root"),
        }
    }
}

/// Btree configuration and statistics persisted along with index file.
///
/// Note that build-only configuration options like:
/// * `tomb_purge`, configuration option.
/// * `flush_queue_size`,  configuration option.
///
/// are not persisted as part of statistics.
///
/// Meanwhile, for `vlog_file` configuration option, only file-name is
/// relevant, directory-path shall be ignored.
///
#[derive(Clone, Default, PartialEq)]
pub struct Stats {
    /// Leaf block size in btree index.
    pub z_blocksize: usize,
    /// Intemediate block size in btree index.
    pub m_blocksize: usize,
    /// If deltas are indexed and/or value to be stored in separate log file.
    pub v_blocksize: usize,
    /// Whether delta was included as part of the entry.
    pub delta_ok: bool,
    /// Separate log file for deltas and value, if `value_in_vlog` is true.
    /// Note that only file-name is relevat, directory-path shall be ignored.
    pub vlog_file: Option<ffi::OsString>,
    /// Whether value was persisted in value log file.
    pub value_in_vlog: bool,

    /// Number of entries indexed.
    pub n_count: u64,
    /// Number of entries that are marked as deleted.
    pub n_deleted: usize,
    /// Sequence number for the latest entry.
    pub seqno: u64,
    /// Total disk footprint for all keys.
    pub key_mem: usize,
    /// Total disk footprint for all deltas.
    pub diff_mem: usize,
    /// Total disk footprint for all values.
    pub val_mem: usize,
    /// Total disk footprint for all leaf-nodes.
    pub z_bytes: usize,
    /// Total disk footprint for all intermediate-nodes.
    pub m_bytes: usize,
    /// Total disk footprint for values and deltas.
    pub v_bytes: usize,
    /// Total disk size wasted in padding leaf-nodes and intermediate-nodes.
    pub padding: usize,
    /// Older size of value-log file, applicable only in incremental build.
    pub n_abytes: usize,

    /// Time take to build this btree.
    pub build_time: u64,
    /// Timestamp for this index.
    pub epoch: i128,
}

impl From<Config> for Stats {
    fn from(config: Config) -> Stats {
        Stats {
            z_blocksize: config.z_blocksize,
            m_blocksize: config.m_blocksize,
            v_blocksize: config.v_blocksize,
            delta_ok: config.delta_ok,
            vlog_file: config.vlog_file,
            value_in_vlog: config.value_in_vlog,

            n_count: Default::default(),
            n_deleted: Default::default(),
            seqno: Default::default(),
            key_mem: Default::default(),
            diff_mem: Default::default(),
            val_mem: Default::default(),
            z_bytes: Default::default(),
            v_bytes: Default::default(),
            m_bytes: Default::default(),
            padding: Default::default(),
            n_abytes: Default::default(),

            build_time: Default::default(),
            epoch: Default::default(),
        }
    }
}

impl FromStr for Stats {
    type Err = Error;

    fn from_str(s: &str) -> Result<Stats> {
        let js: Json = s.parse()?;
        let to_usize = |key: &str| -> Result<usize> {
            let n: usize = js.get(key)?.integer().unwrap().try_into().unwrap();
            Ok(n)
        };
        let to_u64 = |key: &str| -> Result<u64> {
            let n: u64 = js.get(key)?.integer().unwrap().try_into().unwrap();
            Ok(n)
        };
        let s = js.get("/vlog_file")?.string().unwrap();
        let vlog_file: Option<ffi::OsString> = match s {
            s if s.len() == 0 => None,
            s => Some(s.into()),
        };

        Ok(Stats {
            // config fields.
            z_blocksize: to_usize("/z_blocksize")?,
            m_blocksize: to_usize("/m_blocksize")?,
            v_blocksize: to_usize("/v_blocksize")?,
            delta_ok: js.get("/delta_ok")?.boolean().unwrap(),
            vlog_file: vlog_file,
            value_in_vlog: js.get("/value_in_vlog")?.boolean().unwrap(),
            // statitics fields.
            n_count: to_u64("/n_count")?,
            n_deleted: to_usize("/n_deleted")?,
            seqno: to_u64("/seqno")?,
            key_mem: to_usize("/key_mem")?,
            diff_mem: to_usize("/diff_mem")?,
            val_mem: to_usize("/val_mem")?,
            z_bytes: to_usize("/z_bytes")?,
            v_bytes: to_usize("/v_bytes")?,
            m_bytes: to_usize("/m_bytes")?,
            padding: to_usize("/padding")?,
            n_abytes: to_usize("/n_abytes")?,

            build_time: to_u64("/build_time")?,
            epoch: js.get("/epoch")?.integer().unwrap(),
        })
    }
}

impl Display for Stats {
    fn fmt(&self, f: &mut fmt::Formatter) -> result::Result<(), fmt::Error> {
        let mut js = Json::new::<Vec<Property>>(vec![]);

        let vlog_file = self.vlog_file.clone().unwrap_or(Default::default());
        let vlog_file = match vlog_file.into_string() {
            Ok(vlog_file) => vlog_file,
            Err(err) => panic!(err), // TODO: will is explode in production ??
        };

        js.set("/z_blocksize", Json::new(self.z_blocksize)).ok();
        js.set("/m_blocksize", Json::new(self.m_blocksize)).ok();
        js.set("/v_blocksize", Json::new(self.v_blocksize)).ok();
        js.set("/delta_ok", Json::new(self.delta_ok)).ok();
        js.set("/vlog_file", Json::new(vlog_file)).ok();
        js.set("/value_in_vlog", Json::new(self.value_in_vlog)).ok();

        js.set("/n_count", Json::new(self.n_count)).ok();
        js.set("/n_deleted", Json::new(self.n_deleted)).ok();
        js.set("/seqno", Json::new(self.seqno)).ok();
        js.set("/key_mem", Json::new(self.key_mem)).ok();
        js.set("/diff_mem", Json::new(self.diff_mem)).ok();
        js.set("/val_mem", Json::new(self.val_mem)).ok();
        js.set("/z_bytes", Json::new(self.z_bytes)).ok();
        js.set("/v_bytes", Json::new(self.v_bytes)).ok();
        js.set("/m_bytes", Json::new(self.m_bytes)).ok();
        js.set("/padding", Json::new(self.padding)).ok();
        js.set("/n_abytes", Json::new(self.n_abytes)).ok();

        js.set("/build_time", Json::new(self.build_time)).ok();
        js.set("/epoch", Json::new(self.epoch)).ok();

        write!(f, "{}", js.to_string())
    }
}

/// Builder type for Read-Only-BTree.
pub struct Builder<K, V>
where
    K: Clone + Ord + Serialize,
    V: Clone + Diff + Serialize,
    <V as Diff>::D: Serialize,
{
    config: Config,
    iflusher: Flusher,
    vflusher: Option<Flusher>,
    stats: Stats,

    phantom_key: marker::PhantomData<K>,
    phantom_val: marker::PhantomData<V>,
}

impl<K, V> Builder<K, V>
where
    K: Clone + Ord + Serialize,
    V: Clone + Diff + Serialize,
    <V as Diff>::D: Serialize,
{
    /// For initial builds, index file and value-log-file, if any,
    /// are always created new.
    pub fn initial(
        dir: &str, // directory path where index file(s) are stored
        name: &str,
        config: Config,
    ) -> Result<Builder<K, V>> {
        let create = true;
        let iflusher = {
            let file = config.to_index_file(dir, name);
            Flusher::new(file, config.clone(), create)?
        };
        let vflusher = config
            .to_value_log(dir, name)
            .map(|file| Flusher::new(file, config.clone(), create))
            .transpose()?;

        Ok(Builder {
            config: config.clone(),
            iflusher,
            vflusher,
            stats: From::from(config),
            phantom_key: marker::PhantomData,
            phantom_val: marker::PhantomData,
        })
    }

    /// For incremental build, index file is created new, while
    /// value-log-file, if any, is appended to older version.
    pub fn incremental(
        dir: &str, // directory path where index files are stored
        name: &str,
        config: Config,
    ) -> Result<Builder<K, V>> {
        let iflusher = {
            let file = config.to_index_file(dir, name);
            Flusher::new(file, config.clone(), true /*create*/)?
        };
        let vflusher = config
            .to_value_log(dir, name)
            .map(|file| Flusher::new(file, config.clone(), false /*create*/))
            .transpose()?;

        let mut stats: Stats = From::from(config.clone());
        stats.n_abytes += vflusher.as_ref().map_or(0, |vf| vf.fpos) as usize;

        Ok(Builder {
            config: config.clone(),
            iflusher,
            vflusher,
            stats,
            phantom_key: marker::PhantomData,
            phantom_val: marker::PhantomData,
        })
    }

    /// Build a new index.
    pub fn build<I>(mut self, iter: I, app_meta: Vec<u8>) -> Result<()>
    where
        I: Iterator<Item = Result<Entry<K, V>>>,
    {
        let (took, root): (u64, u64) = {
            let start = time::SystemTime::now();
            let root = self.build_tree(iter)?;
            (
                start.elapsed().unwrap().as_nanos().try_into().unwrap(),
                root,
            )
        };

        // meta-stats
        let stats: String = {
            self.stats.build_time = took;
            let epoch: i128 = time::UNIX_EPOCH
                .elapsed()
                .unwrap()
                .as_nanos()
                .try_into()
                .unwrap();
            self.stats.epoch = epoch;
            self.stats.to_string()
        };

        // start building metadata items for index files
        let meta_items: Vec<MetaItem> = vec![
            MetaItem::Root(root),
            MetaItem::AppMetadata(app_meta),
            MetaItem::Stats(stats),
            MetaItem::Marker(ROOT_MARKER.clone()), // tip of the index.
        ];
        // flush them to disk
        write_meta_items(self.iflusher.file.clone(), meta_items)?;

        // flush marker block and close
        self.iflusher.close_wait()?;
        self.vflusher.take().map(|x| x.close_wait()).transpose()?;

        Ok(())
    }

    fn build_tree<I>(&mut self, iter: I) -> Result<u64>
    where
        I: Iterator<Item = Result<Entry<K, V>>>,
    {
        struct Context<K, V>
        where
            K: Clone + Ord + Serialize,
            V: Clone + Diff + Serialize,
            <V as Diff>::D: Serialize,
        {
            fpos: u64,
            zfpos: u64,
            vfpos: u64,
            z: ZBlock<K, V>,
            ms: Vec<MBlock<K, V>>,
        };
        let mut c = {
            let vfpos = self.stats.n_abytes.try_into().unwrap();
            Context {
                fpos: 0,
                zfpos: 0,
                vfpos,
                z: ZBlock::new_encode(vfpos, self.config.clone()),
                ms: vec![MBlock::new_encode(self.config.clone())],
            }
        };

        for entry in iter {
            let mut entry = match self.preprocess(entry?) {
                Some(entry) => entry,
                None => continue,
            };

            match c.z.insert(&entry, &mut self.stats) {
                Ok(_) => (),
                Err(Error::__ZBlockOverflow(_)) => {
                    // zbytes is z_blocksize
                    let (zbytes, vbytes) = c.z.finalize(&mut self.stats);
                    c.z.flush(&mut self.iflusher, self.vflusher.as_mut())?;
                    c.fpos += zbytes;
                    c.vfpos += vbytes;

                    let mut m = c.ms.pop().unwrap();
                    match m.insertz(c.z.as_first_key(), c.zfpos) {
                        Ok(_) => c.ms.push(m),
                        Err(Error::__MBlockOverflow(_)) => {
                            // x is m_blocksize
                            let x = m.finalize(&mut self.stats);
                            m.flush(&mut self.iflusher)?;
                            let k = m.as_first_key();
                            let r = self.insertms(c.ms, c.fpos + x, k, c.fpos)?;
                            c.ms = r.0;
                            c.fpos = r.1;

                            m.reset();
                            m.insertz(c.z.as_first_key(), c.zfpos).unwrap();
                        }
                        Err(err) => return Err(err),
                    }

                    c.zfpos = c.fpos;
                    c.z.reset(c.vfpos);

                    c.z.insert(&entry, &mut self.stats).unwrap();
                }
                Err(err) => return Err(err),
            };

            self.postprocess(&mut entry);
        }

        // flush final z-block
        if c.z.has_first_key() {
            let (zbytes, _vbytes) = c.z.finalize(&mut self.stats);
            c.z.flush(&mut self.iflusher, self.vflusher.as_mut())?;
            c.fpos += zbytes;
            // vfpos += vbytes; TODO: is this required ?

            let mut m = c.ms.pop().unwrap();
            match m.insertz(c.z.as_first_key(), c.zfpos) {
                Ok(_) => c.ms.push(m),
                Err(Error::__MBlockOverflow(_)) => {
                    let x = m.finalize(&mut self.stats);
                    m.flush(&mut self.iflusher)?;
                    let mkey = m.as_first_key();
                    let res = self.insertms(c.ms, c.fpos + x, mkey, c.fpos)?;
                    c.ms = res.0;
                    c.fpos = res.1;

                    m.reset();
                    m.insertz(c.z.as_first_key(), c.zfpos)?;
                }
                Err(err) => return Err(err),
            }
        }

        // flush final set of m-blocks
        while let Some(mut m) = c.ms.pop() {
            if m.has_first_key() && c.ms.len() == 0 {
                let x = m.finalize(&mut self.stats);
                m.flush(&mut self.iflusher)?;
                c.fpos += x;
            } else if m.has_first_key() {
                // x is m_blocksize
                let x = m.finalize(&mut self.stats);
                m.flush(&mut self.iflusher)?;
                let mkey = m.as_first_key();
                let res = self.insertms(c.ms, c.fpos + x, mkey, c.fpos)?;
                c.ms = res.0;
                c.fpos = res.1
            }
        }
        Ok(c.fpos)
    }

    fn insertms(
        &mut self,
        mut ms: Vec<MBlock<K, V>>,
        mut fpos: u64,
        key: &K,
        mfpos: u64,
    ) -> Result<(Vec<MBlock<K, V>>, u64)> {
        let m0 = ms.pop();
        let m0 = match m0 {
            None => {
                let mut m0 = MBlock::new_encode(self.config.clone());
                m0.insertm(key, mfpos).unwrap();
                m0
            }
            Some(mut m0) => match m0.insertm(key, mfpos) {
                Ok(_) => m0,
                Err(Error::__MBlockOverflow(_)) => {
                    // x is m_blocksize
                    let x = m0.finalize(&mut self.stats);
                    m0.flush(&mut self.iflusher)?;
                    let mkey = m0.as_first_key();
                    let res = self.insertms(ms, fpos + x, mkey, fpos)?;
                    ms = res.0;
                    fpos = res.1;

                    m0.reset();
                    m0.insertm(key, mfpos).unwrap();
                    m0
                }
                Err(err) => return Err(err),
            },
        };
        ms.push(m0);
        Ok((ms, fpos))
    }

    fn preprocess(&mut self, entry: Entry<K, V>) -> Option<Entry<K, V>> {
        self.stats.seqno = cmp::max(self.stats.seqno, entry.to_seqno());

        // if tombstone purge is configured, then purge all versions
        // on or before the purge-seqno.
        match self.config.tomb_purge {
            Some(before) => entry.purge(Bound::Excluded(before)),
            _ => Some(entry),
        }
    }

    fn postprocess(&mut self, entry: &mut Entry<K, V>) {
        self.stats.n_count += 1;
        if entry.is_deleted() {
            self.stats.n_deleted += 1;
        }
    }
}

pub(crate) struct Flusher {
    file: ffi::OsString,
    fpos: u64,
    t: thread::JoinHandle<Result<()>>,
    tx: mpsc::SyncSender<Vec<u8>>,
}

impl Flusher {
    fn new(
        file: ffi::OsString,
        config: Config,
        create: bool, // if true create a new file
    ) -> Result<Flusher> {
        let (fd, fpos) = if create {
            (util::open_file_cw(file.clone())?, Default::default())
        } else {
            (util::open_file_w(&file)?, fs::metadata(&file)?.len())
        };

        let (tx, rx) = mpsc::sync_channel(config.flush_queue_size);
        let file1 = file.clone();
        let t = thread::spawn(move || thread_flush(file1, fd, rx));

        Ok(Flusher { file, fpos, t, tx })
    }

    // return error if flush thread has exited/paniced.
    pub(crate) fn send(&mut self, block: Vec<u8>) -> Result<()> {
        self.tx.send(block)?;
        Ok(())
    }

    // return the cause for thread failure, if there is a failure, or return
    // a known error like io::Error or PartialWrite.
    fn close_wait(self) -> Result<()> {
        mem::drop(self.tx);
        match self.t.join() {
            Ok(Ok(())) => Ok(()),
            Ok(Err(Error::PartialWrite(err))) => Err(Error::PartialWrite(err)),
            Ok(Err(_)) => unreachable!(),
            Err(err) => match err.downcast_ref::<String>() {
                Some(msg) => Err(Error::ThreadFail(msg.to_string())),
                None => Err(Error::ThreadFail("unknown error".to_string())),
            },
        }
    }
}

fn thread_flush(
    file: ffi::OsString, // for debuging purpose
    mut fd: fs::File,
    rx: mpsc::Receiver<Vec<u8>>,
) -> Result<()> {
    for data in rx.iter() {
        let n = fd.write(&data)?;
        if n != data.len() {
            let msg = format!("flusher: {:?} {}/{}...", &file, data.len(), n);
            return Err(Error::PartialWrite(msg));
        }
    }
    // file descriptor and receiver channel shall be dropped.
    Ok(())
}

/// A read only snapshot of BTree built using [robt] index.
///
/// [robt]: crate::robt
pub struct Snapshot<K, V>
where
    K: Clone + Ord + Serialize,
    V: Clone + Diff + Serialize,
{
    dir: String,
    name: String,
    meta: Vec<MetaItem>,
    // working fields
    config: Config,
    index_fd: fs::File,
    vlog_fd: Option<fs::File>,
    mutex: sync::Mutex<i32>,

    phantom_key: marker::PhantomData<K>,
    phantom_val: marker::PhantomData<V>,
}

// Construction methods.
impl<K, V> Snapshot<K, V>
where
    K: Clone + Ord + Serialize,
    V: Clone + Diff + Serialize,
{
    /// Open BTree snapshot from file that can be constructed from ``dir``
    /// and ``name``.
    pub fn open(dir: &str, name: &str) -> Result<Snapshot<K, V>> {
        let meta_items = read_meta_items(dir, name)?;
        let mut snap = Snapshot {
            dir: dir.to_string(),
            name: name.to_string(),
            meta: meta_items,
            config: Default::default(),
            index_fd: {
                let index_file = Config::stitch_index_file(dir, name);
                util::open_file_r(&index_file.as_ref())?
            },
            vlog_fd: Default::default(),
            mutex: sync::Mutex::new(0),

            phantom_key: marker::PhantomData,
            phantom_val: marker::PhantomData,
        };
        snap.config = snap.to_stats()?.into();
        snap.config.vlog_file = snap.config.vlog_file.map(|vfile| {
            // stem the file name.
            let vfile = path::Path::new(&vfile).file_name().unwrap();
            let ipath = Config::stitch_index_file(&dir, &name);
            let mut vpath = path::PathBuf::new();
            vpath.push(path::Path::new(&ipath).parent().unwrap());
            vpath.push(vfile);
            vpath.as_os_str().to_os_string()
        });
        snap.vlog_fd = snap
            .config
            .to_value_log(dir, name)
            .as_ref()
            .map(|s| util::open_file_r(s.as_ref()))
            .transpose()?;

        Ok(snap) // Okey dockey
    }
}

// maintanence methods.
impl<K, V> Snapshot<K, V>
where
    K: Clone + Ord + Serialize,
    V: Clone + Diff + Serialize,
{
    /// Return number of entries in the snapshot.
    pub fn len(&self) -> u64 {
        self.to_stats().unwrap().n_count
    }

    /// Return the last seqno found in this snapshot.
    pub fn to_seqno(&self) -> u64 {
        self.to_stats().unwrap().seqno
    }

    /// Return the application metadata.
    pub fn to_app_meta(&self) -> Result<Vec<u8>> {
        if let MetaItem::AppMetadata(data) = &self.meta[1] {
            Ok(data.clone())
        } else {
            let msg = "snapshot app-metadata missing".to_string();
            Err(Error::InvalidSnapshot(msg))
        }
    }

    /// Return Btree statistics.
    pub fn to_stats(&self) -> Result<Stats> {
        if let MetaItem::Stats(stats) = &self.meta[2] {
            Ok(stats.parse()?)
        } else {
            let msg = "snapshot statistics missing".to_string();
            Err(Error::InvalidSnapshot(msg))
        }
    }

    /// Return the file-position for Btree's root node.
    pub fn to_root(&self) -> Result<u64> {
        if let MetaItem::Root(root) = self.meta[3] {
            Ok(root)
        } else {
            Err(Error::InvalidSnapshot("snapshot root missing".to_string()))
        }
    }
}

impl<K, V> Index<K, V> for Snapshot<K, V>
where
    K: Clone + Ord + Serialize + Footprint,
    V: Clone + Diff + Serialize + Footprint,
{
    type W = RobtWriter;

    /// Make a new empty index of this type, with same configuration.
    fn make_new(&self) -> Result<Box<Self>> {
        Ok(Box::new(Snapshot::open(
            self.name.as_str(),
            self.dir.as_str(),
        )?))
    }

    /// Create a new writer handle. Note that, not all indexes allow
    /// concurrent writers, and not all indexes support concurrent
    /// read/write.
    fn to_writer(&mut self) -> Self::W {
        panic!("Read-only-btree don't support write operations")
    }
}

impl<K, V> Footprint for Snapshot<K, V>
where
    K: Clone + Ord + Serialize,
    V: Clone + Diff + Serialize,
{
    fn footprint(&self) -> isize {
        let (dir, name) = (self.dir.as_str(), self.name.as_str());
        let mut footprint = fs::metadata(self.config.to_index_file(dir, name))
            .unwrap()
            .len();
        footprint += match self.config.to_value_log(dir, name) {
            Some(vlog_file) => fs::metadata(vlog_file).unwrap().len(),
            None => 0,
        };
        footprint.try_into().unwrap()
    }
}

// Read methods
impl<K, V> Reader<K, V> for Snapshot<K, V>
where
    K: Clone + Ord + Serialize,
    V: Clone + Diff + Serialize,
    <V as Diff>::D: Clone + Serialize,
{
    fn get<Q>(&self, key: &Q) -> Result<Entry<K, V>>
    where
        K: Borrow<Q>,
        Q: Ord + ?Sized,
    {
        let _lock = self.mutex.lock();
        let snap = unsafe {
            let snap = self as *const Snapshot<K, V> as *mut Snapshot<K, V>;
            snap.as_mut().unwrap()
        };

        snap.do_get(key, false /*versions*/)
    }

    fn iter(&self) -> Result<IndexIter<K, V>> {
        let _lock = self.mutex.lock();
        let snap = unsafe {
            let snap = self as *const Snapshot<K, V> as *mut Snapshot<K, V>;
            snap.as_mut().unwrap()
        };

        let mut mzs = vec![];
        snap.build_fwd(snap.to_root().unwrap(), &mut mzs)?;
        Ok(Iter::new(snap, mzs))
    }

    fn range<'a, R, Q>(&'a self, range: R) -> Result<IndexIter<K, V>>
    where
        K: Borrow<Q>,
        R: 'a + RangeBounds<Q>,
        Q: 'a + Ord + ?Sized,
    {
        let _lock = self.mutex.lock();
        let snap = unsafe {
            let snap = self as *const Snapshot<K, V> as *mut Snapshot<K, V>;
            snap.as_mut().unwrap()
        };

        snap.do_range(range, false /*versions*/)
    }

    fn reverse<'a, R, Q>(&'a self, range: R) -> Result<IndexIter<K, V>>
    where
        K: Borrow<Q>,
        R: 'a + RangeBounds<Q>,
        Q: 'a + Ord + ?Sized,
    {
        let _lock = self.mutex.lock();
        let snap = unsafe {
            let snap = self as *const Snapshot<K, V> as *mut Snapshot<K, V>;
            snap.as_mut().unwrap()
        };

        snap.do_reverse(range, false /*versions*/)
    }

    fn get_with_versions<Q>(&self, key: &Q) -> Result<Entry<K, V>>
    where
        K: Borrow<Q>,
        Q: Ord + ?Sized,
    {
        let _lock = self.mutex.lock();
        let snap = unsafe {
            let snap = self as *const Snapshot<K, V> as *mut Snapshot<K, V>;
            snap.as_mut().unwrap()
        };

        snap.do_get(key, true /*versions*/)
    }

    /// Iterate over all entries in this index. Returned entry shall
    /// have all its previous versions, can be a costly call.
    fn iter_with_versions(&self) -> Result<IndexIter<K, V>> {
        let _lock = self.mutex.lock();
        let snap = unsafe {
            let snap = self as *const Snapshot<K, V> as *mut Snapshot<K, V>;
            snap.as_mut().unwrap()
        };

        let mut mzs = vec![];
        snap.build_fwd(snap.to_root().unwrap(), &mut mzs)?;
        Ok(Iter::new_versions(snap, mzs))
    }

    /// Iterate from lower bound to upper bound. Returned entry shall
    /// have all its previous versions, can be a costly call.
    fn range_with_versions<'a, R, Q>(
        &'a self,
        range: R, // range bound
    ) -> Result<IndexIter<K, V>>
    where
        K: Borrow<Q>,
        R: 'a + RangeBounds<Q>,
        Q: 'a + Ord + ?Sized,
    {
        let _lock = self.mutex.lock();
        let snap = unsafe {
            let snap = self as *const Snapshot<K, V> as *mut Snapshot<K, V>;
            snap.as_mut().unwrap()
        };

        snap.do_range(range, true /*versions*/)
    }

    /// Iterate from upper bound to lower bound. Returned entry shall
    /// have all its previous versions, can be a costly call.
    fn reverse_with_versions<'a, R, Q>(&'a self, range: R) -> Result<IndexIter<K, V>>
    where
        K: Borrow<Q>,
        R: 'a + RangeBounds<Q>,
        Q: 'a + Ord + ?Sized,
    {
        let _lock = self.mutex.lock();
        let snap = unsafe {
            let snap = self as *const Snapshot<K, V> as *mut Snapshot<K, V>;
            snap.as_mut().unwrap()
        };

        snap.do_reverse(range, true /*versions*/)
    }
}

impl<K, V> Snapshot<K, V>
where
    K: Clone + Ord + Serialize,
    V: Clone + Diff + Serialize,
    <V as Diff>::D: Clone + Serialize,
{
    fn get_zpos<Q>(&mut self, key: &Q, fpos: u64) -> Result<u64>
    where
        K: Borrow<Q>,
        Q: Ord + ?Sized,
    {
        let fd = &mut self.index_fd;
        let mblock = MBlock::<K, V>::new_decode(fd, fpos, &self.config)?;
        match mblock.get(key, Bound::Unbounded, Bound::Unbounded) {
            Err(Error::__LessThan) => Err(Error::KeyNotFound),
            Err(Error::__MBlockExhausted(_)) => unreachable!(),
            Ok(mentry) if mentry.is_zblock() => Ok(mentry.to_fpos()),
            Ok(mentry) => self.get_zpos(key, mentry.to_fpos()),
            Err(err) => Err(err),
        }
    }

    fn do_get<Q>(&mut self, key: &Q, versions: bool) -> Result<Entry<K, V>>
    where
        K: Borrow<Q>,
        Q: Ord + ?Sized,
    {
        let zfpos = self.get_zpos(key, self.to_root().unwrap())?;

        let fd = &mut self.index_fd;
        let zblock: ZBlock<K, V> = ZBlock::new_decode(fd, zfpos, &self.config)?;
        match zblock.find(key, Bound::Unbounded, Bound::Unbounded) {
            Ok((_, entry)) => {
                if entry.as_key().borrow().eq(key) {
                    self.fetch(entry, versions)
                } else {
                    Err(Error::KeyNotFound)
                }
            }
            Err(Error::__ZBlockExhausted(_)) => unreachable!(),
            Err(err) => Err(err),
        }
    }

    fn do_range<'a, R, Q>(
        &'a mut self,
        range: R,
        versions: bool, // if true include older versions.
    ) -> Result<IndexIter<K, V>>
    where
        K: Borrow<Q>,
        R: 'a + RangeBounds<Q>,
        Q: 'a + Ord + ?Sized,
    {
        let mut mzs = vec![];
        let skip_one = match range.start_bound() {
            Bound::Unbounded => {
                self.build_fwd(self.to_root().unwrap(), &mut mzs)?;
                false
            }
            Bound::Included(key) => {
                let entry = self.build(key, &mut mzs)?;
                match key.cmp(entry.as_key().borrow()) {
                    cmp::Ordering::Greater => true,
                    _ => false,
                }
            }
            Bound::Excluded(key) => {
                let entry = self.build(key, &mut mzs)?;
                match key.cmp(entry.as_key().borrow()) {
                    cmp::Ordering::Equal | cmp::Ordering::Greater => true,
                    _ => false,
                }
            }
        };
        let mut r = Range::new(self, mzs, range, versions);
        if skip_one {
            r.next();
        }
        Ok(r)
    }

    fn do_reverse<'a, R, Q>(
        &'a mut self,
        range: R, // reverse range bound
        versions: bool,
    ) -> Result<IndexIter<K, V>>
    where
        K: Borrow<Q>,
        R: 'a + RangeBounds<Q>,
        Q: 'a + Ord + ?Sized,
    {
        let mut mzs = vec![];
        let skip_one = match range.end_bound() {
            Bound::Unbounded => {
                self.build_rev(self.to_root().unwrap(), &mut mzs)?;
                false
            }
            Bound::Included(key) => {
                self.build(&key, &mut mzs)?;
                false
            }
            Bound::Excluded(key) => {
                let entry = self.build(&key, &mut mzs)?;
                key.eq(entry.as_key().borrow())
            }
        };
        let mut rr = Reverse::new(self, mzs, range, versions);
        if skip_one {
            rr.next();
        }
        Ok(rr)
    }

    fn build_fwd(
        &mut self,
        mut fpos: u64,           // from node
        mzs: &mut Vec<MZ<K, V>>, // output
    ) -> Result<()> {
        let fd = &mut self.index_fd;
        let config = &self.config;

        let zfpos = loop {
            let mblock = MBlock::<K, V>::new_decode(fd, fpos, config)?;
            let mentry = mblock.to_entry(0)?;
            if mentry.is_zblock() {
                break mentry.to_fpos();
            }
            mzs.push(MZ::M { fpos, index: 0 });
            fpos = mentry.to_fpos();
        };

        let zblock = ZBlock::new_decode(fd, zfpos, config)?;
        mzs.push(MZ::Z { zblock, index: 0 });
        Ok(())
    }

    fn rebuild_fwd(&mut self, mzs: &mut Vec<MZ<K, V>>) -> Result<()> {
        let fd = &mut self.index_fd;
        let config = &self.config;

        match mzs.pop() {
            None => Ok(()),
            Some(MZ::Z { .. }) => unreachable!(),
            Some(MZ::M { fpos, mut index }) => {
                let mblock = MBlock::<K, V>::new_decode(fd, fpos, config)?;
                index += 1;
                match mblock.to_entry(index) {
                    Ok(MEntry::DecZ { fpos: zfpos, .. }) => {
                        mzs.push(MZ::M { fpos, index });

                        let zblock = ZBlock::new_decode(fd, zfpos, config)?;
                        mzs.push(MZ::Z { zblock, index: 0 });
                        Ok(())
                    }
                    Ok(MEntry::DecM { fpos: mfpos, .. }) => {
                        mzs.push(MZ::M { fpos, index });
                        self.build_fwd(mfpos, mzs)?;
                        Ok(())
                    }
                    Err(Error::__ZBlockExhausted(_)) => self.rebuild_fwd(mzs),
                    _ => unreachable!(),
                }
            }
        }
    }

    fn build_rev(
        &mut self,
        mut fpos: u64,           // from node
        mzs: &mut Vec<MZ<K, V>>, // output
    ) -> Result<()> {
        let fd = &mut self.index_fd;
        let config = &self.config;

        let zfpos = loop {
            let mblock = MBlock::<K, V>::new_decode(fd, fpos, config)?;
            let index = mblock.len() - 1;
            let mentry = mblock.to_entry(index)?;
            if mentry.is_zblock() {
                break mentry.to_fpos();
            }
            mzs.push(MZ::M { fpos, index });
            fpos = mentry.to_fpos();
        };

        let zblock = ZBlock::new_decode(fd, zfpos, config)?;
        let index = zblock.len() - 1;
        mzs.push(MZ::Z { zblock, index });
        Ok(())
    }

    fn rebuild_rev(&mut self, mzs: &mut Vec<MZ<K, V>>) -> Result<()> {
        let fd = &mut self.index_fd;
        let config = &self.config;

        match mzs.pop() {
            None => Ok(()),
            Some(MZ::Z { .. }) => unreachable!(),
            Some(MZ::M { index: 0, .. }) => self.rebuild_rev(mzs),
            Some(MZ::M { fpos, mut index }) => {
                let mblock = MBlock::<K, V>::new_decode(fd, fpos, config)?;
                index -= 1;
                match mblock.to_entry(index) {
                    Ok(MEntry::DecZ { fpos: zfpos, .. }) => {
                        mzs.push(MZ::M { fpos, index });

                        let zblock = ZBlock::new_decode(fd, zfpos, config)?;
                        let index = zblock.len() - 1;
                        mzs.push(MZ::Z { zblock, index });
                        Ok(())
                    }
                    Ok(MEntry::DecM { fpos: mfpos, .. }) => {
                        mzs.push(MZ::M { fpos, index });
                        self.build_rev(mfpos, mzs)?;
                        Ok(())
                    }
                    _ => unreachable!(),
                }
            }
        }
    }

    fn build<Q>(
        &mut self,
        key: &Q,
        mzs: &mut Vec<MZ<K, V>>, // output
    ) -> Result<Entry<K, V>>
    where
        K: Borrow<Q>,
        Q: Ord + ?Sized,
    {
        let mut fpos = self.to_root().unwrap();
        let fd = &mut self.index_fd;
        let config = &self.config;
        let (from_min, to_max) = (Bound::Unbounded, Bound::Unbounded);

        let zfpos = loop {
            let mblock = MBlock::<K, V>::new_decode(fd, fpos, config)?;
            match mblock.find(key, from_min, to_max) {
                Ok(mentry) => {
                    if mentry.is_zblock() {
                        break mentry.to_fpos();
                    }
                    let index = mentry.to_index();
                    mzs.push(MZ::M { fpos, index });
                    fpos = mentry.to_fpos();
                }
                Err(Error::__LessThan) => unreachable!(),
                Err(err) => return Err(err),
            }
        };

        let zblock = ZBlock::new_decode(fd, zfpos, config)?;
        let (index, entry) = zblock.find(key, from_min, to_max)?;
        mzs.push(MZ::Z { zblock, index });
        Ok(entry)
    }

    fn fetch(
        &mut self,
        mut entry: Entry<K, V>,
        versions: bool, // fetch deltas as well
    ) -> Result<Entry<K, V>> {
        match &mut self.vlog_fd {
            Some(fd) => entry.fetch_value(fd)?,
            _ => (),
        }
        if versions {
            match &mut self.vlog_fd {
                Some(fd) => entry.fetch_deltas(fd)?,
                _ => (),
            }
        }
        Ok(entry)
    }
}

/// Iterate over [Robt] index, from beginning to end.
///
/// [Robt]: crate::robt::Robt
pub struct Iter<'a, K, V>
where
    K: Clone + Ord + Serialize,
    V: Clone + Diff + Serialize,
    <V as Diff>::D: Clone + Serialize,
{
    snap: &'a mut Snapshot<K, V>,
    mzs: Vec<MZ<K, V>>,
    versions: bool,
}

impl<'a, K, V> Iter<'a, K, V>
where
    K: Clone + Ord + Serialize,
    V: Clone + Diff + Serialize,
    <V as Diff>::D: Clone + Serialize,
{
    fn new(snap: &'a mut Snapshot<K, V>, mzs: Vec<MZ<K, V>>) -> Box<Self> {
        Box::new(Iter {
            snap,
            mzs,
            versions: false,
        })
    }

    fn new_versions(
        snap: &'a mut Snapshot<K, V>, // reference to snapshot
        mzs: Vec<MZ<K, V>>,
    ) -> Box<Self> {
        Box::new(Iter {
            snap,
            mzs,
            versions: true,
        })
    }
}

impl<'a, K, V> Iterator for Iter<'a, K, V>
where
    K: Clone + Ord + Serialize,
    V: Clone + Diff + Serialize,
    <V as Diff>::D: Clone + Serialize,
{
    type Item = Result<Entry<K, V>>;

    fn next(&mut self) -> Option<Result<Entry<K, V>>> {
        match self.mzs.pop() {
            None => None,
            Some(mut z) => match z.next() {
                Some(Err(err)) => {
                    self.mzs.truncate(0);
                    Some(Err(err))
                }
                Some(Ok(entry)) => {
                    self.mzs.push(z);
                    Some(self.snap.fetch(entry, self.versions))
                }
                None => match self.snap.rebuild_fwd(&mut self.mzs) {
                    Err(err) => Some(Err(err)),
                    Ok(_) => self.next(),
                },
            },
        }
    }
}

/// Iterate over [Robt] index, from a lower bound to upper bound.
///
/// [Robt]: crate::robt::Robt
pub struct Range<'a, K, V, R, Q>
where
    K: Clone + Ord + Borrow<Q> + Serialize,
    V: Clone + Diff + Serialize,
    <V as Diff>::D: Clone + Serialize,
    R: RangeBounds<Q>,
    Q: Ord + ?Sized,
{
    snap: &'a mut Snapshot<K, V>,
    mzs: Vec<MZ<K, V>>,
    range: R,
    high: marker::PhantomData<Q>,
    versions: bool,
}

impl<'a, K, V, R, Q> Range<'a, K, V, R, Q>
where
    K: Clone + Ord + Borrow<Q> + Serialize,
    V: Clone + Diff + Serialize,
    <V as Diff>::D: Clone + Serialize,
    R: RangeBounds<Q>,
    Q: Ord + ?Sized,
{
    fn new(
        snap: &'a mut Snapshot<K, V>,
        mzs: Vec<MZ<K, V>>,
        range: R, // range bound
        versions: bool,
    ) -> Box<Self> {
        Box::new(Range {
            snap,
            mzs,
            range,
            high: marker::PhantomData,
            versions,
        })
    }

    fn till_ok(&self, entry: &Entry<K, V>) -> bool {
        match self.range.end_bound() {
            Bound::Unbounded => true,
            Bound::Included(key) => entry.as_key().borrow().le(key),
            Bound::Excluded(key) => entry.as_key().borrow().lt(key),
        }
    }
}

impl<'a, K, V, R, Q> Iterator for Range<'a, K, V, R, Q>
where
    K: Clone + Ord + Borrow<Q> + Serialize,
    V: Clone + Diff + Serialize,
    <V as Diff>::D: Clone + Serialize,
    R: RangeBounds<Q>,
    Q: Ord + ?Sized,
{
    type Item = Result<Entry<K, V>>;

    fn next(&mut self) -> Option<Result<Entry<K, V>>> {
        match self.mzs.pop() {
            None => None,
            Some(mut z) => match z.next() {
                Some(Err(err)) => {
                    self.mzs.truncate(0);
                    Some(Err(err))
                }
                Some(Ok(entry)) => {
                    if self.till_ok(&entry) {
                        self.mzs.push(z);
                        Some(self.snap.fetch(entry, self.versions))
                    } else {
                        self.mzs.truncate(0);
                        None
                    }
                }
                None => match self.snap.rebuild_fwd(&mut self.mzs) {
                    Err(err) => Some(Err(err)),
                    Ok(_) => self.next(),
                },
            },
        }
    }
}

/// Iterate over [Robt] index, from an upper bound to lower bound.
///
/// [Robt]: crate::robt::Robt
pub struct Reverse<'a, K, V, R, Q>
where
    K: Clone + Ord + Borrow<Q> + Serialize,
    V: Clone + Diff + Serialize,
    <V as Diff>::D: Clone + Serialize,
    R: RangeBounds<Q>,
    Q: Ord + ?Sized,
{
    snap: &'a mut Snapshot<K, V>,
    mzs: Vec<MZ<K, V>>,
    range: R,
    low: marker::PhantomData<Q>,
    versions: bool,
}

impl<'a, K, V, R, Q> Reverse<'a, K, V, R, Q>
where
    K: Clone + Ord + Borrow<Q> + Serialize,
    V: Clone + Diff + Serialize,
    <V as Diff>::D: Clone + Serialize,
    R: RangeBounds<Q>,
    Q: Ord + ?Sized,
{
    fn new(
        snap: &'a mut Snapshot<K, V>,
        mzs: Vec<MZ<K, V>>,
        range: R, // reverse range bound
        versions: bool,
    ) -> Box<Self> {
        Box::new(Reverse {
            snap,
            mzs,
            range,
            low: marker::PhantomData,
            versions,
        })
    }

    fn till_ok(&self, entry: &Entry<K, V>) -> bool {
        match self.range.start_bound() {
            Bound::Unbounded => true,
            Bound::Included(key) => entry.as_key().borrow().ge(key),
            Bound::Excluded(key) => entry.as_key().borrow().gt(key),
        }
    }
}

impl<'a, K, V, R, Q> Iterator for Reverse<'a, K, V, R, Q>
where
    K: Clone + Ord + Borrow<Q> + Serialize,
    V: Clone + Diff + Serialize,
    <V as Diff>::D: Clone + Serialize,
    R: RangeBounds<Q>,
    Q: Ord + ?Sized,
{
    type Item = Result<Entry<K, V>>;

    fn next(&mut self) -> Option<Result<Entry<K, V>>> {
        match self.mzs.pop() {
            None => None,
            Some(mut z) => match z.next_back() {
                Some(Err(err)) => {
                    self.mzs.truncate(0);
                    Some(Err(err))
                }
                Some(Ok(entry)) => {
                    if self.till_ok(&entry) {
                        self.mzs.push(z);
                        Some(self.snap.fetch(entry, self.versions))
                    } else {
                        self.mzs.truncate(0);
                        None
                    }
                }
                None => match self.snap.rebuild_rev(&mut self.mzs) {
                    Err(err) => Some(Err(err)),
                    Ok(_) => self.next(),
                },
            },
        }
    }
}

enum MZ<K, V>
where
    K: Clone + Ord + Serialize,
    V: Clone + Diff + Serialize,
    <V as Diff>::D: Clone + Serialize,
{
    M { fpos: u64, index: usize },
    Z { zblock: ZBlock<K, V>, index: usize },
}

impl<K, V> Iterator for MZ<K, V>
where
    K: Clone + Ord + Serialize,
    V: Clone + Diff + Serialize,
    <V as Diff>::D: Clone + Serialize,
{
    type Item = Result<Entry<K, V>>;

    fn next(&mut self) -> Option<Result<Entry<K, V>>> {
        match self {
            MZ::Z { zblock, index } => match zblock.to_entry(*index) {
                Ok((_, entry)) => {
                    *index += 1;
                    Some(Ok(entry))
                }
                Err(Error::__ZBlockExhausted(_)) => None,
                Err(err) => Some(Err(err)),
            },
            MZ::M { .. } => unreachable!(),
        }
    }
}

impl<K, V> DoubleEndedIterator for MZ<K, V>
where
    K: Clone + Ord + Serialize,
    V: Clone + Diff + Serialize,
    <V as Diff>::D: Clone + Serialize,
{
    fn next_back(&mut self) -> Option<Result<Entry<K, V>>> {
        match self {
            MZ::Z { zblock, index } => match zblock.to_entry(*index) {
                Ok((_, entry)) => {
                    *index -= 1;
                    Some(Ok(entry))
                }
                Err(Error::__ZBlockExhausted(_)) => None,
                Err(err) => Some(Err(err)),
            },
            MZ::M { .. } => unreachable!(),
        }
    }
}

/// Dummy writer exported for consistency sake. [Robt] instances are
/// immutable index.
///
/// [Robt]: crate::robt::Robt
pub struct RobtWriter;

impl<K, V> Writer<K, V> for RobtWriter
where
    K: Clone + Ord + Footprint,
    V: Clone + Diff + Footprint,
{
    fn set_index(
        &mut self,
        key: K,
        value: V,
        seqno: u64, // seqno for this mutation
    ) -> (Option<u64>, Result<Option<Entry<K, V>>>) {
        panic!(
            "{} {} {}",
            mem::size_of_val(&key),
            mem::size_of_val(&value),
            seqno
        )
    }

    fn set_cas_index(
        &mut self,
        key: K,
        value: V,
        cas: u64,
        seqno: u64, // seqno for this mutation
    ) -> (Option<u64>, Result<Option<Entry<K, V>>>) {
        panic!(
            "{} {} {} {}",
            mem::size_of_val(&key),
            mem::size_of_val(&value),
            seqno,
            cas
        )
    }

    fn delete_index<Q>(
        &mut self,
        key: &Q,
        seqno: u64, // seqno for this mutation
    ) -> (Option<u64>, Result<Option<Entry<K, V>>>)
    where
        K: Borrow<Q>,
        Q: ToOwned<Owned = K> + Ord + ?Sized,
    {
        panic!("{} {}", mem::size_of_val(key), seqno)
    }
}

#[cfg(test)]
#[path = "robt_test.rs"]
mod robt_test;