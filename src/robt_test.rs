use rand::prelude::random;

use super::*;
use crate::core::Reader;
use crate::llrb::Llrb;
use crate::robt;
use crate::scans::SkipScan;

#[test]
fn test_stats() {
    let vlog_file: &ffi::OsStr = "robt-users-level-1.vlog".as_ref();

    let stats1 = Stats {
        z_blocksize: 16384,
        m_blocksize: 4096,
        v_blocksize: 65536,
        delta_ok: true,
        vlog_file: Some(vlog_file.to_os_string()),
        value_in_vlog: true,

        n_count: 1000000,
        n_deleted: 100,
        seqno: 2000000,
        key_mem: 128000000,
        diff_mem: 512000000,
        val_mem: 1024000000,
        z_bytes: 256000000,
        v_bytes: 2048000000,
        m_bytes: 256000000,
        padding: 100000000,
        n_abytes: 0,

        build_time: 10000000000000,
        epoch: 121345678998765,
    };
    let s = stats1.to_string();
    let stats2: Stats = s.parse().unwrap();
    assert!(stats1 == stats2);

    let vlog_file: &ffi::OsStr = "robt-users-level-1.vlog".as_ref();
    let cnf = Config {
        z_blocksize: 16384,
        m_blocksize: 4096,
        v_blocksize: 65536,
        delta_ok: true,
        vlog_file: Some(vlog_file.to_os_string()),
        value_in_vlog: true,
        tomb_purge: None,
        flush_queue_size: 1024,
    };
    let stats1: Stats = cnf.into();
    let s = stats1.to_string();
    let stats2: Stats = s.parse().unwrap();
    assert!(stats1 == stats2);
}

#[test]
fn test_meta_items() {
    use std::time::SystemTime;

    let dir = std::env::temp_dir().to_str().unwrap().to_string();
    fs::remove_file(dir.clone()).ok();
    let name = "users".to_string();
    let file = Config::stitch_index_file(&dir, &name);
    fs::write(&file, [1, 2, 3, 4, 5]).unwrap();

    let n: u64 = (SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .unwrap()
        .as_nanos()
        % (std::u64::MAX as u128))
        .try_into()
        .unwrap();
    let len1 = ROOT_MARKER.len();
    let stats = <Stats as Default>::default().to_string();
    let len2 = (n % 65536) as usize;
    let app_meta: Vec<u8> = (0..len2).map(|x| (x % 256) as u8).collect();
    let len3 = stats.len();

    let meta_items = vec![
        MetaItem::Root(5),
        MetaItem::AppMetadata(app_meta.clone()),
        MetaItem::Stats(stats.clone()),
        MetaItem::Marker(ROOT_MARKER.clone()),
    ];
    let n = write_meta_items(file, meta_items).unwrap();
    let ref_n = Config::compute_root_block(32 + len1 + len2 + len3);
    assert_eq!(n, ref_n as u64);

    let iter = read_meta_items(&dir, &name).unwrap().into_iter();
    for (i, item) in iter.enumerate() {
        match (i, item) {
            (0, MetaItem::Root(value)) => assert_eq!(value, 5),
            (1, MetaItem::AppMetadata(value)) => assert_eq!(value, app_meta),
            (2, MetaItem::Stats(value)) => assert_eq!(value, stats),
            (3, MetaItem::Marker(v)) => assert_eq!(v, ROOT_MARKER.clone()),
            (i, _) => panic!("at {}, failure", i),
        }
    }
}

