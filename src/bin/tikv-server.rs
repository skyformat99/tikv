// Copyright 2016 PingCAP, Inc.
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//     http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// See the License for the specific language governing permissions and
// limitations under the License.

#![feature(plugin)]
#![cfg_attr(feature = "dev", plugin(clippy))]
#![cfg_attr(not(feature = "dev"), allow(unknown_lints))]

#![allow(needless_pass_by_value)]

#[macro_use]
extern crate clap;
#[cfg(feature = "mem-profiling")]
extern crate jemallocator;
extern crate tikv;
#[macro_use]
extern crate log;
extern crate rocksdb;
extern crate mio;
extern crate toml;
extern crate libc;
extern crate fs2;
#[cfg(unix)]
extern crate signal;
#[cfg(unix)]
extern crate nix;
extern crate prometheus;
extern crate sys_info;
extern crate futures;
extern crate tokio_core;
#[cfg(test)]
extern crate tempdir;
extern crate grpcio as grpc;

mod signal_handler;
#[cfg(unix)]
mod profiling;

use std::process;
use std::fs::{self, File};
use std::usize;
use std::path::Path;
use std::sync::{Arc, mpsc};
use std::io::Read;
use std::time::Duration;
use std::env;

use clap::{Arg, App, ArgMatches};
use rocksdb::{DBOptions, ColumnFamilyOptions, BlockBasedOptions};
use fs2::FileExt;
use sys_info::{cpu_num, mem_info};

use tikv::storage::{TEMP_DIR, CF_DEFAULT, CF_LOCK, CF_WRITE, CF_RAFT};
use tikv::util::{self, panic_hook, rocksdb as rocksdb_util};
use tikv::util::collections::HashMap;
use tikv::util::logger::{self, StderrLogger};
use tikv::util::file_log::RotatingFileLogger;
use tikv::util::transport::SendCh;
use tikv::util::properties::{MvccPropertiesCollectorFactory, SizePropertiesCollectorFactory};
use tikv::server::{DEFAULT_LISTENING_ADDR, DEFAULT_CLUSTER_ID, Server, Node, Config,
                   create_raft_storage};
use tikv::server::transport::ServerRaftStoreRouter;
use tikv::server::resolve;
use tikv::raftstore::store::{self, SnapManager};
use tikv::pd::{RpcClient, PdClient};
use tikv::raftstore::store::keys::region_raft_prefix_len;
use tikv::util::time_monitor::TimeMonitor;

const KB: u64 = 1024;
const MB: u64 = 1024 * KB;
const GB: u64 = 1024 * MB;
const RAFTCF_MIN_MEM: u64 = 256 * MB;
const RAFTCF_MAX_MEM: u64 = 2 * GB;
const LOCKCF_MIN_MEM: u64 = 256 * MB;
const LOCKCF_MAX_MEM: u64 = GB;
// [default cf, write cf, raft cf, lock cf]
const DEFAULT_BLOCK_CACHE_RATIO: &'static [f64] = &[0.25, 0.15, 0.02, 0.02];
const SEC_TO_MS: i64 = 1000;

fn sanitize_memory_usage() -> bool {
    let mut ratio = 0.0;
    for v in DEFAULT_BLOCK_CACHE_RATIO {
        ratio = ratio + v;
    }
    if ratio > 1.0 {
        return false;
    }
    true
}

fn align_to_mb(n: u64) -> u64 {
    n & 0xFFFFFFFFFFF00000
}

fn exit_with_err(msg: String) -> ! {
    error!("{}", msg);
    process::exit(1)
}

fn get_flag_string(matches: &ArgMatches, name: &str) -> Option<String> {
    let s = matches.value_of(name);
    info!("flag {}: {:?}", name, s);

    s.map(|s| s.to_owned())
}

fn get_flag_int(matches: &ArgMatches, name: &str) -> Option<i64> {
    let i = matches.value_of(name).map(|x| {
        x.parse::<i64>()
            .or_else(|_| util::config::parse_readable_int(x))
            .unwrap_or_else(|e| exit_with_err(format!("parse {} failed: {:?}", name, e)))
    });
    info!("flag {}: {:?}", name, i);

    i
}

fn lookup<'a>(config: &'a toml::Value, key: &str) -> Option<&'a toml::Value> {
    let keys = key.split('.');
    let mut res = config;
    for key in keys {
        match res.get(key) {
            Some(v) => res = v,
            None => return None,
        }
    }
    Some(res)
}

fn get_toml_boolean(config: &toml::Value, name: &str, default: Option<bool>) -> bool {
    let b = match lookup(config, name) {
        Some(&toml::Value::Boolean(b)) => b,
        None => {
            info!("{} use default {:?}", name, default);
            default.unwrap_or_else(|| exit_with_err(format!("please specify {}", name)))
        }
        _ => exit_with_err(format!("{} boolean is excepted", name)),
    };
    info!("toml value {}: {:?}", name, b);

    b
}

fn get_toml_string(config: &toml::Value, name: &str, default: Option<String>) -> String {
    let s = match lookup(config, name) {
        Some(&toml::Value::String(ref s)) => s.clone(),
        None => {
            info!("{} use default {:?}", name, default);
            default.unwrap_or_else(|| exit_with_err(format!("please specify {}", name)))
        }
        _ => exit_with_err(format!("{} string is excepted", name)),
    };
    info!("toml value {}: {:?}", name, s);

    s
}

fn get_toml_string_opt(config: &toml::Value, name: &str) -> Option<String> {
    lookup(config, name)
        .and_then(|val| val.as_str())
        .map(|s| s.to_owned())
}

