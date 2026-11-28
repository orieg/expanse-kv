//! Core in-memory KV engine (`docs/PRD.md` §3, §4.2, §7 Phase 2).

use crate::cursor::{collect_prefix, scan_prefix_batch, scan_range_forward, scan_range_reverse};
use crate::slab::{KeyType, SlotEntry, SlotPayload, ValueSlab};
use core::fmt;
use expanse_kv_core::key::{
    PREFIX_STRING, decode_hash_field_key, decode_string_key, decode_ttl_key, decode_zset_score_key,
    encode_hash_field_key, encode_hash_prefix, encode_string_key, encode_ttl_key,
    encode_zset_member_key, encode_zset_member_prefix, encode_zset_score_bound,
    encode_zset_score_key, encode_zset_score_prefix,
};
use expanse_trie::sync::{OrderedBytesReader, SyncExpanseOrderedBytesMap, SyncExpanseSet};

/// Reusable optimistic reader handle for [`Engine`].
pub type EngineReader<'a> = OrderedBytesReader<'a>;
use std::sync::Mutex;
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

/// Memory eviction policy (`docs/PRD.md` §3.5).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum EvictionPolicy {
    /// Return [`EngineError::Oom`] on writes when `max_memory` is exceeded.
    #[default]
    NoEviction,
    /// Evict soonest-expiring keys directly from the ordered TTL trie.
    VolatileTtl,
    /// Evict least-recently-used keys using sampled 24-bit LRU timestamps.
    AllKeysLru,
}

/// Configuration for [`Engine`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct EngineConfig {
    /// Maximum memory budget in bytes (`0` = unbounded).
    pub max_memory: usize,
    /// Eviction policy applied when `max_memory > 0` and memory is exceeded.
    pub eviction_policy: EvictionPolicy,
    /// Configured shard count (`1` per Phase 0 decision `#14`).
    pub shards: usize,
}

impl Default for EngineConfig {
    fn default() -> Self {
        Self {
            max_memory: 0,
            eviction_policy: EvictionPolicy::NoEviction,
            shards: 1,
        }
    }
}

/// Options for [`Engine::set`].
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct SetOptions {
    /// Set only if the key does not already exist (`NX`).
    pub nx: bool,
    /// Set only if the key already exists (`XX`).
    pub xx: bool,
    /// Retain any existing TTL (`KEEPTTL`).
    pub keep_ttl: bool,
    /// Return the previous string value before overwriting (`GET`).
    pub get_prev: bool,
    /// Optional absolute expiration timestamp in Unix milliseconds.
    pub expiry_ms: Option<u64>,
}

/// Bound type for `ZRANGE` (`BYSCORE` or `BYLEX`) and `ZREMRANGEBYSCORE`.
#[derive(Debug, Clone, PartialEq)]
pub enum ScoreBound {
    /// Inclusive numeric score bound (`x`).
    Inclusive(f64),
    /// Exclusive numeric score bound (`(x`).
    Exclusive(f64),
    /// Unbounded (`-inf` or `+inf`).
    Unbounded,
}

/// Bound type for `ZRANGE ... BYLEX`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LexBound<'a> {
    /// Inclusive member byte slice (`[member`).
    Inclusive(&'a [u8]),
    /// Exclusive member byte slice (`(member`).
    Exclusive(&'a [u8]),
    /// Unbounded (`-` or `+`).
    Unbounded,
}

/// Options for [`Engine::zrange_by_rank`].
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ZRangeRankOptions {
    /// Reverse order (`REV`).
    pub rev: bool,
    /// Include scores (`WITHSCORES`).
    pub with_scores: bool,
}

/// A `(field, value)` pair yielded by [`Engine::hscan`].
pub type FieldValuePair = (Vec<u8>, Vec<u8>);

/// One page returned by a stateless key-bookmark [`Engine::scan`] or [`Engine::hscan`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScanPage<T> {
    /// Next bookmark cursor (`None` when the scan reached the end).
    pub next_cursor: Option<Vec<u8>>,
    /// Entries yielded in this batch.
    pub items: Vec<T>,
}

/// Error returned by [`Engine`] operations.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EngineError {
    /// Operation against a key holding the wrong kind of value (`WRONGTYPE`).
    WrongType,
    /// Value is not an integer or out of `i64` range (`INCRBY`).
    NotAnInteger,
    /// Arithmetic overflow (`INCRBY`).
    Overflow,
    /// Score is `NaN`.
    InvalidScore,
    /// Memory budget exceeded under [`EvictionPolicy::NoEviction`].
    Oom,
    /// Both `NX` and `XX` were specified on `SET`.
    ConflictingOptions,
    /// Bitmap offset exceeded Redis's 32-bit limit (`2^32 - 1`).
    BitOffsetOutOfRange,
}

impl fmt::Display for EngineError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::WrongType => {
                f.write_str("WRONGTYPE Operation against a key holding the wrong kind of value")
            }
            Self::NotAnInteger => f.write_str("ERR value is not an integer or out of range"),
            Self::Overflow => f.write_str("ERR increment or decrement would overflow"),
            Self::InvalidScore => f.write_str("ERR score is not a valid float"),
            Self::Oom => f.write_str("OOM command not allowed when used memory > 'maxmemory'"),
            Self::ConflictingOptions => {
                f.write_str("ERR syntax error: NX and XX are mutually exclusive")
            }
            Self::BitOffsetOutOfRange => {
                f.write_str("ERR bit offset is not an integer or out of range")
            }
        }
    }
}

impl std::error::Error for EngineError {}

/// Multi-threaded, memory-safe in-memory KV engine backed by `expanse-trie`.
pub struct Engine {
    config: EngineConfig,
    /// Shared ordered map for String (`0x01`), Hash (`0x02`), ZSet (`0x03`, `0x04`).
    data: SyncExpanseOrderedBytesMap,
    /// Ordered expiration index: `[expiry_ms_be_u64][key] -> 1`.
    ttl: SyncExpanseOrderedBytesMap,
    /// Single shared `SyncExpanseSet` for all sparse bitmaps (`(id << 32) | offset`).
    bitmaps: SyncExpanseSet,
    /// Generational in-memory value slab.
    slab: ValueSlab,
    /// Live logical key count (`DBSIZE`).
    key_count: AtomicU64,
    /// Next unique 32-bit bitmap namespace ID.
    next_bitmap_id: AtomicU32,
    /// Coordinates multi-step compound mutations (e.g., ZSet score+member, Hash+header, DEL).
    mutation_lock: Mutex<()>,
    /// Optional deterministic test clock override (`0` = use `SystemTime::now()`).
    clock_override_ms: AtomicU64,
}

impl Default for Engine {
    fn default() -> Self {
        Self::new(EngineConfig::default())
    }
}

impl Engine {
    /// Creates a new empty engine with `config`.
    #[must_use]
    pub fn new(config: EngineConfig) -> Self {
        Self {
            config,
            data: SyncExpanseOrderedBytesMap::new(),
            ttl: SyncExpanseOrderedBytesMap::new(),
            bitmaps: SyncExpanseSet::new(),
            slab: ValueSlab::new(),
            key_count: AtomicU64::new(0),
            next_bitmap_id: AtomicU32::new(1),
            mutation_lock: Mutex::new(()),
            clock_override_ms: AtomicU64::new(0),
        }
    }

    /// Sets a deterministic clock in Unix milliseconds for testing TTL behavior
    /// (`0` restores wall-clock `SystemTime::now()`).
    pub fn set_clock_ms(&self, now_ms: u64) {
        self.clock_override_ms.store(now_ms, Ordering::Relaxed);
    }