#[test]
fn test_config() {
    let vlog_file: &ffi::OsStr = "same-file.log".as_ref();
    let mut config1 = Config {
        z_blocksize: 1024 * 4,
        v_blocksize: 1024 * 16,
        m_blocksize: 1024 * 32,
        tomb_purge: Some(543),
        delta_ok: true,
        vlog_file: Some(vlog_file.to_os_string()),
        value_in_vlog: true,
        flush_queue_size: 1234,
    };

    let stats: Stats = config1.clone().into();
    let config2: Config = stats.into();
    assert_eq!(config2.z_blocksize, config1.z_blocksize);
    assert_eq!(config2.v_blocksize, config1.v_blocksize);
    assert_eq!(config2.m_blocksize, config1.m_blocksize);
    assert_eq!(config2.delta_ok, config1.delta_ok);
    assert_eq!(config2.vlog_file, config1.vlog_file);
    assert_eq!(config2.value_in_vlog, config1.value_in_vlog);
    assert_eq!(config2.tomb_purge, None);
    assert_eq!(config2.flush_queue_size, Config::FLUSH_QUEUE_SIZE);

    config1.set_blocksize(1024 * 8, 1024 * 32, 1024 * 64);
    config1.set_tombstone_purge(782);
    config1.set_delta(None);
    config1.set_value_log(None);
    config1.set_flush_queue_size(1023);
    assert_eq!(config1.z_blocksize, 1024 * 8);
    assert_eq!(config1.v_blocksize, 1024 * 32);
    assert_eq!(config1.m_blocksize, 1024 * 64);
    assert_eq!(config1.delta_ok, false);
    assert_eq!(config1.value_in_vlog, false);
    assert_eq!(config1.tomb_purge, Some(782));
    assert_eq!(config1.flush_queue_size, 1023);

    assert_eq!(Config::compute_root_block(4095), 4096);
    assert_eq!(Config::compute_root_block(4096), 4096);
    assert_eq!(Config::compute_root_block(4097), 8192);
    let config: Config = Default::default();
    let dir_path = std::env::temp_dir();
    let dir: &ffi::OsStr = dir_path.as_ref();
    let ref_file: &ffi::OsStr = "/tmp/robt-users.indx".as_ref();
    assert_eq!(
        config.to_index_file(dir.to_str().unwrap(), "users"),
        ref_file.to_os_string()
    );
    let ref_file: &ffi::OsStr = "/tmp/robt-users.vlog".as_ref();
    assert_eq!(
        config.to_value_log(dir.to_str().unwrap(), "users").unwrap(),
        ref_file.to_os_string()
    );
}

#[test]
fn test_robt_llrb() {
    let lsm: bool = random();
    let mut llrb: Box<Llrb<i64, i64>> = if lsm {
        Llrb::new_lsm("test-llrb")
    } else {
        Llrb::new("test-llrb")
    };
    let n_ops = 6_000;
    let key_max = 2_000;
    for _i in 0..n_ops {
        let key = ((random::<i32>() as u32) % key_max) as i64;
        match random::<usize>() % 3 {
            0 => {
                let value: i64 = random();
                llrb.set(key, value).unwrap();
            }
            1 => {
                let value: i64 = random();
                let cas = match llrb.get(&key) {
                    Err(Error::KeyNotFound) => 0,
                    Err(_err) => unreachable!(),
                    Ok(e) => e.to_seqno(),
                };
                llrb.set_cas(key, value, cas).unwrap();
            }
            2 => {
                llrb.delete(&key).unwrap();
            }
            _ => unreachable!(),
        }
    }

    let dir = {
        let mut dir = std::env::temp_dir();
        dir.push("test-robt-build");
        dir.to_str().unwrap().to_string()
    };
    let mut config: robt::Config = Default::default();
    config.delta_ok = random();
    config.value_in_vlog = random();
    config.tomb_purge = match random::<u64>() % 100 {
        0..=60 => None,
        61..=70 => Some(0),
        71..=80 => Some(1),
        81..=90 => Some(random::<u64>() % n_ops),
        91..=100 => Some(10_000_000),
        _ => unreachable!(),
    };
    println!(
        "lsm:{} delta:{} vlog:{} purge:{:?}",
        lsm, config.delta_ok, config.value_in_vlog, config.tomb_purge
    );

    let iter = SkipScan::new(&llrb, ..);
    let refs: Vec<Entry<i64, i64>> = iter.map(|e| e.unwrap()).collect();

    let iter = SkipScan::new(&llrb, ..);
    let b = Builder::initial(&dir, "test-build", config.clone()).unwrap();
    let app_meta = "heloo world".to_string();
    b.build(iter, app_meta.as_bytes().to_vec()).unwrap();

    for e in refs.iter() {
        println!("{}", e.to_key());
    }
    println!("total entries {}", refs.len());

    //let snap = Robt::
    ////assert_eq!(stats
}

#[test]
fn test_robt_mvcc() {
    // panic!("TBD");
}