fn get_toml_int_opt(config: &toml::Value, name: &str) -> Option<i64> {
    let res = match lookup(config, name) {
        Some(&toml::Value::Integer(i)) => Some(i),
        Some(&toml::Value::String(ref s)) => {
            Some(util::config::parse_readable_int(s)
                .unwrap_or_else(|e| exit_with_err(format!("{} parse failed {:?}", name, e))))
        }
        None => None,
        _ => exit_with_err(format!("{} int or readable int is excepted", name)),
    };
    if let Some(i) = res {
        info!("toml value {} : {:?}", name, i);
    }
    res
}

fn get_toml_int(config: &toml::Value, name: &str, default: Option<i64>) -> i64 {
    get_toml_int_opt(config, name).unwrap_or_else(|| {
        let i = default.unwrap_or_else(|| exit_with_err(format!("please specify {}", name)));
        info!("{} use default {:?}", name, default);
        i
    })
}

fn get_toml_float_opt(config: &toml::Value, name: &str) -> Option<f64> {
    let res = match lookup(config, name) {
        Some(&toml::Value::Float(f)) => Some(f),
        Some(&toml::Value::String(ref s)) => {
            Some(s.parse()
                .unwrap_or_else(|e| exit_with_err(format!("{} parse failed {:?}", name, e))))
        }
        None => None,
        _ => exit_with_err(format!("{} float is excepted", name)),
    };
    if let Some(f) = res {
        info!("toml value {} : {:?}", name, f);
    }
    res
}

#[allow(absurd_extreme_comparisons)]
fn cfg_usize(target: &mut usize, config: &toml::Value, name: &str) -> bool {
    match get_toml_int_opt(config, name) {
        Some(i) => {
            assert!(i >= 0 && i as u64 <= usize::MAX as u64,
                    "{}: {} is invalid.",
                    name,
                    i);
            *target = i as usize;
            true
        }
        None => {
            info!("{} keep default {}", name, *target);
            false
        }
    }
}

fn cfg_u64(target: &mut u64, config: &toml::Value, name: &str) {
    match get_toml_int_opt(config, name) {
        Some(i) => {
            assert!(i >= 0, "{}: {} is invalid", name, i);
            *target = i as u64;
        }
        None => info!("{} keep default {}", name, *target),
    }
}

fn cfg_f64(target: &mut f64, config: &toml::Value, name: &str) {
    match get_toml_float_opt(config, name) {
        Some(f) => {
            assert!(f >= 0.0, "{}: {} is invalid", name, f);
            *target = f;
        }
        None => info!("{} keep default {}", name, *target),
    }
}

fn cfg_duration(target: &mut Duration, config: &toml::Value, name: &str) {
    match get_toml_int_opt(config, name) {
        Some(i) => {
            assert!(i >= 0);
            *target = Duration::from_millis(i as u64);
        }
        None => info!("{} keep default {:?}", name, *target),
    }
}

fn init_log(matches: &ArgMatches, config: &toml::Value) {
    let level = matches.value_of("log-level")
        .map(|s| s.to_owned())
        .or_else(|| get_toml_string_opt(config, "server.log-level"))
        .unwrap_or_else(|| "info".to_owned());

    let log_file_opt = matches.value_of("log-file")
        .map(|s| s.to_owned())
        .or_else(|| get_toml_string_opt(config, "server.log-file"));

    let level_filter = logger::get_level_by_string(&level);
    if let Some(log_file) = log_file_opt {
        let w = RotatingFileLogger::new(&log_file)
            .unwrap_or_else(|err| exit_with_err(format!("{:?}", err)));
        logger::init_log(w, level_filter).unwrap_or_else(|err| exit_with_err(format!("{:?}", err)));
    } else {
        let w = StderrLogger;
        logger::init_log(w, level_filter).unwrap_or_else(|err| exit_with_err(format!("{:?}", err)));
    }
}

fn initial_metric(config: &toml::Value, node_id: Option<u64>) {
    let push_interval = get_toml_int(config, "metric.interval", Some(0));
    if push_interval == 0 {
        return;
    }

    let push_address = get_toml_string(config, "metric.address", Some("".to_owned()));
    if push_address.is_empty() {
        return;
    }

    let mut push_job = get_toml_string(config, "metric.job", Some("tikv".to_owned()));
    if let Some(id) = node_id {
        push_job.push_str(&format!("_{}", id));
    }

    info!("start prometheus client");

    util::monitor_threads("tikv").unwrap_or_else(|err| exit_with_err(format!("{:?}", err)));

    util::run_prometheus(Duration::from_millis(push_interval as u64),
                         &push_address,
                         &push_job);
}

fn check_system_config(config: &toml::Value) {
    let max_open_files = get_toml_int(config, "rocksdb.max-open-files", Some(40960));
    if let Err(e) = util::config::check_max_open_fds(max_open_files as u64) {
        exit_with_err(format!("{:?}", e));
    }

    for e in util::config::check_kernel() {
        warn!("{:?}", e);
    }

    if !cfg!(windows) && env::var("TZ").is_err() {
        env::set_var("TZ", "/etc/localtime");
        warn!("environment variable `TZ` is missing, use `/etc/localtime`");
    }
}

fn check_advertise_address(addr: &str) {
    if let Err(e) = util::config::check_addr(addr) {
        exit_with_err(format!("{:?}", e));
    }

    // FIXME: Forbidden addresses ending in 0 or 255? Those are not always invalid.
    // See more: https://en.wikipedia.org/wiki/IPv4#Addresses_ending_in_0_or_255
    let invalid_patterns = [("0.", 0)]; // Current network is not allowed.

    for &(pat, pos) in &invalid_patterns {
        if let Some(idx) = addr.find(pat) {
            if pos == idx {
                exit_with_err(format!("invalid advertise-addr: {:?}", addr));
            }
        }
    }
}