    /// Returns current engine time in Unix milliseconds.
    #[must_use]
    pub fn now_ms(&self) -> u64 {
        let ov = self.clock_override_ms.load(Ordering::Relaxed);
        if ov != 0 {
            return ov;
        }
        match SystemTime::now().duration_since(UNIX_EPOCH) {
            Ok(d) => d.as_millis() as u64,
            Err(_) => 0,
        }
    }

    #[inline]
    fn lru24_now(&self) -> u32 {
        ((self.now_ms() / 1000) & 0x00FF_FFFF) as u32
    }

    /// Total tracked memory in bytes (`expanse-trie` `mem_used()` + `ValueSlab`).
    #[must_use]
    pub fn mem_used(&self) -> usize {
        self.data.mem_used()
            + self.ttl.mem_used()
            + self.bitmaps.mem_used()
            + self.slab.live_bytes()
    }

    /// Returns a reusable optimistic reader handle for the backing data trie.
    ///
    /// Reusing a reader across a pipelined batch or connection avoids per-op
    /// reader-registry lock contention while holding zero epoch pins when idle
    /// (`AGENTS.md` §2.5).
    #[must_use]
    pub fn reader(&self) -> OrderedBytesReader<'_> {
        self.data.reader()
    }

    /// Returns a reference to the backing ordered data trie (for Phase 2 diagnostics).
    #[must_use]
    pub fn data_trie(&self) -> &SyncExpanseOrderedBytesMap {
        &self.data
    }

    /// Approximate number of live logical keys (`DBSIZE`).
    #[must_use]
    pub fn dbsize(&self) -> u64 {
        self.key_count.load(Ordering::Relaxed)
    }

    /// Removes all keys and resets memory accounting (`FLUSHDB`).
    pub fn flushdb(&self) {
        let _guard = self.mutation_lock.lock().expect("mutation lock poisoned");
        self.data.clear();
        self.ttl.clear();
        self.bitmaps.clear();
        self.slab.clear();
        self.key_count.store(0, Ordering::Relaxed);
    }

    /// Approximate memory usage of a single logical key (`MEMORY USAGE`).
    #[must_use]
    pub fn memory_usage(&self, key: &[u8]) -> Option<usize> {
        let mut kbuf = Vec::new();
        encode_string_key(key, &mut kbuf);
        let handle = self.data.reader().get(&kbuf)?;
        let entry = self.slab.get(handle)?;
        if self.is_expired(&entry) {
            return None;
        }
        Some(key.len() + entry.payload.byte_size() + 16)
    }

    #[inline]
    fn is_expired(&self, entry: &SlotEntry) -> bool {
        entry.expiry_ms.is_some_and(|exp| exp <= self.now_ms())
    }

    /// Removes `key` and all its sub-entries (Hash fields, ZSet indices, Bitmap bits, TTL entry).
    /// Caller MUST hold `self.mutation_lock`.
    fn delete_key_locked(&self, key: &[u8]) -> bool {
        let mut kbuf = Vec::new();
        encode_string_key(key, &mut kbuf);
        let Some(handle) = self.data.remove(&kbuf) else {
            return false;
        };
        self.key_count.fetch_sub(1, Ordering::Relaxed);
        let Some(entry) = self.slab.remove(handle) else {
            return true;
        };
        if let Some(exp) = entry.expiry_ms {
            let mut tbuf = Vec::new();
            encode_ttl_key(exp, key, &mut tbuf);
            self.ttl.remove(&tbuf);
        }
        match entry.payload {
            SlotPayload::Bytes(_) => {}
            SlotPayload::HashHeader { .. } => {
                let mut pbuf = Vec::new();
                encode_hash_prefix(key, &mut pbuf);
                for (hk, fhandle) in collect_prefix(&self.data, &pbuf) {
                    self.data.remove(&hk);
                    self.slab.remove(fhandle);
                }
            }
            SlotPayload::ZSetHeader { .. } => {
                let mut sp = Vec::new();
                encode_zset_score_prefix(key, &mut sp);
                for (zk, _) in collect_prefix(&self.data, &sp) {
                    self.data.remove(&zk);
                }
                let mut mp = Vec::new();
                encode_zset_member_prefix(key, &mut mp);
                for (zk, _) in collect_prefix(&self.data, &mp) {
                    self.data.remove(&zk);
                }
            }
            SlotPayload::BitmapHeader { id, .. } => {
                let lo = u64::from(id) << 32;
                let hi = (u64::from(id) + 1) << 32;
                let mut cur = self.bitmaps.reader().next_at_or_after(lo);
                while let Some(bit_key) = cur {
                    if bit_key >= hi {
                        break;
                    }
                    self.bitmaps.remove(bit_key);
                    cur = bit_key
                        .checked_add(1)
                        .and_then(|next| self.bitmaps.reader().next_at_or_after(next));
                }
            }
        }
        true
    }

    /// Lazily deletes `key` if expired; returns `true` if the key was expired and removed.
    fn expire_if_needed_locked(&self, key: &[u8]) -> bool {
        let mut kbuf = Vec::new();
        encode_string_key(key, &mut kbuf);
        let Some(handle) = self.data.reader().get(&kbuf) else {
            return false;
        };
        let expired = self
            .slab
            .with_entry(handle, |e| self.is_expired(e))
            .unwrap_or(false);
        if expired {
            self.delete_key_locked(key);
            true
        } else {
            false
        }
    }

    /// Actively sweeps up to `max_keys` expired entries from the ordered TTL trie.
    pub fn active_expire_cycle(&self, max_keys: usize) -> usize {
        let now = self.now_ms();
        let _guard = self.mutation_lock.lock().expect("mutation lock poisoned");
        let mut expired_count = 0usize;
        let mut after: Option<Vec<u8>> = None;

        while expired_count < max_keys {
            let batch = scan_prefix_batch(
                &self.ttl,
                &[],
                after.as_deref(),
                (max_keys - expired_count).min(64),
            );
            if batch.is_empty() {
                break;
            }
            let mut progressed = false;
            for (tk, _) in batch {
                let Ok((exp_ms, key)) = decode_ttl_key(&tk) else {
                    after = Some(tk);
                    continue;
                };
                if exp_ms > now {
                    return expired_count;
                }
                self.ttl.remove(&tk);
                // Re-check current expiry in slab (key might have been updated with a later TTL).
                let mut kbuf = Vec::new();
                encode_string_key(key, &mut kbuf);
                if let Some(handle) = self.data.reader().get(&kbuf) {
                    let should_del = self
                        .slab
                        .with_entry(handle, |e| e.expiry_ms.is_some_and(|e_ms| e_ms <= now))
                        .unwrap_or(false);
                    if should_del && self.delete_key_locked(key) {
                        expired_count += 1;
                    }
                }
                after = Some(tk);
                progressed = true;
            }
            if !progressed {
                break;
            }
        }
        expired_count
    }

    /// Enforces `config.max_memory` according to `config.eviction_policy`.
    /// Caller MUST hold `self.mutation_lock`.
    fn enforce_memory_limit_locked(&self) -> Result<(), EngineError> {
        let limit = self.config.max_memory;
        if limit == 0 || self.mem_used() <= limit {
            return Ok(());
        }
        match self.config.eviction_policy {
            EvictionPolicy::NoEviction => Err(EngineError::Oom),
            EvictionPolicy::VolatileTtl => {
                for _ in 0..64 {
                    if self.mem_used() <= limit {
                        return Ok(());
                    }
                    let batch = scan_prefix_batch(&self.ttl, &[], None, 1);
                    let Some((tk, _)) = batch.into_iter().next() else {
                        return Err(EngineError::Oom);
                    };
                    self.ttl.remove(&tk);
                    if let Ok((_, key)) = decode_ttl_key(&tk) {
                        self.delete_key_locked(key);
                    }
                }
                if self.mem_used() <= limit {
                    Ok(())
                } else {
                    Err(EngineError::Oom)
                }
            }
            EvictionPolicy::AllKeysLru => {
                for _ in 0..64 {
                    if self.mem_used() <= limit {
                        return Ok(());
                    }
                    // Sample up to 16 keys from the keyspace and evict the oldest LRU timestamp.
                    let sample = scan_prefix_batch(&self.data, &[PREFIX_STRING], None, 16);
                    if sample.is_empty() {
                        return Err(EngineError::Oom);
                    }
                    let mut victim: Option<(Vec<u8>, u32)> = None;
                    for (ek, handle) in sample {
                        let Ok(raw_key) = decode_string_key(&ek) else {
                            continue;
                        };
                        let lru = self.slab.with_entry(handle, |e| e.lru24).unwrap_or(0);
                        match &victim {
                            None => victim = Some((raw_key.to_vec(), lru)),
                            Some((_, best_lru)) if lru < *best_lru => {
                                victim = Some((raw_key.to_vec(), lru));
                            }
                            _ => {}
                        }
                    }
                    if let Some((vkey, _)) = victim {
                        self.delete_key_locked(&vkey);
                    } else {
                        return Err(EngineError::Oom);
                    }
                }
                if self.mem_used() <= limit {
                    Ok(())
                } else {
                    Err(EngineError::Oom)
                }
            }
        }
    }

    // -----------------------------------------------------------------------
    // String & Generic Key Commands
    // -----------------------------------------------------------------------

    /// `GET key`: optimistic read path (`OrderedBytesReader::get`).
    ///
    /// # Errors
    ///
    /// Returns [`EngineError::WrongType`] if `key` holds a non-string type.
    pub fn get(&self, key: &[u8]) -> Result<Option<Vec<u8>>, EngineError> {
        let reader = self.data.reader();
        self.get_with_reader(&reader, key, |v| v.to_vec())
    }

    /// Zero-allocation `GET key` using a caller-reused [`OrderedBytesReader`].
    ///
    /// Calls `f` with a borrowed slice of the string value directly inside the
    /// slab shard read guard, avoiding per-op key and value heap allocations.
    ///
    /// # Errors
    ///
    /// Returns [`EngineError::WrongType`] if `key` holds a non-string type.
    pub fn get_with_reader<R>(
        &self,
        reader: &OrderedBytesReader<'_>,
        key: &[u8],
        f: impl FnOnce(&[u8]) -> R,
    ) -> Result<Option<R>, EngineError> {
        let handle = if key.len() < 255 {
            let mut stack_buf = [0u8; 256];
            stack_buf[0] = PREFIX_STRING;
            stack_buf[1..=key.len()].copy_from_slice(key);
            match reader.get(&stack_buf[..=key.len()]) {
                Some(h) => h,
                None => return Ok(None),
            }
        } else {
            let mut kbuf = Vec::with_capacity(key.len() + 1);
            encode_string_key(key, &mut kbuf);
            match reader.get(&kbuf) {
                Some(h) => h,
                None => return Ok(None),
            }
        };

        let mut expired = false;
        let out = self.slab.with_entry(handle, |entry| {
            if self.is_expired(entry) {
                expired = true;
                return Ok(None);
            }
            match &entry.payload {
                SlotPayload::Bytes(v) => Ok(Some(f(v))),
                _ => Err(EngineError::WrongType),
            }
        });

        if expired {
            let _guard = self.mutation_lock.lock().expect("mutation lock poisoned");
            self.expire_if_needed_locked(key);
            return Ok(None);
        }
        match out {
            Some(res) => res,
            None => Ok(None),
        }
    }

    /// `SET key value [NX|XX] [KEEPTTL] [GET] [EX|PX|EXAT|PXAT]`.
    ///
    /// Returns `(written, previous_value_if_get_prev)`.
    ///
    /// # Errors
    ///
    /// Returns [`EngineError::ConflictingOptions`], [`EngineError::WrongType`]
    /// (when `get_prev` is set on a non-string key), or [`EngineError::Oom`].
    pub fn set(
        &self,
        key: &[u8],
        value: &[u8],
        opts: SetOptions,
    ) -> Result<(bool, Option<Vec<u8>>), EngineError> {
        let reader = self.data.reader();
        self.set_with_reader(&reader, key, value, opts)
    }

    /// `SET` using a caller-reused [`EngineReader`], with an in-place String
    /// overwrite fast path that avoids trie delete+reinsert and slab churn.
    ///
    /// # Errors
    ///
    /// Returns [`EngineError::ConflictingOptions`], [`EngineError::WrongType`],
    /// or [`EngineError::Oom`].
    pub fn set_with_reader(
        &self,
        reader: &EngineReader<'_>,
        key: &[u8],
        value: &[u8],
        opts: SetOptions,
    ) -> Result<(bool, Option<Vec<u8>>), EngineError> {
        if opts.nx && opts.xx {
            return Err(EngineError::ConflictingOptions);
        }

        let mut heap_kbuf = Vec::new();
        let mut stack_kbuf = [0u8; 256];
        let kbuf: &[u8] = if key.len() < 255 {
            stack_kbuf[0] = PREFIX_STRING;
            stack_kbuf[1..=key.len()].copy_from_slice(key);
            &stack_kbuf[..=key.len()]
        } else {
            encode_string_key(key, &mut heap_kbuf);
            &heap_kbuf
        };

        // Fast path: plain overwrite of an existing non-expiring String key when
        // no max_memory eviction or TTL options are active.
        if !opts.nx
            && !opts.xx
            && !opts.get_prev
            && opts.expiry_ms.is_none()
            && self.config.max_memory == 0
            && let Some(handle) = reader.get(kbuf)
        {
            let updated_in_place = self
                .slab
                .update_entry(handle, |entry| {
                    if entry.expiry_ms.is_none()
                        && let SlotPayload::Bytes(buf) = &mut entry.payload
                    {
                        buf.clear();
                        buf.extend_from_slice(value);
                        true
                    } else {
                        false
                    }
                })
                .unwrap_or(false);
            if updated_in_place {
                return Ok((true, None));
            }
        }

        let _guard = self.mutation_lock.lock().expect("mutation lock poisoned");
        self.expire_if_needed_locked(key);

        let existing_handle = reader.get(kbuf);

        let prev_val = if opts.get_prev {
            match existing_handle.and_then(|h| self.slab.get(h)) {
                Some(entry) => match entry.payload {
                    SlotPayload::Bytes(v) => Some(v),
                    _ => return Err(EngineError::WrongType),
                },
                None => None,
            }
        } else {
            None
        };

        if opts.nx && existing_handle.is_some() {
            return Ok((false, prev_val));
        }
        if opts.xx && existing_handle.is_none() {
            return Ok((false, prev_val));
        }

        let effective_expiry = if opts.keep_ttl {
            existing_handle
                .and_then(|h| self.slab.with_entry(h, |e| e.expiry_ms))
                .flatten()
        } else {
            opts.expiry_ms
        };

        self.enforce_memory_limit_locked()?;

        // If replacing an existing key of another type (Hash/ZSet/Bitmap), clean up its sub-entries.
        if existing_handle.is_some() {
            self.delete_key_locked(key);
        }

        let entry = SlotEntry {
            lru24: self.lru24_now(),
            expiry_ms: effective_expiry,
            payload: SlotPayload::Bytes(value.to_vec()),
        };
        let handle = self.slab.insert(entry);
        self.data.insert(kbuf, handle);
        self.key_count.fetch_add(1, Ordering::Relaxed);

        if let Some(exp) = effective_expiry {
            let mut tbuf = Vec::new();
            encode_ttl_key(exp, key, &mut tbuf);
            self.ttl.insert(&tbuf, 1);
        }
        Ok((true, prev_val))
    }

    /// `MGET key [key ...]`: returns `None` for missing or non-string keys (Redis semantics).
    #[must_use]
    pub fn mget(&self, keys: &[&[u8]]) -> Vec<Option<Vec<u8>>> {
        keys.iter()
            .map(|k| self.get(k).unwrap_or_default())
            .collect()
    }

    /// `MSET key value [key value ...]`.
    ///
    /// # Errors
    ///
    /// Returns [`EngineError::Oom`] if memory budget is exceeded.
    pub fn mset(&self, pairs: &[(&[u8], &[u8])]) -> Result<(), EngineError> {
        for &(k, v) in pairs {
            self.set(k, v, SetOptions::default())?;
        }
        Ok(())
    }

    /// `INCRBY key delta` (also serves `INCR`, `DECR`).
    ///
    /// # Errors
    ///
    /// Returns [`EngineError::WrongType`], [`EngineError::NotAnInteger`],
    /// [`EngineError::Overflow`], or [`EngineError::Oom`].
    pub fn incr_by(&self, key: &[u8], delta: i64) -> Result<i64, EngineError> {
        let _guard = self.mutation_lock.lock().expect("mutation lock poisoned");
        self.expire_if_needed_locked(key);

        let mut kbuf = Vec::new();
        encode_string_key(key, &mut kbuf);
        let (cur, keep_exp) = match self.data.reader().get(&kbuf) {
            Some(handle) => {
                let entry = self.slab.get(handle).ok_or(EngineError::NotAnInteger)?;
                match entry.payload {
                    SlotPayload::Bytes(bytes) => {
                        let s =
                            core::str::from_utf8(&bytes).map_err(|_| EngineError::NotAnInteger)?;
                        let v = s.parse::<i64>().map_err(|_| EngineError::NotAnInteger)?;
                        (v, entry.expiry_ms)
                    }
                    _ => return Err(EngineError::WrongType),
                }
            }
            None => (0i64, None),
        };

        let next = cur.checked_add(delta).ok_or(EngineError::Overflow)?;
        self.enforce_memory_limit_locked()?;

        let next_bytes = next.to_string().into_bytes();
        if let Some(handle) = self.data.reader().get(&kbuf) {
            self.slab.update_entry(handle, |e| {
                e.lru24 = self.lru24_now();
                e.payload = SlotPayload::Bytes(next_bytes);
            });
        } else {
            let entry = SlotEntry {
                lru24: self.lru24_now(),
                expiry_ms: keep_exp,
                payload: SlotPayload::Bytes(next_bytes),
            };
            let handle = self.slab.insert(entry);
            self.data.insert(&kbuf, handle);
            self.key_count.fetch_add(1, Ordering::Relaxed);
        }
        Ok(next)
    }

    /// `DEL key [key ...]`: deletes keys and returns the number removed.
    pub fn del(&self, keys: &[&[u8]]) -> u64 {
        let _guard = self.mutation_lock.lock().expect("mutation lock poisoned");
        let mut removed = 0u64;
        for &k in keys {
            if !self.expire_if_needed_locked(k) && self.delete_key_locked(k) {
                removed += 1;
            }
        }
        removed
    }

    /// `EXISTS key`: returns `true` if `key` exists and is not expired.
    #[must_use]
    pub fn exists(&self, key: &[u8]) -> bool {
        let mut kbuf = Vec::new();
        encode_string_key(key, &mut kbuf);
        let Some(handle) = self.data.reader().get(&kbuf) else {
            return false;
        };
        self.slab
            .with_entry(handle, |e| !self.is_expired(e))
            .unwrap_or(false)
    }

    /// `TYPE key`: returns the logical [`KeyType`], or `None` if absent/expired.
    #[must_use]
    pub fn key_type(&self, key: &[u8]) -> Option<KeyType> {
        let mut kbuf = Vec::new();
        encode_string_key(key, &mut kbuf);
        let handle = self.data.reader().get(&kbuf)?;
        self.slab.with_entry(handle, |e| {
            if self.is_expired(e) {
                None
            } else {
                Some(e.payload.key_type())
            }
        })?
    }

    /// `PEXPIREAT key expiry_ms`: sets an absolute expiration timestamp in Unix ms.
    pub fn pexpire_at(&self, key: &[u8], expiry_ms: u64) -> bool {
        let _guard = self.mutation_lock.lock().expect("mutation lock poisoned");
        if self.expire_if_needed_locked(key) {
            return false;
        }
        let mut kbuf = Vec::new();
        encode_string_key(key, &mut kbuf);
        let Some(handle) = self.data.reader().get(&kbuf) else {
            return false;
        };
        if expiry_ms <= self.now_ms() {
            self.delete_key_locked(key);
            return true;
        }
        let old_exp = self
            .slab
            .update_entry(handle, |e| {
                let prev = e.expiry_ms;
                e.expiry_ms = Some(expiry_ms);
                prev
            })
            .flatten();
        if let Some(prev) = old_exp {
            let mut old_tbuf = Vec::new();
            encode_ttl_key(prev, key, &mut old_tbuf);
            self.ttl.remove(&old_tbuf);
        }
        let mut new_tbuf = Vec::new();
        encode_ttl_key(expiry_ms, key, &mut new_tbuf);
        self.ttl.insert(&new_tbuf, 1);
        true
    }

    /// `PTTL key`: returns remaining TTL in ms, `-1` if no expiry, `-2` if absent/expired.
    #[must_use]
    pub fn pttl(&self, key: &[u8]) -> i64 {
        let mut kbuf = Vec::new();
        encode_string_key(key, &mut kbuf);
        let Some(handle) = self.data.reader().get(&kbuf) else {
            return -2;
        };
        let Some(exp) = self.slab.with_entry(handle, |e| e.expiry_ms) else {
            return -2;
        };
        match exp {
            None => -1,
            Some(exp_ms) => {
                let now = self.now_ms();
                if exp_ms <= now {
                    -2
                } else {
                    (exp_ms - now) as i64
                }
            }
        }
    }

    /// `PERSIST key`: removes expiration from `key`; returns `true` if a TTL was cleared.
    pub fn persist(&self, key: &[u8]) -> bool {
        let _guard = self.mutation_lock.lock().expect("mutation lock poisoned");
        if self.expire_if_needed_locked(key) {
            return false;
        }
        let mut kbuf = Vec::new();
        encode_string_key(key, &mut kbuf);
        let Some(handle) = self.data.reader().get(&kbuf) else {
            return false;
        };
        let old_exp = self
            .slab
            .update_entry(handle, |e| e.expiry_ms.take())
            .flatten();
        if let Some(prev) = old_exp {
            let mut tbuf = Vec::new();
            encode_ttl_key(prev, key, &mut tbuf);
            self.ttl.remove(&tbuf);
            true
        } else {
            false
        }
    }

    /// Stateless bookmark `SCAN`: scans logical keys matching optional `prefix`
    /// starting strictly after `cursor`, yielding up to `count` live keys.
    ///
    /// Never holds a reclamation epoch pin across calls (`AGENTS.md` §2.5).
    #[must_use]
    pub fn scan(&self, cursor: Option<&[u8]>, prefix: &[u8], count: usize) -> ScanPage<Vec<u8>> {
        let mut full_prefix = Vec::with_capacity(1 + prefix.len());
        full_prefix.push(PREFIX_STRING);
        full_prefix.extend_from_slice(prefix);

        let after_encoded = cursor.map(|c| {
            let mut b = Vec::with_capacity(1 + c.len());
            encode_string_key(c, &mut b);
            b
        });

        let batch = scan_prefix_batch(
            &self.data,
            &full_prefix,
            after_encoded.as_deref(),
            count.saturating_add(1),
        );
        let has_more = batch.len() > count;
        let mut items = Vec::with_capacity(batch.len().min(count));
        let mut last_raw: Option<Vec<u8>> = None;

        for (ek, handle) in batch.into_iter().take(count) {
            let Ok(raw_key) = decode_string_key(&ek) else {
                continue;
            };
            last_raw = Some(raw_key.to_vec());
            let live = self
                .slab
                .with_entry(handle, |e| !self.is_expired(e))
                .unwrap_or(false);
            if live {
                items.push(raw_key.to_vec());
            }
        }
        ScanPage {
            next_cursor: if has_more { last_raw } else { None },
            items,
        }
    }

    // -----------------------------------------------------------------------
    // Hash Commands (`HSET`, `HGET`, `HMGET`, `HDEL`, `HEXISTS`, `HLEN`, `HGETALL`, `HSCAN`)
    // -----------------------------------------------------------------------

    /// `HSET key field value [field value ...]`: returns count of newly added fields.
    ///
    /// # Errors
    ///
    /// Returns [`EngineError::WrongType`] or [`EngineError::Oom`].
    pub fn hset(&self, key: &[u8], fields: &[(&[u8], &[u8])]) -> Result<u64, EngineError> {
        let _guard = self.mutation_lock.lock().expect("mutation lock poisoned");
        self.expire_if_needed_locked(key);
        self.enforce_memory_limit_locked()?;

        let mut kbuf = Vec::new();
        encode_string_key(key, &mut kbuf);
        let header_handle = match self.data.reader().get(&kbuf) {
            Some(h) => {
                let is_hash = self
                    .slab
                    .with_entry(h, |e| matches!(e.payload, SlotPayload::HashHeader { .. }))
                    .unwrap_or(false);
                if !is_hash {
                    return Err(EngineError::WrongType);
                }
                h
            }
            None => {
                let h = self.slab.insert(SlotEntry {
                    lru24: self.lru24_now(),
                    expiry_ms: None,
                    payload: SlotPayload::HashHeader { len: 0 },
                });
                self.data.insert(&kbuf, h);
                self.key_count.fetch_add(1, Ordering::Relaxed);
                h
            }
        };

        let mut added = 0u64;
        let mut fbuf = Vec::new();
        for &(field, val) in fields {
            encode_hash_field_key(key, field, &mut fbuf);
            let new_handle = self.slab.insert(SlotEntry {
                lru24: self.lru24_now(),
                expiry_ms: None,
                payload: SlotPayload::Bytes(val.to_vec()),
            });
            if let Some(old_handle) = self.data.insert(&fbuf, new_handle) {
                self.slab.remove(old_handle);
            } else {
                added += 1;
            }
        }
        if added > 0 {
            self.slab.update_entry(header_handle, |e| {
                if let SlotPayload::HashHeader { len } = &mut e.payload {
                    *len += added;
                }
            });
        }
        Ok(added)
    }

    /// `HGET key field`.
    ///
    /// # Errors
    ///
    /// Returns [`EngineError::WrongType`] if `key` is not a Hash.
    pub fn hget(&self, key: &[u8], field: &[u8]) -> Result<Option<Vec<u8>>, EngineError> {
        match self.key_type(key) {
            None => return Ok(None),
            Some(KeyType::Hash) => {}
            Some(_) => return Err(EngineError::WrongType),
        }
        let mut fbuf = Vec::new();
        encode_hash_field_key(key, field, &mut fbuf);
        let Some(handle) = self.data.reader().get(&fbuf) else {
            return Ok(None);
        };
        Ok(self.slab.get(handle).and_then(|e| match e.payload {
            SlotPayload::Bytes(v) => Some(v),
            _ => None,
        }))
    }

    /// `HDEL key field [field ...]`: returns number of deleted fields (auto-deletes empty Hash).
    ///
    /// # Errors
    ///
    /// Returns [`EngineError::WrongType`] if `key` is not a Hash.
    pub fn hdel(&self, key: &[u8], fields: &[&[u8]]) -> Result<u64, EngineError> {
        let _guard = self.mutation_lock.lock().expect("mutation lock poisoned");
        if self.expire_if_needed_locked(key) {
            return Ok(0);
        }
        let mut kbuf = Vec::new();
        encode_string_key(key, &mut kbuf);
        let Some(header_handle) = self.data.reader().get(&kbuf) else {
            return Ok(0);
        };
        let is_hash = self
            .slab
            .with_entry(header_handle, |e| {
                matches!(e.payload, SlotPayload::HashHeader { .. })
            })
            .unwrap_or(false);
        if !is_hash {
            return Err(EngineError::WrongType);
        }

        let mut removed = 0u64;
        let mut fbuf = Vec::new();
        for &field in fields {
            encode_hash_field_key(key, field, &mut fbuf);
            if let Some(old_handle) = self.data.remove(&fbuf) {
                self.slab.remove(old_handle);
                removed += 1;
            }
        }
        if removed > 0 {
            let remaining = self
                .slab
                .update_entry(header_handle, |e| {
                    if let SlotPayload::HashHeader { len } = &mut e.payload {
                        *len = len.saturating_sub(removed);
                        *len
                    } else {
                        0
                    }
                })
                .unwrap_or(0);
            if remaining == 0 {
                self.delete_key_locked(key);
            }
        }
        Ok(removed)
    }

    /// `HLEN key`.
    ///
    /// # Errors
    ///
    /// Returns [`EngineError::WrongType`] if `key` is not a Hash.
    pub fn hlen(&self, key: &[u8]) -> Result<u64, EngineError> {
        let mut kbuf = Vec::new();
        encode_string_key(key, &mut kbuf);
        let Some(handle) = self.data.reader().get(&kbuf) else {
            return Ok(0);
        };
        let Some(entry) = self.slab.get(handle) else {
            return Ok(0);
        };
        if self.is_expired(&entry) {
            return Ok(0);
        }
        match entry.payload {
            SlotPayload::HashHeader { len } => Ok(len),
            _ => Err(EngineError::WrongType),
        }
    }

    /// `HSCAN key cursor count`: stateless bookmark scan of Hash `(field, value)` pairs.
    ///
    /// # Errors
    ///
    /// Returns [`EngineError::WrongType`] if `key` is not a Hash.
    pub fn hscan(
        &self,
        key: &[u8],
        cursor: Option<&[u8]>,
        count: usize,
    ) -> Result<ScanPage<FieldValuePair>, EngineError> {
        match self.key_type(key) {
            None => {
                return Ok(ScanPage {
                    next_cursor: None,
                    items: Vec::new(),
                });
            }
            Some(KeyType::Hash) => {}
            Some(_) => return Err(EngineError::WrongType),
        }
        let mut prefix = Vec::new();
        encode_hash_prefix(key, &mut prefix);
        let after_encoded = cursor.map(|f| {
            let mut b = Vec::new();
            encode_hash_field_key(key, f, &mut b);
            b
        });
        let batch = scan_prefix_batch(
            &self.data,
            &prefix,
            after_encoded.as_deref(),
            count.saturating_add(1),
        );
        let has_more = batch.len() > count;
        let mut items = Vec::with_capacity(batch.len().min(count));
        let mut last_field: Option<Vec<u8>> = None;

        for (hk, fhandle) in batch.into_iter().take(count) {
            let Ok((_, field)) = decode_hash_field_key(&hk) else {
                continue;
            };
            last_field = Some(field.to_vec());
            if let Some(SlotPayload::Bytes(v)) = self.slab.get(fhandle).map(|e| e.payload) {
                items.push((field.to_vec(), v));
            }
        }
        Ok(ScanPage {
            next_cursor: if has_more { last_field } else { None },
            items,
        })
    }

    // -----------------------------------------------------------------------
    // Sorted Set Commands (`ZADD`, `ZREM`, `ZSCORE`, `ZCARD`, `ZRANGE`, `ZREMRANGEBYSCORE`)
    // -----------------------------------------------------------------------

    /// `ZADD key score member [score member ...]`: returns count of newly added members.
    ///
    /// # Errors
    ///
    /// Returns [`EngineError::InvalidScore`], [`EngineError::WrongType`], or [`EngineError::Oom`].
    pub fn zadd(&self, key: &[u8], members: &[(f64, &[u8])]) -> Result<u64, EngineError> {
        for &(score, _) in members {
            if score.is_nan() {
                return Err(EngineError::InvalidScore);
            }
        }
        let _guard = self.mutation_lock.lock().expect("mutation lock poisoned");
        self.expire_if_needed_locked(key);
        self.enforce_memory_limit_locked()?;

        let mut kbuf = Vec::new();
        encode_string_key(key, &mut kbuf);
        let header_handle = match self.data.reader().get(&kbuf) {
            Some(h) => {
                let is_zset = self
                    .slab
                    .with_entry(h, |e| matches!(e.payload, SlotPayload::ZSetHeader { .. }))
                    .unwrap_or(false);
                if !is_zset {
                    return Err(EngineError::WrongType);
                }
                h
            }
            None => {
                let h = self.slab.insert(SlotEntry {
                    lru24: self.lru24_now(),
                    expiry_ms: None,
                    payload: SlotPayload::ZSetHeader { len: 0 },
                });
                self.data.insert(&kbuf, h);
                self.key_count.fetch_add(1, Ordering::Relaxed);
                h
            }
        };

        let mut added = 0u64;
        let mut mbuf = Vec::new();
        let mut sbuf = Vec::new();
        for &(raw_score, member) in members {
            let score = if raw_score == 0.0 { 0.0 } else { raw_score };
            encode_zset_member_key(key, member, &mut mbuf);
            if let Some(old_bits) = self.data.insert(&mbuf, score.to_bits()) {
                let old_score = f64::from_bits(old_bits);
                if old_score.to_bits() != score.to_bits() {
                    encode_zset_score_key(key, old_score, member, &mut sbuf)
                        .map_err(|_| EngineError::InvalidScore)?;
                    self.data.remove(&sbuf);
                }
            } else {
                added += 1;
            }
            encode_zset_score_key(key, score, member, &mut sbuf)
                .map_err(|_| EngineError::InvalidScore)?;
            self.data.insert(&sbuf, 1);
        }
        if added > 0 {
            self.slab.update_entry(header_handle, |e| {
                if let SlotPayload::ZSetHeader { len } = &mut e.payload {
                    *len += added;
                }
            });
        }
        Ok(added)
    }

    /// `ZSCORE key member`.
    ///
    /// # Errors
    ///
    /// Returns [`EngineError::WrongType`] if `key` is not a Sorted Set.
    pub fn zscore(&self, key: &[u8], member: &[u8]) -> Result<Option<f64>, EngineError> {
        match self.key_type(key) {
            None => return Ok(None),
            Some(KeyType::ZSet) => {}
            Some(_) => return Err(EngineError::WrongType),
        }
        let mut mbuf = Vec::new();
        encode_zset_member_key(key, member, &mut mbuf);
        Ok(self.data.reader().get(&mbuf).map(f64::from_bits))
    }

    /// `ZCARD key`.
    ///
    /// # Errors
    ///
    /// Returns [`EngineError::WrongType`] if `key` is not a Sorted Set.
    pub fn zcard(&self, key: &[u8]) -> Result<u64, EngineError> {
        let mut kbuf = Vec::new();
        encode_string_key(key, &mut kbuf);
        let Some(handle) = self.data.reader().get(&kbuf) else {
            return Ok(0);
        };
        let Some(entry) = self.slab.get(handle) else {
            return Ok(0);
        };
        if self.is_expired(&entry) {
            return Ok(0);
        }
        match entry.payload {
            SlotPayload::ZSetHeader { len } => Ok(len),
            _ => Err(EngineError::WrongType),
        }
    }

    /// `ZREM key member [member ...]`: returns number of removed members.
    ///
    /// # Errors
    ///
    /// Returns [`EngineError::WrongType`] if `key` is not a Sorted Set.
    pub fn zrem(&self, key: &[u8], members: &[&[u8]]) -> Result<u64, EngineError> {
        let _guard = self.mutation_lock.lock().expect("mutation lock poisoned");
        if self.expire_if_needed_locked(key) {
            return Ok(0);
        }
        let mut kbuf = Vec::new();
        encode_string_key(key, &mut kbuf);
        let Some(header_handle) = self.data.reader().get(&kbuf) else {
            return Ok(0);
        };
        let is_zset = self
            .slab
            .with_entry(header_handle, |e| {
                matches!(e.payload, SlotPayload::ZSetHeader { .. })
            })
            .unwrap_or(false);
        if !is_zset {
            return Err(EngineError::WrongType);
        }

        let mut removed = 0u64;
        let mut mbuf = Vec::new();
        let mut sbuf = Vec::new();
        for &member in members {
            encode_zset_member_key(key, member, &mut mbuf);
            if let Some(old_bits) = self.data.remove(&mbuf) {
                let old_score = f64::from_bits(old_bits);
                if encode_zset_score_key(key, old_score, member, &mut sbuf).is_ok() {
                    self.data.remove(&sbuf);
                }
                removed += 1;
            }
        }
        if removed > 0 {
            let remaining = self
                .slab
                .update_entry(header_handle, |e| {
                    if let SlotPayload::ZSetHeader { len } = &mut e.payload {
                        *len = len.saturating_sub(removed);
                        *len
                    } else {
                        0
                    }
                })
                .unwrap_or(0);
            if remaining == 0 {
                self.delete_key_locked(key);
            }
        }
        Ok(removed)
    }

    /// `ZRANGE key start stop [REV]` by rank (supports negative indices like `-1`).
    ///
    /// # Errors
    ///
    /// Returns [`EngineError::WrongType`] if `key` is not a Sorted Set.
    pub fn zrange_by_rank(
        &self,
        key: &[u8],
        start: i64,
        stop: i64,
        rev: bool,
    ) -> Result<Vec<(Vec<u8>, f64)>, EngineError> {
        let card = self.zcard(key)? as i64;
        if card == 0 {
            return Ok(Vec::new());
        }
        let s = if start < 0 {
            (card + start).max(0)
        } else {
            start
        };
        let e = if stop < 0 {
            card + stop
        } else {
            stop.min(card - 1)
        };
        if s > e || s >= card {
            return Ok(Vec::new());
        }
        let offset = s as usize;
        let limit = (e - s + 1) as usize;

        let mut prefix = Vec::new();
        encode_zset_score_prefix(key, &mut prefix);
        let raw = if rev {
            scan_range_reverse(&self.data, &prefix, None, None, offset, Some(limit))
        } else {
            scan_range_forward(&self.data, &prefix, None, None, offset, Some(limit))
        };
        Ok(raw
            .into_iter()
            .filter_map(|(k, _)| {
                decode_zset_score_key(&k)
                    .ok()
                    .map(|(_, score, member)| (member.to_vec(), score))
            })
            .collect())
    }

    /// `ZRANGE key min max BYSCORE [REV] [LIMIT offset count]`.
    ///
    /// # Errors
    ///
    /// Returns [`EngineError::WrongType`] or [`EngineError::InvalidScore`].
    pub fn zrange_by_score(
        &self,
        key: &[u8],
        min: &ScoreBound,
        max: &ScoreBound,
        rev: bool,
        offset: usize,
        limit: Option<usize>,
    ) -> Result<Vec<(Vec<u8>, f64)>, EngineError> {
        match self.key_type(key) {
            None => return Ok(Vec::new()),
            Some(KeyType::ZSet) => {}
            Some(_) => return Err(EngineError::WrongType),
        }

        let mut prefix = Vec::new();
        encode_zset_score_prefix(key, &mut prefix);

        // Compute tight prefix-encoded score bounds in composite key space.
        let mut lo_buf = Vec::new();
        let lo_bound = match min {
            ScoreBound::Unbounded => None,
            ScoreBound::Inclusive(v) => {
                encode_zset_score_bound(key, *v, &mut lo_buf)
                    .map_err(|_| EngineError::InvalidScore)?;
                Some((lo_buf.as_slice(), true))
            }
            ScoreBound::Exclusive(v) => {
                // Next representable f64 above `v` for exclusive lower bound.
                let next_v = next_f64_up(*v)?;
                encode_zset_score_bound(key, next_v, &mut lo_buf)
                    .map_err(|_| EngineError::InvalidScore)?;
                Some((lo_buf.as_slice(), true))
            }
        };

        let mut hi_buf = Vec::new();
        let hi_bound = match max {
            ScoreBound::Unbounded => None,
            ScoreBound::Inclusive(v) => {
                // Any member with score == v has key `[prefix][score(v)][member]`, which is
                // strictly less than `[prefix][score(next_up(v))]`.
                if *v == f64::INFINITY {
                    None
                } else {
                    let next_v = next_f64_up(*v)?;
                    encode_zset_score_bound(key, next_v, &mut hi_buf)
                        .map_err(|_| EngineError::InvalidScore)?;
                    Some((hi_buf.as_slice(), false))
                }
            }
            ScoreBound::Exclusive(v) => {
                encode_zset_score_bound(key, *v, &mut hi_buf)
                    .map_err(|_| EngineError::InvalidScore)?;
                Some((hi_buf.as_slice(), false))
            }
        };

        let raw = if rev {
            scan_range_reverse(&self.data, &prefix, lo_bound, hi_bound, offset, limit)
        } else {
            scan_range_forward(&self.data, &prefix, lo_bound, hi_bound, offset, limit)
        };
        Ok(raw
            .into_iter()
            .filter_map(|(k, _)| {
                decode_zset_score_key(&k)
                    .ok()
                    .map(|(_, score, member)| (member.to_vec(), score))
            })
            .collect())
    }

    /// `ZREMRANGEBYSCORE key min max`: removes members in score range and returns count removed.
    ///
    /// # Errors
    ///
    /// Returns [`EngineError::WrongType`] or [`EngineError::InvalidScore`].
    pub fn zremrangebyscore(
        &self,
        key: &[u8],
        min: &ScoreBound,
        max: &ScoreBound,
    ) -> Result<u64, EngineError> {
        let targets = self.zrange_by_score(key, min, max, false, 0, None)?;
        if targets.is_empty() {
            return Ok(0);
        }
        let refs: Vec<&[u8]> = targets.iter().map(|(m, _)| m.as_slice()).collect();
        self.zrem(key, &refs)
    }

    // -----------------------------------------------------------------------
    // Sparse Bitmap Commands (`SETBIT`, `GETBIT`, `BITCOUNT`)
    // -----------------------------------------------------------------------

    /// `SETBIT key offset value`: sets bit `offset` (`0..=u32::MAX`) to `value`,
    /// returning the previous bit value (`0` or `1`).
    ///
    /// # Errors
    ///
    /// Returns [`EngineError::BitOffsetOutOfRange`], [`EngineError::WrongType`], or [`EngineError::Oom`].
    pub fn setbit(&self, key: &[u8], offset: u64, value: bool) -> Result<u8, EngineError> {
        let off32 = u32::try_from(offset).map_err(|_| EngineError::BitOffsetOutOfRange)?;
        let _guard = self.mutation_lock.lock().expect("mutation lock poisoned");
        self.expire_if_needed_locked(key);
        self.enforce_memory_limit_locked()?;

        let mut kbuf = Vec::new();
        encode_string_key(key, &mut kbuf);
        let (header_handle, bitmap_id) = match self.data.reader().get(&kbuf) {
            Some(h) => {
                let id = self
                    .slab
                    .with_entry(h, |e| match e.payload {
                        SlotPayload::BitmapHeader { id, .. } => Some(id),
                        _ => None,
                    })
                    .flatten()
                    .ok_or(EngineError::WrongType)?;
                (h, id)
            }
            None => {
                let id = self.next_bitmap_id.fetch_add(1, Ordering::Relaxed);
                let h = self.slab.insert(SlotEntry {
                    lru24: self.lru24_now(),
                    expiry_ms: None,
                    payload: SlotPayload::BitmapHeader { id, bit_count: 0 },
                });
                self.data.insert(&kbuf, h);
                self.key_count.fetch_add(1, Ordering::Relaxed);
                (h, id)
            }
        };

        let packed = (u64::from(bitmap_id) << 32) | u64::from(off32);
        let prev = if value {
            let inserted = self.bitmaps.insert(packed);
            if inserted {
                self.slab.update_entry(header_handle, |e| {
                    if let SlotPayload::BitmapHeader { bit_count, .. } = &mut e.payload {
                        *bit_count += 1;
                    }
                });
                0
            } else {
                1
            }
        } else {
            let removed = self.bitmaps.remove(packed);
            if removed {
                self.slab.update_entry(header_handle, |e| {
                    if let SlotPayload::BitmapHeader { bit_count, .. } = &mut e.payload {
                        *bit_count = bit_count.saturating_sub(1);
                    }
                });
                1
            } else {
                0
            }
        };
        Ok(prev)
    }

    /// `GETBIT key offset`: optimistic bit lookup via `SyncExpanseSet::reader().contains`.
    ///
    /// # Errors
    ///
    /// Returns [`EngineError::BitOffsetOutOfRange`] or [`EngineError::WrongType`].
    pub fn getbit(&self, key: &[u8], offset: u64) -> Result<u8, EngineError> {
        let off32 = u32::try_from(offset).map_err(|_| EngineError::BitOffsetOutOfRange)?;
        let mut kbuf = Vec::new();
        encode_string_key(key, &mut kbuf);
        let Some(handle) = self.data.reader().get(&kbuf) else {
            return Ok(0);
        };
        let Some(entry) = self.slab.get(handle) else {
            return Ok(0);
        };
        if self.is_expired(&entry) {
            return Ok(0);
        }
        let SlotPayload::BitmapHeader { id, .. } = entry.payload else {
            return Err(EngineError::WrongType);
        };
        let packed = (u64::from(id) << 32) | u64::from(off32);
        Ok(u8::from(self.bitmaps.reader().contains(packed)))
    }

    /// `BITCOUNT key`: returns total number of set bits in `key`.
    ///
    /// # Errors
    ///
    /// Returns [`EngineError::WrongType`] if `key` is not a sparse bitmap.
    pub fn bitcount(&self, key: &[u8]) -> Result<u64, EngineError> {
        let mut kbuf = Vec::new();
        encode_string_key(key, &mut kbuf);
        let Some(handle) = self.data.reader().get(&kbuf) else {
            return Ok(0);
        };
        let Some(entry) = self.slab.get(handle) else {
            return Ok(0);
        };
        if self.is_expired(&entry) {
            return Ok(0);
        }
        match entry.payload {
            SlotPayload::BitmapHeader { bit_count, .. } => Ok(bit_count),
            _ => Err(EngineError::WrongType),
        }
    }
}

