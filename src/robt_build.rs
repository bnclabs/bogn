// TODO: flush put blocks into tx channel. Right now we simply unwrap()

use std::ops::Bound;
use std::sync::mpsc;
use std::{cmp, convert::TryInto, fs, io::Write, marker, mem, thread, time};

use crate::core::{Diff, Entry, Result, Serialize};
use crate::error::Error;
use crate::robt_config::{self, Config, MetaItem, ROOT_MARKER};
use crate::robt_indx::{MBlock, ZBlock};
use crate::robt_stats::Stats;
use crate::util;

/// Build a new instance of Read-Only-BTree.
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
    pub fn initial(config: Config, dir: &str, name: &str) -> Result<Builder<K, V>> {
        let iflusher = {
            let file = config.to_index_file(dir, name);
            Flusher::new(file, config.clone(), false /*reuse*/)?
        };
        let vflusher = config
            .to_value_log(dir, name)
            .map(|file| Flusher::new(file, config.clone(), false /*reuse*/))
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
    pub fn incremental(config: Config, dir: &str, name: &str) -> Result<Builder<K, V>> {
        let iflusher = {
            let file = config.to_index_file(dir, name);
            Flusher::new(file, config.clone(), false /*reuse*/)?
        };
        let vflusher = config
            .to_value_log(dir, name)
            .map(|file| Flusher::new(file, config.clone(), true /*reuse*/))
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
    pub fn build<I>(mut self, iter: I, metadata: Vec<u8>) -> Result<()>
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
        let stats = {
            self.stats.buildtime = took;
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
            MetaItem::Marker(ROOT_MARKER.clone()),
            MetaItem::Metadata(metadata),
            MetaItem::Stats(stats),
            MetaItem::Root(root),
        ];
        // flush them to disk
        robt_config::write_meta_items(meta_items, &mut self.iflusher)?;

        // flush marker block and close
        self.iflusher.close_wait()?;
        self.vflusher.take().map(|x| x.close_wait()).transpose()?;

        Ok(())
    }

    fn build_tree<I>(&mut self, mut iter: I) -> Result<u64>
    // return root
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

        for entry in iter.next() {
            let mut entry = match self.preprocess(entry?) {
                Some(entry) => entry,
                None => continue,
            };

            match c.z.insert(&entry, &mut self.stats) {
                Ok(_) => (),
                Err(Error::__ZBlockOverflow(_)) => {
                    let (zbytes, vbytes) = c.z.finalize(&mut self.stats);
                    c.z.flush(&mut self.iflusher, self.vflusher.as_mut())?;
                    c.fpos += zbytes;
                    c.vfpos += vbytes;

                    let mut m = c.ms.pop().unwrap();
                    match m.insertz(c.z.as_first_key(), c.zfpos) {
                        Ok(_) => (),
                        Err(Error::__MBlockOverflow(_)) => {
                            let x = m.finalize(&mut self.stats);
                            m.flush(&mut self.iflusher)?;
                            let k = m.as_first_key();
                            let r = self.insertms(c.ms, c.fpos + x, k, c.fpos)?;
                            c.ms = r.0;
                            c.fpos = r.1;

                            m.reset();
                            m.insertz(c.z.as_first_key(), c.zfpos).unwrap();
                        }
                        _ => unreachable!(),
                    }
                    c.ms.push(m);

                    c.zfpos = c.fpos;
                    c.z.reset(c.vfpos);

                    c.z.insert(&entry, &mut self.stats).unwrap();
                }
                _ => unreachable!(),
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
                Ok(_) => (),
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
                _ => unreachable!(),
            }
            c.ms.push(m);
        }
        // flush final set of m-blocks
        if c.ms.len() > 0 {
            while let Some(mut m) = c.ms.pop() {
                if m.has_first_key() {
                    let x = m.finalize(&mut self.stats);
                    m.flush(&mut self.iflusher)?;
                    let mkey = m.as_first_key();
                    let res = self.insertms(c.ms, c.fpos + x, mkey, c.fpos)?;
                    c.ms = res.0;
                    c.fpos = res.1
                }
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
                _ => unreachable!(),
            },
        };
        ms.push(m0);
        Ok((ms, fpos))
    }

    // return whether this entry can be skipped.
    fn preprocess(&mut self, mut entry: Entry<K, V>) -> Option<Entry<K, V>> {
        self.stats.seqno = cmp::max(self.stats.seqno, entry.to_seqno());

        // if tombstone purge is configured, then purge.
        match self.config.tomb_purge {
            Some(before) => {
                if entry.purge_todo(Bound::Included(before)) {
                    None
                } else {
                    Some(entry)
                }
            }
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
    fpos: u64,
    thread: thread::JoinHandle<Result<()>>,
    tx: mpsc::SyncSender<(Vec<u8>, mpsc::SyncSender<Result<()>>)>,
}

impl Flusher {
    fn new(file: String, config: Config, reuse: bool) -> Result<Flusher> {
        let fd = util::open_file_w(&file, reuse)?;
        let fpos = if reuse {
            fs::metadata(&file)?.len()
        } else {
            Default::default()
        };

        let (tx, rx) = mpsc::sync_channel(config.flush_queue_size);
        let thread = thread::spawn(move || thread_flush(file, fd, rx));

        Ok(Flusher { tx, thread, fpos })
    }

    // return error if flush thread has exited/paniced.
    pub(crate) fn send(&mut self, block: Vec<u8>) -> Result<()> {
        let (tx, rx) = mpsc::sync_channel(0);
        self.tx.send((block, tx))?;
        rx.recv()?
    }

    // return the cause thread failure if there is a failure, or return
    // a known error like io::Error or PartialWrite.
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

fn thread_flush(
    file: String, // for debuging purpose
    mut fd: fs::File,
    rx: mpsc::Receiver<(Vec<u8>, mpsc::SyncSender<Result<()>>)>,
) -> Result<()> {
    let mut write_data = |data: &[u8]| -> Result<()> {
        let n = fd.write(data)?;
        if n == data.len() {
            Ok(())
        } else {
            let msg = format!("flusher: {:?} {}/{}...", &file, data.len(), n);
            Err(Error::PartialWrite(msg))
        }
    };

    for (data, tx) in rx.iter() {
        write_data(&data)?;
        tx.send(Ok(()))?;
    }
    // file descriptor and receiver channel shall be dropped.
    Ok(())
}