fn get_rocksdb_db_option(config: &toml::Value) -> DBOptions {
    let mut opts = DBOptions::new();
    let rmode = get_toml_int(config, "rocksdb.wal-recovery-mode", Some(2));
    let wal_recovery_mode = util::config::parse_rocksdb_wal_recovery_mode(rmode)
        .unwrap_or_else(|err| exit_with_err(format!("{:?}", err)));
    opts.set_wal_recovery_mode(wal_recovery_mode);

    let wal_dir = get_toml_string(config, "rocksdb.wal-dir", Some("".to_owned()));
    if !wal_dir.is_empty() {
        opts.set_wal_dir(&wal_dir)
    };

    let wal_ttl_seconds = get_toml_int(config, "rocksdb.wal-ttl-seconds", Some(0));
    opts.set_wal_ttl_seconds(wal_ttl_seconds as u64);

    let wal_size_limit = get_toml_int(config, "rocksdb.wal-size-limit", Some(0));
    // return size in MB
    let wal_size_limit_mb = align_to_mb(wal_size_limit as u64) / MB;
    opts.set_wal_size_limit_mb(wal_size_limit_mb as u64);

    let max_total_wal_size = get_toml_int(config,
                                          "rocksdb.max-total-wal-size",
                                          Some(4 * 1024 * 1024 * 1024));
    opts.set_max_total_wal_size(max_total_wal_size as u64);

    let max_background_jobs = get_toml_int(config, "rocksdb.max-background-jobs", Some(6));
    opts.set_max_background_jobs(max_background_jobs as i32);

    let max_manifest_file_size = get_toml_int(config,
                                              "rocksdb.max-manifest-file-size",
                                              Some(20 * 1024 * 1024));
    opts.set_max_manifest_file_size(max_manifest_file_size as u64);

    let create_if_missing = get_toml_boolean(config, "rocksdb.create-if-missing", Some(true));
    opts.create_if_missing(create_if_missing);

    let max_open_files = get_toml_int(config, "rocksdb.max-open-files", Some(40960));
    opts.set_max_open_files(max_open_files as i32);

    let enable_statistics = get_toml_boolean(config, "rocksdb.enable-statistics", Some(true));
    if enable_statistics {
        opts.enable_statistics();
        let stats_dump_period_sec =
            get_toml_int(config, "rocksdb.stats-dump-period-sec", Some(600));
        opts.set_stats_dump_period_sec(stats_dump_period_sec as usize);
    }

    let compaction_readahead_size =
        get_toml_int(config, "rocksdb.compaction-readahead-size", Some(0));
    opts.set_compaction_readahead_size(compaction_readahead_size as u64);

    let max_file_size = get_toml_int(config, "rocksdb.info-log-max-size", Some(0));
    opts.set_max_log_file_size(max_file_size as u64);

    // RocksDB needs seconds, but here we will get milliseconds.
    let roll_time_secs = get_toml_int(config, "rocksdb.info-log-roll-time", Some(0)) / SEC_TO_MS;
    opts.set_log_file_time_to_roll(roll_time_secs as u64);

    let info_log_dir = get_toml_string(config, "rocksdb.info-log-dir", Some("".to_owned()));
    if !info_log_dir.is_empty() {
        opts.create_info_log(&info_log_dir).unwrap_or_else(|e| {
            panic!("create RocksDB info log {} error {:?}", info_log_dir, e);
        })
    }

    let rate_bytes_per_sec = get_toml_int(config, "rocksdb.rate-bytes-per-sec", Some(0));
    if rate_bytes_per_sec > 0 {
        opts.set_ratelimiter(rate_bytes_per_sec as i64);
    }

    let max_sub_compactions = get_toml_int(config, "rocksdb.max-sub-compactions", Some(1));
    opts.set_max_subcompactions(max_sub_compactions as u32);

    let writable_file_max_buffer_size = get_toml_int(config,
                                                     "rocksdb.writable-file-max-buffer-size",
                                                     Some(1024 * 1024));
    opts.set_writable_file_max_buffer_size(writable_file_max_buffer_size as i32);

    let direct_io = get_toml_boolean(config,
                                     "rocksdb.use-direct-io-for-flush-and-compaction",
                                     Some(false));
    opts.set_use_direct_io_for_flush_and_compaction(direct_io);

    let pipelined_write = get_toml_boolean(config, "rocksdb.enable-pipelined-write", Some(true));
    opts.enable_pipelined_write(pipelined_write);

    opts
}

struct CfOptValues {
    pub block_size: i64,
    pub block_cache_size: i64,
    pub cache_index_and_filter_blocks: bool,
    pub use_bloom_filter: bool,
    pub whole_key_filtering: bool,
    pub bloom_bits_per_key: i64,
    pub block_based_filter: bool,
    pub compression_per_level: String,
    pub write_buffer_size: i64,
    pub max_write_buffer_number: i64,
    pub min_write_buffer_number_to_merge: i64,
    pub max_bytes_for_level_base: i64,
    pub target_file_size_base: i64,
    pub level_zero_file_num_compaction_trigger: i64,
    pub level_zero_slowdown_writes_trigger: i64,
    pub level_zero_stop_writes_trigger: i64,
    pub max_compaction_bytes: i64,
    pub compaction_pri: i64,
}

impl Default for CfOptValues {
    fn default() -> CfOptValues {
        CfOptValues {
            block_size: 64 * KB as i64,
            block_cache_size: 256 * MB as i64,
            cache_index_and_filter_blocks: true,
            use_bloom_filter: false,
            whole_key_filtering: true,
            bloom_bits_per_key: 10,
            block_based_filter: false,
            compression_per_level: String::from("no:no:lz4:lz4:lz4:zstd:zstd"),
            write_buffer_size: 128 * MB as i64,
            max_write_buffer_number: 5,
            min_write_buffer_number_to_merge: 1,
            max_bytes_for_level_base: 512 * MB as i64,
            target_file_size_base: 32 * MB as i64,
            level_zero_file_num_compaction_trigger: 4,
            level_zero_slowdown_writes_trigger: 20,
            level_zero_stop_writes_trigger: 36,
            max_compaction_bytes: 2 * GB as i64,
            compaction_pri: 0,
        }
    }
}