/// Returns the smallest `f64` strictly greater than `v` (canonicalizing `-0.0` to `+0.0`).
fn next_f64_up(v: f64) -> Result<f64, EngineError> {
    if v.is_nan() {
        return Err(EngineError::InvalidScore);
    }
    let v = if v == 0.0 { 0.0 } else { v };
    Ok(v.next_up())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strings_incr_ttl_and_scan_work_end_to_end() {
        let eng = Engine::default();
        eng.set_clock_ms(1_000);

        assert_eq!(eng.get(b"k1"), Ok(None));
        assert_eq!(
            eng.set(b"k1", b"hello", SetOptions::default()),
            Ok((true, None))
        );
        assert_eq!(eng.get(b"k1"), Ok(Some(b"hello".to_vec())));
        assert_eq!(eng.dbsize(), 1);

        // NX / XX / GET
        assert_eq!(
            eng.set(
                b"k1",
                b"nope",
                SetOptions {
                    nx: true,
                    get_prev: true,
                    ..SetOptions::default()
                }
            ),
            Ok((false, Some(b"hello".to_vec())))
        );
        assert_eq!(
            eng.set(
                b"k1",
                b"world",
                SetOptions {
                    xx: true,
                    get_prev: true,
                    expiry_ms: Some(2_000),
                    ..SetOptions::default()
                }
            ),
            Ok((true, Some(b"hello".to_vec())))
        );
        assert_eq!(eng.pttl(b"k1"), 1_000);

        // Expiry at t = 2_000
        eng.set_clock_ms(2_000);
        assert_eq!(eng.get(b"k1"), Ok(None));
        assert_eq!(eng.dbsize(), 0);

        // INCRBY
        assert_eq!(eng.incr_by(b"counter", 5), Ok(5));
        assert_eq!(eng.incr_by(b"counter", -2), Ok(3));
        assert_eq!(eng.get(b"counter"), Ok(Some(b"3".to_vec())));

        // Prefix SCAN
        for i in 0..5u8 {
            let k = format!("org:user:{i}");
            let _ = eng
                .set(k.as_bytes(), b"v", SetOptions::default())
                .expect("set");
        }
        let p1 = eng.scan(None, b"org:user:", 2);
        assert_eq!(
            p1.items,
            vec![b"org:user:0".to_vec(), b"org:user:1".to_vec()]
        );
        assert!(p1.next_cursor.is_some());
        let p2 = eng.scan(p1.next_cursor.as_deref(), b"org:user:", 10);
        assert_eq!(
            p2.items,
            vec![
                b"org:user:2".to_vec(),
                b"org:user:3".to_vec(),
                b"org:user:4".to_vec()
            ]
        );
        assert_eq!(p2.next_cursor, None);
    }

    #[test]
    fn hashes_zsets_and_sparse_bitmaps_enforce_types_and_ordering() {
        let eng = Engine::default();

        // Hash
        assert_eq!(eng.hset(b"h1", &[(b"f2", b"v2"), (b"f1", b"v1")]), Ok(2));
        assert_eq!(eng.hlen(b"h1"), Ok(2));
        assert_eq!(eng.hget(b"h1", b"f1"), Ok(Some(b"v1".to_vec())));
        assert_eq!(eng.get(b"h1"), Err(EngineError::WrongType));
        let hs = eng.hscan(b"h1", None, 10).expect("hscan");
        assert_eq!(
            hs.items,
            vec![
                (b"f1".to_vec(), b"v1".to_vec()),
                (b"f2".to_vec(), b"v2".to_vec())
            ]
        );
        assert_eq!(eng.hdel(b"h1", &[b"f1", b"f2"]), Ok(2));
        assert!(!eng.exists(b"h1"));

        // ZSet
        assert_eq!(
            eng.zadd(
                b"z1",
                &[(2.5, b"b"), (-1.0, b"a"), (2.5, b"a2"), (10.0, b"c")]
            ),
            Ok(4)
        );
        assert_eq!(eng.zcard(b"z1"), Ok(4));
        assert_eq!(eng.zscore(b"z1", b"b"), Ok(Some(2.5)));
        assert_eq!(
            eng.zrange_by_rank(b"z1", 0, -1, false),
            Ok(vec![
                (b"a".to_vec(), -1.0),
                (b"a2".to_vec(), 2.5),
                (b"b".to_vec(), 2.5),
                (b"c".to_vec(), 10.0),
            ])
        );
        assert_eq!(
            eng.zrange_by_rank(b"z1", 0, 1, true),
            Ok(vec![(b"c".to_vec(), 10.0), (b"b".to_vec(), 2.5)])
        );
        assert_eq!(
            eng.zrange_by_score(
                b"z1",
                &ScoreBound::Inclusive(0.0),
                &ScoreBound::Exclusive(10.0),
                false,
                0,
                None
            ),
            Ok(vec![(b"a2".to_vec(), 2.5), (b"b".to_vec(), 2.5)])
        );
        assert_eq!(
            eng.zremrangebyscore(
                b"z1",
                &ScoreBound::Inclusive(-1.0),
                &ScoreBound::Inclusive(2.5)
            ),
            Ok(3)
        );
        assert_eq!(eng.zcard(b"z1"), Ok(1));

        // Sparse Bitmap across wide 32-bit offsets
        assert_eq!(eng.setbit(b"bm1", 1_000_000_000, true), Ok(0));
        assert_eq!(eng.setbit(b"bm1", 4_000_000_000, true), Ok(0));
        assert_eq!(eng.setbit(b"bm1", 1_000_000_000, true), Ok(1));
        assert_eq!(eng.getbit(b"bm1", 1_000_000_000), Ok(1));
        assert_eq!(eng.getbit(b"bm1", 42), Ok(0));
        assert_eq!(eng.bitcount(b"bm1"), Ok(2));
        assert_eq!(eng.del(&[b"bm1"]), 1);
        assert_eq!(eng.bitcount(b"bm1"), Ok(0));
    }

    #[test]
    fn active_expiry_and_eviction_policies_work() {
        let eng = Engine::new(EngineConfig {
            max_memory: 0,
            eviction_policy: EvictionPolicy::VolatileTtl,
            shards: 1,
        });
        eng.set_clock_ms(1_000);
        let _ = eng.set(
            b"exp1",
            b"v1",
            SetOptions {
                expiry_ms: Some(1_500),
                ..SetOptions::default()
            },
        );
        let _ = eng.set(
            b"exp2",
            b"v2",
            SetOptions {
                expiry_ms: Some(2_500),
                ..SetOptions::default()
            },
        );
        eng.set_clock_ms(2_000);
        assert_eq!(eng.active_expire_cycle(10), 1);
        assert!(!eng.exists(b"exp1"));
        assert!(eng.exists(b"exp2"));

        // NoEviction returns Oom when budget is exceeded.
        let oom_eng = Engine::new(EngineConfig {
            max_memory: 1,
            eviction_policy: EvictionPolicy::NoEviction,
            shards: 1,
        });
        let _ = oom_eng.set(b"first", b"val", SetOptions::default());
        assert_eq!(
            oom_eng.set(b"second", b"val", SetOptions::default()),
            Err(EngineError::Oom)
        );
    }
}
