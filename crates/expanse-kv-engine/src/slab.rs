//! Sharded generational in-memory value slab with exact byte accounting (`docs/PRD.md` §3.5).
//!
//! Stores variable-length string/field payloads and logical key headers behind
//! 64-bit generational handles `(shard_u8 << 56) | (generation_u24 << 32) | index_u32`
//! so that `SyncExpanseOrderedBytesMap` stores only `u64` handles. Sharded into
//! 64 cache-line-padded `RwLock` shards to eliminate cross-core lock contention.
//! Zero `unsafe`.

use std::sync::RwLock;
use std::sync::atomic::{AtomicUsize, Ordering};

const NUM_SHARDS: usize = 64;
const GEN_MASK: u32 = 0x00FF_FFFF;

/// Logical Redis key type stored at `[PREFIX_STRING][key]`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KeyType {
    /// Redis String (`GET`, `SET`, `INCR`, ...).
    String,
    /// Redis Hash (`HSET`, `HGET`, `HSCAN`, ...).
    Hash,
    /// Redis Sorted Set (`ZADD`, `ZRANGE`, ...).
    ZSet,
    /// Sparse Bitmap (`SETBIT`, `GETBIT`, `BITCOUNT`).
    Bitmap,
}

impl KeyType {
    /// Returns the Redis `TYPE` command string name.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::String => "string",
            Self::Hash => "hash",
            Self::ZSet => "zset",
            Self::Bitmap => "string",
        }
    }
}

/// Payload stored inside a [`ValueSlab`] slot.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SlotPayload {
    /// String value bytes (also used for Hash field values).
    Bytes(Vec<u8>),
    /// Logical Hash header tracking field count.
    HashHeader {
        /// Number of fields currently stored in this Hash.
        len: u64,
    },
    /// Logical Sorted Set header tracking member count.
    ZSetHeader {
        /// Number of members currently stored in this Sorted Set.
        len: u64,
    },
    /// Logical Sparse Bitmap header referencing a namespace ID in the shared `SyncExpanseSet`.
    BitmapHeader {
        /// Unique 32-bit namespace ID packed into high 32 bits of `SyncExpanseSet` keys.
        id: u32,
        /// Number of set bits currently stored in this bitmap.
        bit_count: u64,
    },
}

impl SlotPayload {
    /// Returns the logical [`KeyType`] for top-level key headers.
    #[must_use]
    pub const fn key_type(&self) -> KeyType {
        match self {
            Self::Bytes(_) => KeyType::String,
            Self::HashHeader { .. } => KeyType::Hash,
            Self::ZSetHeader { .. } => KeyType::ZSet,
            Self::BitmapHeader { .. } => KeyType::Bitmap,
        }
    }

    /// Approximate heap footprint of this payload in bytes.
    #[must_use]
    pub fn byte_size(&self) -> usize {
        let base = core::mem::size_of::<SlotEntry>();
        match self {
            Self::Bytes(v) => base + v.len(),
            Self::HashHeader { .. } | Self::ZSetHeader { .. } | Self::BitmapHeader { .. } => base,
        }
    }
}

/// Stored metadata + payload for one live entry in [`ValueSlab`].
#[derive(Debug, Clone)]
pub struct SlotEntry {
    /// 24-bit approximate LRU clock (seconds modulo `2^24`).
    pub lru24: u32,
    /// Optional absolute expiration timestamp in Unix milliseconds.
    pub expiry_ms: Option<u64>,
    /// Value payload or collection header.
    pub payload: SlotPayload,
}

struct SlabSlot {
    generation: u32,
    entry: Option<SlotEntry>,
}

struct SlabInner {
    slots: Vec<SlabSlot>,
    free_list: Vec<u32>,
}

#[repr(align(64))]
struct SlabShard {
    inner: RwLock<SlabInner>,
}

impl SlabShard {
    fn new() -> Self {
        Self {
            inner: RwLock::new(SlabInner {
                slots: Vec::new(),
                free_list: Vec::new(),
            }),
        }
    }
}

/// Thread-safe, 64-shard generational in-memory value slab with atomic live-byte tracking.
pub struct ValueSlab {
    shards: Box<[SlabShard]>,
    next_shard: AtomicUsize,
    live_bytes: AtomicUsize,
}

impl Default for ValueSlab {
    fn default() -> Self {
        Self::new()
    }
}

impl ValueSlab {
    /// Creates an empty 64-shard value slab.
    #[must_use]
    pub fn new() -> Self {
        let mut shards = Vec::with_capacity(NUM_SHARDS);
        for _ in 0..NUM_SHARDS {
            shards.push(SlabShard::new());
        }
        Self {
            shards: shards.into_boxed_slice(),
            next_shard: AtomicUsize::new(0),
            live_bytes: AtomicUsize::new(0),
        }
    }

    #[inline]
    fn pack_handle(shard: usize, generation: u32, idx: u32) -> u64 {
        ((shard as u64) << 56) | (u64::from(generation & GEN_MASK) << 32) | u64::from(idx)
    }