fn get_rocksdb_cf_option(config: &toml::Value,
                         cf: &str,
                         default_values: CfOptValues)
                         -> ColumnFamilyOptions {
    let prefix = String::from("rocksdb.") + cf + ".";
    let mut block_base_opts = BlockBasedOptions::new();
    let block_size = get_toml_int(config,
                                  (prefix.clone() + "block-size").as_str(),
                                  Some(default_values.block_size));
    block_base_opts.set_block_size(block_size as usize);
    let block_cache_size = get_toml_int(config,
                                        (prefix.clone() + "block-cache-size").as_str(),
                                        Some(default_values.block_cache_size));
    block_base_opts.set_lru_cache(block_cache_size as usize);

    let cache_index_and_filter =
        get_toml_boolean(config,
                         (prefix.clone() + "cache-index-and-filter-blocks").as_str(),
                         Some(default_values.cache_index_and_filter_blocks));
    block_base_opts.set_cache_index_and_filter_blocks(cache_index_and_filter);

    if default_values.use_bloom_filter {
        let bloom_bits_per_key = get_toml_int(config,
                                              (prefix.clone() + "bloom-filter-bits-per-key")
                                                  .as_str(),
                                              Some(default_values.bloom_bits_per_key));
        let block_based_filter = get_toml_boolean(config,
                                                  (prefix.clone() + "block-based-bloom-filter")
                                                      .as_str(),
                                                  Some(default_values.block_based_filter));
        block_base_opts.set_bloom_filter(bloom_bits_per_key as i32, block_based_filter);

        block_base_opts.set_whole_key_filtering(default_values.whole_key_filtering);
    }
    let mut cf_opts = ColumnFamilyOptions::new();
    cf_opts.set_block_based_table_factory(&block_base_opts);

    let cpl = get_toml_string(config,
                              (prefix.clone() + "compression-per-level").as_str(),
                              Some(default_values.compression_per_level.clone()));
    let per_level_compression = util::config::parse_rocksdb_per_level_compression(&cpl)
        .unwrap_or_else(|err| exit_with_err(format!("{:?}", err)));
    cf_opts.compression_per_level(&per_level_compression);

    let write_buffer_size = get_toml_int(config,
                                         (prefix.clone() + "write-buffer-size").as_str(),
                                         Some(default_values.write_buffer_size));
    cf_opts.set_write_buffer_size(write_buffer_size as u64);

    let max_write_buffer_number = get_toml_int(config,
                                               (prefix.clone() + "max-write-buffer-number")
                                                   .as_str(),
                                               Some(default_values.max_write_buffer_number));
    cf_opts.set_max_write_buffer_number(max_write_buffer_number as i32);

    let min_write_buffer_number_to_merge =
        get_toml_int(config,
                     (prefix.clone() + "min-write-buffer-number-to-merge").as_str(),
                     Some(default_values.min_write_buffer_number_to_merge));
    cf_opts.set_min_write_buffer_number_to_merge(min_write_buffer_number_to_merge as i32);

    let max_bytes_for_level_base = get_toml_int(config,
                                                (prefix.clone() + "max-bytes-for-level-base")
                                                    .as_str(),
                                                Some(default_values.max_bytes_for_level_base));
    cf_opts.set_max_bytes_for_level_base(max_bytes_for_level_base as u64);

    let target_file_size_base = get_toml_int(config,
                                             (prefix.clone() + "target-file-size-base").as_str(),
                                             Some(default_values.target_file_size_base));
    cf_opts.set_target_file_size_base(target_file_size_base as u64);

    let level_zero_file_num_compaction_trigger =
        get_toml_int(config,
                     (prefix.clone() + "level0-file-num-compaction-trigger").as_str(),
                     Some(default_values.level_zero_file_num_compaction_trigger));
    cf_opts.set_level_zero_file_num_compaction_trigger(
        level_zero_file_num_compaction_trigger as i32);

    let level_zero_slowdown_writes_trigger =
        get_toml_int(config,
                     (prefix.clone() + "level0-slowdown-writes-trigger").as_str(),
                     Some(default_values.level_zero_slowdown_writes_trigger));
    cf_opts.set_level_zero_slowdown_writes_trigger(level_zero_slowdown_writes_trigger as i32);

    let level_zero_stop_writes_trigger =
        get_toml_int(config,
                     (prefix.clone() + "level0-stop-writes-trigger").as_str(),
                     Some(default_values.level_zero_stop_writes_trigger));
    cf_opts.set_level_zero_stop_writes_trigger(level_zero_stop_writes_trigger as i32);

    let max_compaction_bytes = get_toml_int(config,
                                            (prefix.clone() + "max-compaction-bytes").as_str(),
                                            Some(default_values.max_compaction_bytes));
    cf_opts.set_max_compaction_bytes(max_compaction_bytes as u64);

    let priority = get_toml_int(config,
                                (prefix.clone() + "compaction-pri").as_str(),
                                Some(0));
    let compaction_priority = util::config::parse_rocksdb_compaction_pri(priority)
        .unwrap_or_else(|err| exit_with_err(format!("{:?}", err)));
    cf_opts.compaction_priority(compaction_priority);

    cf_opts
}

fn get_rocksdb_default_cf_option(config: &toml::Value, total_mem: u64) -> ColumnFamilyOptions {
    let mut default_values = CfOptValues::default();
    default_values.block_cache_size =
        align_to_mb((total_mem as f64 * DEFAULT_BLOCK_CACHE_RATIO[0]) as u64) as i64;
    default_values.use_bloom_filter = true;
    default_values.whole_key_filtering = true;
    default_values.compaction_pri = 3;

    let mut cf_opts = get_rocksdb_cf_option(config, "defaultcf", default_values);
    let f = Box::new(SizePropertiesCollectorFactory::default());
    cf_opts.add_table_properties_collector_factory("tikv.size-properties-collector", f);
    cf_opts
}

