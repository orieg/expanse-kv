//! Lock-free server metrics and Prometheus text exposition renderer.

use std::sync::atomic::{AtomicU64, Ordering};

use expanse_kv_engine::Engine;

/// Cache-line-padded atomic counter to prevent cross-core false sharing.
#[repr(align(64))]
#[derive(Debug, Default)]
pub struct PaddedCounter(pub AtomicU64);

impl PaddedCounter {
    /// Atomically adds `v` to the counter.
    #[inline]
    pub fn fetch_add(&self, v: u64, order: Ordering) -> u64 {
        self.0.fetch_add(v, order)
    }

    /// Atomically subtracts `v` from the counter.
    #[inline]
    pub fn fetch_sub(&self, v: u64, order: Ordering) -> u64 {
        self.0.fetch_sub(v, order)
    }

    /// Loads the current counter value.
    #[inline]
    #[must_use]
    pub fn load(&self, order: Ordering) -> u64 {
        self.0.load(order)
    }
}

/// Atomic operational counters for the `expanse-kv` server (cache-line padded).
#[derive(Debug, Default)]
pub struct ServerMetrics {
    /// Total commands executed across all connections.
    pub commands_total: PaddedCounter,
    /// Total TCP connections accepted since startup.
    pub connections_total: PaddedCounter,
    /// Currently open client connections.
    pub connections_active: PaddedCounter,
    /// Total protocol/command errors returned to clients.
    pub errors_total: PaddedCounter,
    /// Total bytes read from client sockets.
    pub read_bytes_total: PaddedCounter,
    /// Total bytes written to client sockets.
    pub write_bytes_total: PaddedCounter,
    /// Total mutating commands appended to the AOF.
    pub aof_appends_total: PaddedCounter,
}

impl ServerMetrics {
    /// Renders metrics in Prometheus text exposition format (`text/plain; version=0.0.4`).
    #[must_use]
    pub fn render_prometheus(&self, engine: &Engine) -> String {
        let cmds = self.commands_total.load(Ordering::Relaxed);
        let conns_total = self.connections_total.load(Ordering::Relaxed);
        let conns_active = self.connections_active.load(Ordering::Relaxed);
        let errors = self.errors_total.load(Ordering::Relaxed);
        let read_bytes = self.read_bytes_total.load(Ordering::Relaxed);
        let write_bytes = self.write_bytes_total.load(Ordering::Relaxed);
        let aof_appends = self.aof_appends_total.load(Ordering::Relaxed);
        let keys = engine.dbsize();
        let mem_bytes = engine.mem_used();

        format!(
            concat!(
                "# HELP expanse_kv_commands_total Total RESP commands processed.\n",
                "# TYPE expanse_kv_commands_total counter\n",
                "expanse_kv_commands_total {cmds}\n",
                "# HELP expanse_kv_connections_total Total client connections accepted.\n",
                "# TYPE expanse_kv_connections_total counter\n",
                "expanse_kv_connections_total {conns_total}\n",
                "# HELP expanse_kv_connections_active Current active client connections.\n",
                "# TYPE expanse_kv_connections_active gauge\n",
                "expanse_kv_connections_active {conns_active}\n",
                "# HELP expanse_kv_errors_total Total command errors returned.\n",
                "# TYPE expanse_kv_errors_total counter\n",
                "expanse_kv_errors_total {errors}\n",
                "# HELP expanse_kv_read_bytes_total Total bytes read from client sockets.\n",
                "# TYPE expanse_kv_read_bytes_total counter\n",
                "expanse_kv_read_bytes_total {read_bytes}\n",
                "# HELP expanse_kv_write_bytes_total Total bytes written to client sockets.\n",
                "# TYPE expanse_kv_write_bytes_total counter\n",
                "expanse_kv_write_bytes_total {write_bytes}\n",
                "# HELP expanse_kv_aof_appends_total Total commands appended to the AOF.\n",
                "# TYPE expanse_kv_aof_appends_total counter\n",
                "expanse_kv_aof_appends_total {aof_appends}\n",
                "# HELP expanse_kv_keys Current number of live keys in the keyspace.\n",
                "# TYPE expanse_kv_keys gauge\n",
                "expanse_kv_keys {keys}\n",
                "# HELP expanse_kv_memory_used_bytes Accounted value-slab memory in bytes.\n",
                "# TYPE expanse_kv_memory_used_bytes gauge\n",
                "expanse_kv_memory_used_bytes {mem_bytes}\n",
            ),
            cmds = cmds,
            conns_total = conns_total,
            conns_active = conns_active,
            errors = errors,
            read_bytes = read_bytes,
            write_bytes = write_bytes,
            aof_appends = aof_appends,
            keys = keys,
            mem_bytes = mem_bytes,
        )
    }
}