    #[inline]
    fn unpack_handle(handle: u64) -> (usize, u32, usize) {
        let shard = ((handle >> 56) as usize) & (NUM_SHARDS - 1);
        let gen_id = ((handle >> 32) as u32) & GEN_MASK;
        let idx = (handle & 0xFFFF_FFFF) as usize;
        (shard, gen_id, idx)
    }

    #[inline]
    fn next_generation(cur: u32) -> u32 {
        let next = cur.wrapping_add(1) & GEN_MASK;
        if next == 0 { 1 } else { next }
    }

    /// Inserts `entry` into the slab and returns its 64-bit generational handle.
    pub fn insert(&self, entry: SlotEntry) -> u64 {
        let bytes = entry.payload.byte_size();
        let shard_idx = self.next_shard.fetch_add(1, Ordering::Relaxed) & (NUM_SHARDS - 1);
        let mut guard = self.shards[shard_idx]
            .inner
            .write()
            .expect("value slab lock poisoned");
        let handle = if let Some(idx) = guard.free_list.pop() {
            let slot = &mut guard.slots[idx as usize];
            slot.generation = Self::next_generation(slot.generation);
            slot.entry = Some(entry);
            Self::pack_handle(shard_idx, slot.generation, idx)
        } else {
            let idx = u32::try_from(guard.slots.len()).expect("slab index fits in u32");
            guard.slots.push(SlabSlot {
                generation: 1,
                entry: Some(entry),
            });
            Self::pack_handle(shard_idx, 1, idx)
        };
        drop(guard);
        self.live_bytes.fetch_add(bytes, Ordering::Relaxed);
        handle
    }

    /// Clones the live [`SlotEntry`] for `handle`, or returns `None` if stale/freed.
    #[must_use]
    pub fn get(&self, handle: u64) -> Option<SlotEntry> {
        let (shard_idx, gen_id, idx) = Self::unpack_handle(handle);
        let guard = self.shards[shard_idx]
            .inner
            .read()
            .expect("value slab lock poisoned");
        let slot = guard.slots.get(idx)?;
        if slot.generation != gen_id {
            return None;
        }
        slot.entry.clone()
    }

    /// Runs `f` with a shared reference to the live [`SlotEntry`], returning `None` if stale.
    pub fn with_entry<R>(&self, handle: u64, f: impl FnOnce(&SlotEntry) -> R) -> Option<R> {
        let (shard_idx, gen_id, idx) = Self::unpack_handle(handle);
        let guard = self.shards[shard_idx]
            .inner
            .read()
            .expect("value slab lock poisoned");
        let slot = guard.slots.get(idx)?;
        if slot.generation != gen_id {
            return None;
        }
        slot.entry.as_ref().map(f)
    }

    /// Runs `f` with a mutable reference to the live [`SlotEntry`], updating `live_bytes`.
    pub fn update_entry<R>(&self, handle: u64, f: impl FnOnce(&mut SlotEntry) -> R) -> Option<R> {
        let (shard_idx, gen_id, idx) = Self::unpack_handle(handle);
        let mut guard = self.shards[shard_idx]
            .inner
            .write()
            .expect("value slab lock poisoned");
        let slot = guard.slots.get_mut(idx)?;
        if slot.generation != gen_id {
            return None;
        }
        let entry = slot.entry.as_mut()?;
        let before = entry.payload.byte_size();
        let res = f(entry);
        let after = entry.payload.byte_size();
        drop(guard);
        if after >= before {
            self.live_bytes.fetch_add(after - before, Ordering::Relaxed);
        } else {
            self.live_bytes.fetch_sub(before - after, Ordering::Relaxed);
        }
        Some(res)
    }

    /// Removes and returns the [`SlotEntry`] for `handle`, returning `None` if already freed.
    pub fn remove(&self, handle: u64) -> Option<SlotEntry> {
        let (shard_idx, gen_id, idx) = Self::unpack_handle(handle);
        let mut guard = self.shards[shard_idx]
            .inner
            .write()
            .expect("value slab lock poisoned");
        let slot = guard.slots.get_mut(idx)?;
        if slot.generation != gen_id {
            return None;
        }
        let old = slot.entry.take()?;
        let bytes = old.payload.byte_size();
        slot.generation = Self::next_generation(slot.generation);
        guard.free_list.push(idx as u32);
        drop(guard);
        self.live_bytes.fetch_sub(bytes, Ordering::Relaxed);
        Some(old)
    }

    /// Removes all entries from all shards and resets live bytes to zero.
    pub fn clear(&self) {
        for shard in &self.shards {
            let mut guard = shard.inner.write().expect("value slab lock poisoned");
            guard.slots.clear();
            guard.free_list.clear();
        }
        self.live_bytes.store(0, Ordering::Relaxed);
    }

    /// Approximate live bytes currently stored in the slab.
    #[must_use]
    pub fn live_bytes(&self) -> usize {
        self.live_bytes.load(Ordering::Relaxed)
    }
}