fn get_rocksdb_write_cf_option(config: &toml::Value, total_mem: u64) -> ColumnFamilyOptions {
    let mut default_values = CfOptValues::default();
    default_values.block_cache_size =
        align_to_mb((total_mem as f64 * DEFAULT_BLOCK_CACHE_RATIO[1]) as u64) as i64;
    default_values.use_bloom_filter = true;
    default_values.whole_key_filtering = false;
    default_values.compaction_pri = 3;

    let mut cf_opts = get_rocksdb_cf_option(config, "writecf", default_values);
    // Prefix extractor(trim the timestamp at tail) for write cf.
    cf_opts.set_prefix_extractor("FixedSuffixSliceTransform",
                              Box::new(rocksdb_util::FixedSuffixSliceTransform::new(8)))
        .unwrap_or_else(|err| exit_with_err(format!("{:?}", err)));
    // Create prefix bloom filter for memtable.
    cf_opts.set_memtable_prefix_bloom_size_ratio(0.1 as f64);
    // Collects user defined properties.
    let f = Box::new(MvccPropertiesCollectorFactory::default());
    cf_opts.add_table_properties_collector_factory("tikv.mvcc-properties-collector", f);
    let f = Box::new(SizePropertiesCollectorFactory::default());
    cf_opts.add_table_properties_collector_factory("tikv.size-properties-collector", f);
    cf_opts
}

fn get_rocksdb_raftlog_cf_option(config: &toml::Value, total_mem: u64) -> ColumnFamilyOptions {
    let cache_size = align_to_mb((total_mem as f64 * DEFAULT_BLOCK_CACHE_RATIO[2]) as u64);
    let block_cache_size = adjust_block_cache_size(cache_size, RAFTCF_MIN_MEM, RAFTCF_MAX_MEM);
    let mut default_values = CfOptValues::default();
    default_values.block_cache_size = block_cache_size as i64;

    let mut cf_opts = get_rocksdb_cf_option(config, "raftcf", default_values);
    cf_opts.set_memtable_insert_hint_prefix_extractor("RaftPrefixSliceTransform",
            Box::new(rocksdb_util::FixedPrefixSliceTransform::new(region_raft_prefix_len())))
        .unwrap_or_else(|err| exit_with_err(format!("{:?}", err)));
    cf_opts
}

fn get_rocksdb_lock_cf_option(config: &toml::Value, total_mem: u64) -> ColumnFamilyOptions {
    let cache_size = align_to_mb((total_mem as f64 * DEFAULT_BLOCK_CACHE_RATIO[3]) as u64);
    let block_cache_size = adjust_block_cache_size(cache_size, LOCKCF_MIN_MEM, LOCKCF_MAX_MEM);
    let mut default_values = CfOptValues::default();
    default_values.block_cache_size = block_cache_size as i64;
    default_values.block_size = 16 * KB as i64;
    default_values.use_bloom_filter = true;
    default_values.whole_key_filtering = true;
    default_values.compression_per_level = String::from("no:no:no:no:no:no:no");
    default_values.level_zero_file_num_compaction_trigger = 1;
    default_values.max_bytes_for_level_base = 128 * MB as i64;

    let mut cf_opts = get_rocksdb_cf_option(config, "lockcf", default_values);
    // Currently if we want create bloom filter for memtable, we must set prefix extractor.
    cf_opts.set_prefix_extractor("NoopSliceTransform",
                              Box::new(rocksdb_util::NoopSliceTransform))
        .unwrap_or_else(|err| exit_with_err(format!("{:?}", err)));
    cf_opts.set_memtable_prefix_bloom_size_ratio(0.1 as f64);
    cf_opts
}

fn adjust_block_cache_size(cache_size: u64, min_limit: u64, max_limit: u64) -> u64 {
    if cache_size < min_limit {
        return min_limit;
    }
    if cache_size > max_limit {
        return max_limit;
    }
    cache_size
}

fn adjust_end_points_by_cpu_num(total_cpu_num: usize) -> usize {
    if total_cpu_num >= 8 {
        (total_cpu_num as f32 * 0.8) as usize
    } else {
        4
    }
}

// TODO: merge this function with Config::new
// Currently, to add a new option, we will define three default value
// in config.rs, this file and config-template.toml respectively. It may be more
// maintainable to keep things in one place.
fn build_cfg(matches: &ArgMatches,
             config: &toml::Value,
             cluster_id: u64,
             addr: String,
             total_cpu_num: usize)
             -> Config {
    let mut cfg = Config::new();
    cfg.cluster_id = cluster_id;
    cfg.addr = addr.to_owned();
    cfg_usize(&mut cfg.notify_capacity, config, "server.notify-capacity");
    cfg_usize(&mut cfg.grpc_concurrency, config, "server.grpc-concurrency");
    cfg_usize(&mut cfg.grpc_concurrent_stream,
              config,
              "server.grpc-concurrent-stream");
    cfg_usize(&mut cfg.grpc_raft_conn_num,
              config,
              "server.grpc-raft-conn-num");
    cfg_usize(&mut cfg.grpc_stream_initial_window_size,
              config,
              "server.grpc-stream-initial-window-size");
    if !cfg_usize(&mut cfg.end_point_concurrency,
                  config,
                  "server.end-point-concurrency") {
        cfg.end_point_concurrency = adjust_end_points_by_cpu_num(total_cpu_num);
    }

    cfg_usize(&mut cfg.messages_per_tick,
              config,
              "server.messages-per-tick");
    let capacity = get_flag_int(matches, "capacity")
        .or_else(|| get_toml_int_opt(config, "server.capacity"));
    if let Some(cap) = capacity {
        assert!(cap >= 0);
        cfg.raft_store.capacity = cap as u64;
    }

    // Set advertise address for outer node and client use.
    // If no advertise listening address set, use the associated listening address.
    cfg.advertise_addr = get_flag_string(matches, "advertise-addr")
        .unwrap_or_else(|| get_toml_string(config, "server.advertise-addr", Some(addr.to_owned())));
    check_advertise_address(&cfg.advertise_addr);

    cfg.raft_store.sync_log = get_toml_boolean(config, "raftstore.sync-log", Some(true));
    cfg_usize(&mut cfg.raft_store.notify_capacity,
              config,
              "raftstore.notify-capacity");
    cfg_usize(&mut cfg.raft_store.messages_per_tick,
              config,
              "raftstore.messages-per-tick");
    cfg_u64(&mut cfg.raft_store.raft_base_tick_interval,
            config,
            "raftstore.raft-base-tick-interval");
    cfg_usize(&mut cfg.raft_store.raft_heartbeat_ticks,
              config,
              "raftstore.raft-heartbeat-ticks");
    if cfg_usize(&mut cfg.raft_store.raft_election_timeout_ticks,
                 config,
                 "raftstore.raft-election-timeout-ticks") {
        warn!("Election timeout ticks needs to be same across all the cluster, otherwise it may \
               lead to inconsistency.");
    }
    cfg_u64(&mut cfg.raft_store.split_region_check_tick_interval,
            config,
            "raftstore.split-region-check-tick-interval");
    cfg_u64(&mut cfg.raft_store.region_split_size,
            config,
            "raftstore.region-split-size");
    cfg_u64(&mut cfg.raft_store.region_max_size,
            config,
            "raftstore.region-max-size");
    cfg_u64(&mut cfg.raft_store.region_check_size_diff,
            config,
            "raftstore.region-split-check-diff");
    cfg_u64(&mut cfg.raft_store.raft_log_gc_tick_interval,
            config,
            "raftstore.raft-log-gc-tick-interval");
    cfg_u64(&mut cfg.raft_store.raft_log_gc_threshold,
            config,
            "raftstore.raft-log-gc-threshold");
    cfg_u64(&mut cfg.raft_store.raft_log_gc_count_limit,
            config,
            "raftstore.raft-log-gc-count-limit");
    cfg_u64(&mut cfg.raft_store.raft_log_gc_size_limit,
            config,
            "raftstore.raft-log-gc-size-limit");
    cfg_u64(&mut cfg.raft_store.region_compact_check_interval,
            config,
            "raftstore.region-compact-check-interval");
    cfg_u64(&mut cfg.raft_store.region_compact_delete_keys_count,
            config,
            "raftstore.region-compact-delete-keys-count");
    cfg_u64(&mut cfg.raft_store.lock_cf_compact_interval,
            config,
            "raftstore.lock-cf-compact-interval");
    cfg_u64(&mut cfg.raft_store.lock_cf_compact_bytes_threshold,
            config,
            "raftstore.lock-cf-compact-bytes-threshold");
    cfg_u64(&mut cfg.raft_store.raft_entry_max_size,
            config,
            "raftstore.raft-entry-max-size");
    cfg_duration(&mut cfg.raft_store.max_peer_down_duration,
                 config,
                 "raftstore.max-peer-down-duration");
    cfg_u64(&mut cfg.raft_store.pd_heartbeat_tick_interval,
            config,
            "raftstore.pd-heartbeat-tick-interval");
    cfg_u64(&mut cfg.raft_store.pd_store_heartbeat_tick_interval,
            config,
            "raftstore.pd-store-heartbeat-tick-interval");
    cfg_u64(&mut cfg.raft_store.consistency_check_tick_interval,
            config,
            "raftstore.consistency-check-interval");
    cfg.raft_store.use_sst_file_snapshot =
        get_toml_boolean(config, "raftstore.use-sst-file-snapshot", Some(true));
    cfg_f64(&mut cfg.storage.gc_ratio_threshold,
            config,
            "storage.gc-ratio-threshold");
    cfg_usize(&mut cfg.storage.scheduler_notify_capacity,
              config,
              "storage.scheduler-notify-capacity");
    cfg_usize(&mut cfg.storage.scheduler_message_per_tick,
              config,
              "storage.scheduler-messages-per-tick");
    cfg_usize(&mut cfg.storage.scheduler_concurrency,
              config,
              "storage.scheduler-concurrency");
    cfg_usize(&mut cfg.storage.scheduler_worker_pool_size,
              config,
              "storage.scheduler-worker-pool-size");
    cfg_usize(&mut cfg.storage.scheduler_too_busy_threshold,
              config,
              "storage.scheduler-too-busy-threshold");

    cfg
}

fn canonicalize_path(path: &str) -> String {
    let p = Path::new(path);
    if p.exists() && p.is_file() {
        exit_with_err(format!("{} is not a directory!", path));
    }
    if !p.exists() {
        fs::create_dir_all(p).unwrap_or_else(|err| exit_with_err(format!("{:?}", err)));
    }
    format!("{}",
            p.canonicalize().unwrap_or_else(|err| exit_with_err(format!("{:?}", err))).display())
}

fn get_data_and_backup_dirs(matches: &ArgMatches, config: &toml::Value) -> (String, String) {
    // store data path
    let abs_data_dir = matches.value_of("data-dir")
        .map(|s| s.to_owned())
        .or_else(|| get_toml_string_opt(config, "server.data-dir"))
        .or_else(|| get_toml_string_opt(config, "server.store"))
        .map(|s| canonicalize_path(&s))
        .unwrap_or_else(|| {
            warn!("data dir parsing failed, use default data dir {}", TEMP_DIR);
            TEMP_DIR.to_owned()
        });
    info!("server.data-dir uses {:?}", abs_data_dir);

    // Backup path
    let mut backup_dir = get_toml_string_opt(config, "server.backup-dir")
        .or_else(|| get_toml_string_opt(config, "server.backup"))
        .unwrap_or_default();
    if backup_dir.is_empty() && abs_data_dir != TEMP_DIR {
        backup_dir = format!("{}", Path::new(&abs_data_dir).join("backup").display())
    }

    if backup_dir.is_empty() {
        info!("empty backup path, backup is disabled");
        (abs_data_dir, backup_dir)
    } else {
        let abs_backup_dir = canonicalize_path(&backup_dir);
        info!("server.backup-dir uses {:?}", abs_backup_dir);
        (abs_data_dir, abs_backup_dir)
    }
}

fn get_store_labels(matches: &ArgMatches, config: &toml::Value) -> HashMap<String, String> {
    let labels = matches.value_of("labels")
        .map(|s| s.to_owned())
        .or_else(|| get_toml_string_opt(config, "server.labels"))
        .unwrap_or_default();
    util::config::parse_store_labels(&labels)
        .unwrap_or_else(|err| exit_with_err(format!("{:?}", err)))
}

fn run_raft_server(pd_client: RpcClient,
                   cfg: Config,
                   backup_path: &str,
                   config: &toml::Value,
                   total_mem: u64) {
    info!("tikv server config: {:?}", cfg);

    let store_path = Path::new(&cfg.storage.data_dir);
    let lock_path = store_path.join(Path::new("LOCK"));
    let db_path = store_path.join(Path::new("db"));
    let snap_path = store_path.join(Path::new("snap"));

    let f = File::create(lock_path).unwrap_or_else(|err| exit_with_err(format!("{:?}", err)));
    if f.try_lock_exclusive().is_err() {
        panic!("lock {:?} failed, maybe another instance is using this directory.",
               store_path);
    }

    // Initialize raftstore channels.
    let mut event_loop = store::create_event_loop(&cfg.raft_store)
        .unwrap_or_else(|err| exit_with_err(format!("{:?}", err)));
    let store_sendch = SendCh::new(event_loop.channel(), "raftstore");
    let raft_router = ServerRaftStoreRouter::new(store_sendch.clone());
    let (snap_status_sender, snap_status_receiver) = mpsc::channel();

    // Create engine, storage.
    let opts = get_rocksdb_db_option(config);
    let cfs_opts =
        vec![rocksdb_util::CFOptions::new(CF_DEFAULT,
                                          get_rocksdb_default_cf_option(config, total_mem)),
             rocksdb_util::CFOptions::new(CF_LOCK, get_rocksdb_lock_cf_option(config, total_mem)),
             rocksdb_util::CFOptions::new(CF_WRITE,
                                          get_rocksdb_write_cf_option(config, total_mem)),
             rocksdb_util::CFOptions::new(CF_RAFT,
                                          get_rocksdb_raftlog_cf_option(config, total_mem))];
    let engine = Arc::new(rocksdb_util::new_engine_opt(db_path.to_str()
                                                           .unwrap(),
                                                       opts,
                                                       cfs_opts)
        .unwrap_or_else(|err| exit_with_err(format!("{:?}", err))));
    let mut storage = create_raft_storage(raft_router.clone(), engine.clone(), &cfg)
        .unwrap_or_else(|err| exit_with_err(format!("{:?}", err)));

    // Create pd client, snapshot manager, server.
    let pd_client = Arc::new(pd_client);
    let (mut worker, resolver) = resolve::new_resolver(pd_client.clone())
        .unwrap_or_else(|err| exit_with_err(format!("{:?}", err)));
    let snap_mgr = SnapManager::new(snap_path.as_path().to_str().unwrap().to_owned(),
                                    Some(store_sendch),
                                    cfg.raft_store.use_sst_file_snapshot);
    let mut server = Server::new(&cfg,
                                 storage.clone(),
                                 raft_router,
                                 snap_status_sender,
                                 resolver,
                                 snap_mgr.clone())
        .unwrap_or_else(|err| exit_with_err(format!("{:?}", err)));
    let trans = server.transport();

    // Create node.
    let mut node = Node::new(&mut event_loop, &cfg, pd_client);
    node.start(event_loop,
               engine.clone(),
               trans,
               snap_mgr,
               snap_status_receiver)
        .unwrap_or_else(|err| exit_with_err(format!("{:?}", err)));
    initial_metric(config, Some(node.id()));

    // Start storage.
    info!("start storage");
    if let Err(e) = storage.start(&cfg.storage) {
        panic!("failed to start storage, error = {:?}", e);
    }

    // Run server.
    server.start(&cfg).unwrap_or_else(|err| exit_with_err(format!("{:?}", err)));
    signal_handler::handle_signal(engine, backup_path);

    // Stop.
    server.stop().unwrap_or_else(|err| exit_with_err(format!("{:?}", err)));
    node.stop().unwrap_or_else(|err| exit_with_err(format!("{:?}", err)));
    if let Some(Err(e)) = worker.stop().map(|h| h.join()) {
        info!("ignore failure when stopping resolver: {:?}", e);
    }
}

fn main() {
    let long_version: String = {
        let (hash, time, rust_ver) = util::build_info();
        format!("{}\nGit Commit Hash: {}\nUTC Build Time:  {}\nRust Version:    {}",
                crate_version!(),
                hash,
                time,
                rust_ver)
    };
    let matches = App::new("TiKV")
        .long_version(long_version.as_ref())
        .author("PingCAP Inc. <info@pingcap.com>")
        .about("A Distributed transactional key-value database powered by Rust and Raft")
        .arg(Arg::with_name("config")
            .short("C")
            .long("config")
            .value_name("FILE")
            .help("Sets config file")
            .takes_value(true))
        .arg(Arg::with_name("addr")
            .short("A")
            .long("addr")
            .takes_value(true)
            .value_name("IP:PORT")
            .help("Sets listening address"))
        .arg(Arg::with_name("advertise-addr")
            .long("advertise-addr")
            .takes_value(true)
            .value_name("IP:PORT")
            .help("Sets advertise listening address for client communication"))
        .arg(Arg::with_name("log-level")
            .short("L")
            .long("log-level")
            .alias("log")
            .takes_value(true)
            .value_name("LEVEL")
            .possible_values(&["trace", "debug", "info", "warn", "error", "off"])
            .help("Sets log level"))
        .arg(Arg::with_name("log-file")
            .short("f")
            .long("log-file")
            .takes_value(true)
            .value_name("FILE")
            .help("Sets log file")
            .long_help("Sets log file. If not set, output log to stderr"))
        .arg(Arg::with_name("data-dir")
            .long("data-dir")
            .short("s")
            .alias("store")
            .takes_value(true)
            .value_name("PATH")
            .help("Sets the path to store directory"))
        .arg(Arg::with_name("capacity")
            .long("capacity")
            .takes_value(true)
            .value_name("CAPACITY")
            .help("Sets the store capacity")
            .long_help("Sets the store capacity. If not set, use entire partition"))
        .arg(Arg::with_name("pd-endpoints")
            .long("pd-endpoints")
            .aliases(&["pd", "pd-endpoint"])
            .takes_value(true)
            .value_name("PD_URL")
            .multiple(true)
            .use_delimiter(true)
            .require_delimiter(true)
            .value_delimiter(",")
            .help("Sets PD endpoints")
            .long_help("Sets PD endpoints. Uses `,` to separate multiple PDs"))
        .arg(Arg::with_name("labels")
            .long("labels")
            .alias("label")
            .takes_value(true)
            .value_name("KEY=VALUE")
            .multiple(true)
            .use_delimiter(true)
            .require_delimiter(true)
            .value_delimiter(",")
            .help("Sets server labels")
            .long_help("Sets server labels. Uses `,` to separate kv pairs, like \
                        `zone=cn,disk=ssd`"))
        .get_matches();

    let config = match matches.value_of("config") {
        Some(path) => {
            let mut config_file = File::open(&path).expect("config open failed");
            let mut s = String::new();
            config_file.read_to_string(&mut s).expect("config read failed");
            s.parse().expect("malformed config file")
        }
        // Default empty value, lookup() always returns `None`.
        None => toml::Value::Integer(0),
    };

    init_log(&matches, &config);

    // Print version information.
    util::print_tikv_info();

    panic_hook::set_exit_hook();

    // Before any startup, check system configuration.
    check_system_config(&config);

    let addr = matches.value_of("addr")
        .map(|s| s.to_owned())
        .or_else(|| get_toml_string_opt(&config, "server.addr"))
        .unwrap_or_else(|| DEFAULT_LISTENING_ADDR.to_owned());

    if let Err(e) = util::config::check_addr(&addr) {
        exit_with_err(format!("{:?}", e));
    }

    let pd_endpoints: Vec<_> = matches.values_of("pd-endpoints")
        .map(|s| s.map(|s| s.to_owned()).collect())
        .or_else(|| {
            get_toml_string_opt(&config, "pd.endpoints").map(|s| {
                s.split(',')
                    .map(|s| s.to_owned())
                    .collect()
            })
        })
        .expect("empty pd endpoints");

    for addr in &pd_endpoints {
        if let Err(e) = util::config::check_addr(addr) {
            panic!("{:?}", e);
        }
    }

    let pd_client = RpcClient::new(&pd_endpoints)
        .unwrap_or_else(|err| exit_with_err(format!("{:?}", err)));
    let cluster_id = pd_client.get_cluster_id()
        .unwrap_or_else(|err| exit_with_err(format!("{:?}", err)));
    info!("connect to PD cluster {}", cluster_id);

    let total_cpu_num = cpu_num().unwrap_or_else(|err| exit_with_err(format!("{:?}", err)));
    // return memory in KB.
    let mem = mem_info().unwrap_or_else(|err| exit_with_err(format!("{:?}", err)));
    let total_mem = mem.total * KB;
    if !sanitize_memory_usage() {
        panic!("default block cache size over total memory.");
    }

    let mut cfg = build_cfg(&matches, &config, cluster_id, addr, total_cpu_num as usize);
    cfg.labels = get_store_labels(&matches, &config);
    let (store_path, backup_path) = get_data_and_backup_dirs(&matches, &config);
    cfg.storage.data_dir = store_path;

    if cluster_id == DEFAULT_CLUSTER_ID {
        panic!("in raftkv, cluster_id must greater than 0");
    }
    let _m = TimeMonitor::default();
    run_raft_server(pd_client, cfg, &backup_path, &config, total_mem);
}

#[cfg(test)]
mod tests {
    use toml::Value;

    #[test]
    fn test_lookup() {
        let value = r#"
            foo = 'bar'
            [rocksdb]
            compaction-readahead-size = 0
            [rocksdb.defaultcf]
            compression-per-level = "no"
            "#
            .parse()
            .unwrap();

        let queries = vec![
            ("foo", Value::String("bar".to_owned())),
            ("rocksdb.compaction-readahead-size", Value::Integer(0)),
            ("rocksdb.defaultcf.compression-per-level", Value::String("no".to_owned())),
        ];
        for (key, exp) in queries {
            let res = super::lookup(&value, key).unwrap();
            assert_eq!(*res, exp);
        }
        assert!(super::lookup(&value, "foo1").is_none());
    }
}
