//! RESP2/RESP3 command router, per-connection state (`MULTI`/`EXEC`, `AUTH`,
//! `HELLO`), and AOF persistence integration (`docs/PRD.md` §4.2, §7 Phase 3).

use std::path::PathBuf;
use std::sync::Mutex;
use std::sync::atomic::Ordering;

use expanse_kv_core::resp::{
    encode_array_header, encode_bulk_string, encode_integer, encode_null, encode_null_bulk,
    encode_simple_error, encode_simple_string,
};
use expanse_kv_engine::{Engine, EngineError, EngineReader, KeyType, ScoreBound, SetOptions};
use expanse_kv_persist::aof::{AofWriter, replay_aof};

fn encode_map_header(len: usize, out: &mut Vec<u8>) {
    out.extend_from_slice(format!("%{len}\r\n").as_bytes());
}

fn encode_double(v: f64, out: &mut Vec<u8>) {
    let s = format_score(v);
    out.push(b',');
    out.extend_from_slice(s.as_bytes());
    out.extend_from_slice(b"\r\n");
}

use crate::config::ServerConfig;
use crate::metrics::ServerMetrics;

/// Shared server state accessed concurrently by all client connections.
pub struct ServerState {
    /// Core in-memory `expanse-trie` storage engine.
    pub engine: Engine,
    /// Lock-free Prometheus metrics counters.
    pub metrics: ServerMetrics,
    /// Optional password required before executing commands.
    pub requirepass: Option<Vec<u8>>,
    /// Optional Append-Only File writer protected by a mutex.
    pub aof: Option<Mutex<AofWriter>>,
    /// Optional snapshot path.
    pub snapshot_path: Option<PathBuf>,
}

impl ServerState {
    /// Builds a new [`ServerState`] from `config`, replaying any existing AOF
    /// file before opening it for new appends.
    ///
    /// # Errors
    /// Returns an error string if AOF replay or opening fails.
    pub fn from_config(config: &ServerConfig) -> Result<Self, String> {
        let engine = Engine::new(config.engine);
        let aof = match &config.aof_path {
            Some(path) => {
                if path.exists() {
                    let mut replay_state = ConnState::new(true);
                    let mut sink = Vec::new();
                    let temp = Self {
                        engine: Engine::new(config.engine),
                        metrics: ServerMetrics::default(),
                        requirepass: None,
                        aof: None,
                        snapshot_path: None,
                    };
                    let mut reader = temp.engine.reader();
                    let _stats = replay_aof(path, true, |cmd| {
                        sink.clear();
                        temp.dispatch_direct(cmd, &mut replay_state, &mut reader, &mut sink, false);
                        Ok(())
                    })
                    .map_err(|e| format!("failed to replay AOF `{}`: {e}", path.display()))?;
                    let writer = AofWriter::open(path, config.fsync_policy)
                        .map_err(|e| format!("failed to open AOF `{}`: {e}", path.display()))?;
                    return Ok(Self {
                        engine: temp.engine,
                        metrics: ServerMetrics::default(),
                        requirepass: config.requirepass.as_ref().map(|s| s.as_bytes().to_vec()),
                        aof: Some(Mutex::new(writer)),
                        snapshot_path: config.snapshot_path.clone(),
                    });
                }
                let writer = AofWriter::open(path, config.fsync_policy)
                    .map_err(|e| format!("failed to open AOF `{}`: {e}", path.display()))?;
                Some(Mutex::new(writer))
            }
            None => None,
        };

        Ok(Self {
            engine,
            metrics: ServerMetrics::default(),
            requirepass: config.requirepass.as_ref().map(|s| s.as_bytes().to_vec()),
            aof,
            snapshot_path: config.snapshot_path.clone(),
        })
    }

    /// Flushes and fsyncs the AOF file during graceful shutdown.
    ///
    /// # Errors
    /// Returns an error string if flushing or fsyncing fails.
    pub fn flush_aof(&self) -> Result<(), String> {
        if let Some(aof_mutex) = &self.aof {
            let mut guard = aof_mutex
                .lock()
                .map_err(|_| "AOF mutex poisoned".to_string())?;
            guard
                .flush_and_sync()
                .map_err(|e| format!("AOF flush failed: {e}"))?;
        }
        Ok(())
    }

    /// Dispatches one parsed RESP command using a caller-reused [`EngineReader`],
    /// handling `AUTH`, `MULTI`/`EXEC`, `DISCARD`, AOF logging, and response
    /// encoding into `out`.
    pub fn handle_command(
        &self,
        args: &[&[u8]],
        conn: &mut ConnState,
        reader: &mut EngineReader<'_>,
        out: &mut Vec<u8>,
    ) {
        if args.is_empty() {
            return;
        }

        let mut cmd_buf = [0u8; 24];
        let cmd = uppercase_stack(args[0], &mut cmd_buf);

        // Commands allowed before AUTH
        if !conn.authenticated && !matches!(cmd, b"AUTH" | b"HELLO" | b"PING" | b"QUIT") {
            self.metrics.errors_total.fetch_add(1, Ordering::Relaxed);
            encode_simple_error(b"NOAUTH Authentication required.", out);
            return;
        }

        // Fast-path common read/write commands without transaction check overhead when !in_multi
        if !conn.in_multi {
            match cmd {
                b"GET" => {
                    if args.len() != 2 {
                        self.err_arity("get", out);
                    } else {
                        match self
                            .engine
                            .get_with_reader(reader, args[1], |v| encode_bulk_string(v, out))
                        {
                            Ok(Some(())) => {}
                            Ok(None) => encode_nil(conn.resp3, out),
                            Err(e) => self.err_engine(e, out),
                        }
                    }
                    return;
                }
                b"SET" => {
                    let mutated = self.handle_set(args, conn.resp3, reader, out);
                    if mutated {
                        self.append_aof(args);
                    }
                    return;
                }
                _ => {}
            }
        }

        // Transaction control
        match cmd {
            b"MULTI" => {
                if conn.in_multi {
                    self.metrics.errors_total.fetch_add(1, Ordering::Relaxed);
                    encode_simple_error(b"ERR MULTI calls can not be nested", out);
                } else {
                    conn.in_multi = true;
                    conn.queued.clear();
                    encode_simple_string(b"OK", out);
                }
                return;
            }
            b"DISCARD" => {
                if !conn.in_multi {
                    self.metrics.errors_total.fetch_add(1, Ordering::Relaxed);
                    encode_simple_error(b"ERR DISCARD without MULTI", out);
                } else {
                    conn.in_multi = false;
                    conn.queued.clear();
                    encode_simple_string(b"OK", out);
                }
                return;
            }
            b"EXEC" => {
                if !conn.in_multi {
                    self.metrics.errors_total.fetch_add(1, Ordering::Relaxed);
                    encode_simple_error(b"ERR EXEC without MULTI", out);
                    return;
                }
                conn.in_multi = false;
                let queued = std::mem::take(&mut conn.queued);
                encode_array_header(queued.len(), out);
                for qcmd in &queued {
                    let refs: Vec<&[u8]> = qcmd.iter().map(Vec::as_slice).collect();
                    self.dispatch_direct(&refs, conn, reader, out, true);
                }
                return;
            }
            _ => {}
        }

        if conn.in_multi {
            conn.queued.push(args.iter().map(|s| s.to_vec()).collect());
            encode_simple_string(b"QUEUED", out);
            return;
        }

        self.dispatch_direct(args, conn, reader, out, true);
    }

    fn append_aof(&self, args: &[&[u8]]) {
        if let Some(aof_mutex) = &self.aof
            && let Ok(mut guard) = aof_mutex.lock()
            && guard.append(args).is_ok()
        {
            self.metrics
                .aof_appends_total
                .fetch_add(1, Ordering::Relaxed);
        }
    }

    fn dispatch_direct(
        &self,
        args: &[&[u8]],
        conn: &mut ConnState,
        reader: &mut EngineReader<'_>,
        out: &mut Vec<u8>,
        log_aof: bool,
    ) {
        if args.is_empty() {
            return;
        }
        let mut cmd_buf = [0u8; 24];
        let cmd = uppercase_stack(args[0], &mut cmd_buf);
        let mut mutated = false;

        match cmd {
            // --- Connection & Server ---
            b"PING" => match args.len() {
                1 => encode_simple_string(b"PONG", out),
                2 => encode_bulk_string(args[1], out),
                _ => self.err_arity("ping", out),
            },
            b"ECHO" => {
                if args.len() != 2 {
                    self.err_arity("echo", out);
                } else {
                    encode_bulk_string(args[1], out);
                }
            }
            b"QUIT" => {
                conn.should_close = true;
                encode_simple_string(b"OK", out);
            }
            b"AUTH" => {
                if args.len() < 2 || args.len() > 3 {
                    self.err_arity("auth", out);
                } else {
                    let supplied = if args.len() == 3 { args[2] } else { args[1] };
                    match &self.requirepass {
                        Some(expected) if supplied == expected.as_slice() => {
                            conn.authenticated = true;
                            encode_simple_string(b"OK", out);
                        }
                        Some(_) => {
                            self.metrics.errors_total.fetch_add(1, Ordering::Relaxed);
                            encode_simple_error(b"WRONGPASS invalid password", out);
                        }
                        None => {
                            conn.authenticated = true;
                            encode_simple_string(b"OK", out);
                        }
                    }
                }
            }
            b"HELLO" => {
                self.handle_hello(args, conn, out);
            }
            b"CLIENT" => {
                if args.len() >= 2 {
                    let sub = ascii_upper(args[1]);
                    match sub.as_slice() {
                        b"SETNAME" if args.len() == 3 => {
                            conn.client_name = Some(args[2].to_vec());
                            encode_simple_string(b"OK", out);
                        }
                        b"GETNAME" => match &conn.client_name {
                            Some(name) => encode_bulk_string(name, out),
                            None => encode_nil(conn.resp3, out),
                        },
                        b"ID" => encode_integer(conn.id as i64, out),
                        _ => encode_simple_string(b"OK", out),
                    }
                } else {
                    self.err_arity("client", out);
                }
            }
            b"DBSIZE" => {
                encode_integer(self.engine.dbsize() as i64, out);
            }
            b"FLUSHDB" | b"FLUSHALL" => {
                self.engine.flushdb();
                mutated = true;
                encode_simple_string(b"OK", out);
            }
            b"INFO" => {
                let info = format!(
                    "# Server\r\nredis_version:7.2.0-expanse-kv\r\n\r\n# Keyspace\r\ndb0:keys={}\r\n# Memory\r\nused_memory:{}\r\n",
                    self.engine.dbsize(),
                    self.engine.mem_used()
                );
                encode_bulk_string(info.as_bytes(), out);
            }
            b"MEMORY" => {
                if args.len() >= 3 && ascii_upper(args[1]).as_slice() == b"USAGE" {
                    match self.engine.memory_usage(args[2]) {
                        Some(bytes) => encode_integer(bytes as i64, out),
                        None => encode_nil(conn.resp3, out),
                    }
                } else {
                    self.err_syntax(out);
                }
            }
            b"COMMAND" => {
                if args.len() >= 2 && ascii_upper(args[1]).as_slice() == b"DOCS" {
                    if conn.resp3 {
                        encode_map_header(0, out);
                    } else {
                        encode_array_header(0, out);
                    }
                } else {
                    encode_array_header(0, out);
                }
            }

            // --- Strings ---
            b"GET" => {
                if args.len() != 2 {
                    self.err_arity("get", out);
                } else {
                    match self
                        .engine
                        .get_with_reader(reader, args[1], |v| encode_bulk_string(v, out))
                    {
                        Ok(Some(())) => {}
                        Ok(None) => encode_nil(conn.resp3, out),
                        Err(e) => self.err_engine(e, out),
                    }
                }
            }
            b"SET" => {
                mutated = self.handle_set(args, conn.resp3, reader, out);
            }
            b"MGET" => {
                if args.len() < 2 {
                    self.err_arity("mget", out);
                } else {
                    let keys: Vec<&[u8]> = args[1..].to_vec();
                    let vals = self.engine.mget(&keys);
                    encode_array_header(vals.len(), out);
                    for v in vals {
                        match v {
                            Some(bytes) => encode_bulk_string(&bytes, out),
                            None => encode_nil(conn.resp3, out),
                        }
                    }
                }
            }
            b"MSET" => {
                if args.len() < 3 || ((args.len() - 1) & 1) != 0 {
                    self.err_arity("mset", out);
                } else {
                    let mut pairs = Vec::with_capacity((args.len() - 1) / 2);
                    let mut i = 1;
                    while i + 1 < args.len() {
                        pairs.push((args[i], args[i + 1]));
                        i += 2;
                    }
                    match self.engine.mset(&pairs) {
                        Ok(()) => {
                            mutated = true;
                            encode_simple_string(b"OK", out);
                        }
                        Err(e) => self.err_engine(e, out),
                    }
                }
            }
            b"INCR" => {
                if args.len() != 2 {
                    self.err_arity("incr", out);
                } else {
                    match self.engine.incr_by(args[1], 1) {
                        Ok(v) => {
                            mutated = true;
                            encode_integer(v, out);
                        }
                        Err(e) => self.err_engine(e, out),
                    }
                }
            }
            b"DECR" => {
                if args.len() != 2 {
                    self.err_arity("decr", out);
                } else {
                    match self.engine.incr_by(args[1], -1) {
                        Ok(v) => {
                            mutated = true;
                            encode_integer(v, out);
                        }
                        Err(e) => self.err_engine(e, out),
                    }
                }
            }
            b"INCRBY" => {
                if args.len() != 3 {
                    self.err_arity("incrby", out);
                } else if let Some(delta) = parse_i64(args[2]) {
                    match self.engine.incr_by(args[1], delta) {
                        Ok(v) => {
                            mutated = true;
                            encode_integer(v, out);
                        }
                        Err(e) => self.err_engine(e, out),
                    }
                } else {
                    self.err_engine(EngineError::NotAnInteger, out);
                }
            }
            b"DECRBY" => {
                if args.len() != 3 {
                    self.err_arity("decrby", out);
                } else if let Some(delta) = parse_i64(args[2]) {
                    match delta.checked_neg() {
                        Some(neg) => match self.engine.incr_by(args[1], neg) {
                            Ok(v) => {
                                mutated = true;
                                encode_integer(v, out);
                            }
                            Err(e) => self.err_engine(e, out),
                        },
                        None => self.err_engine(EngineError::Overflow, out),
                    }
                } else {
                    self.err_engine(EngineError::NotAnInteger, out);
                }
            }
            b"DEL" | b"UNLINK" => {
                if args.len() < 2 {
                    self.err_arity("del", out);
                } else {
                    let removed = self.engine.del(&args[1..]);
                    if removed > 0 {
                        mutated = true;
                    }
                    encode_integer(removed as i64, out);
                }
            }
            b"EXISTS" => {
                if args.len() < 2 {
                    self.err_arity("exists", out);
                } else {
                    let mut count = 0i64;
                    for &k in &args[1..] {
                        if self.engine.exists(k) {
                            count += 1;
                        }
                    }
                    encode_integer(count, out);
                }
            }
            b"TYPE" => {
                if args.len() != 2 {
                    self.err_arity("type", out);
                } else {
                    let name: &[u8] = match self.engine.key_type(args[1]) {
                        None => b"none",
                        Some(KeyType::String) => b"string",
                        Some(KeyType::Hash) => b"hash",
                        Some(KeyType::ZSet) => b"zset",
                        Some(KeyType::Bitmap) => b"string",
                    };
                    encode_simple_string(name, out);
                }
            }
            b"EXPIRE" => {
                if args.len() != 3 {
                    self.err_arity("expire", out);
                } else if let Some(secs) = parse_i64(args[2]) {
                    let target = if secs <= 0 {
                        0
                    } else {
                        self.engine
                            .now_ms()
                            .saturating_add((secs as u64).saturating_mul(1000))
                    };
                    let ok = self.engine.pexpire_at(args[1], target);
                    if ok {
                        mutated = true;
                    }
                    encode_integer(i64::from(ok), out);
                } else {
                    self.err_engine(EngineError::NotAnInteger, out);
                }
            }
            b"PEXPIRE" => {
                if args.len() != 3 {
                    self.err_arity("pexpire", out);
                } else if let Some(ms) = parse_i64(args[2]) {
                    let target = if ms <= 0 {
                        0
                    } else {
                        self.engine.now_ms().saturating_add(ms as u64)
                    };
                    let ok = self.engine.pexpire_at(args[1], target);
                    if ok {
                        mutated = true;
                    }
                    encode_integer(i64::from(ok), out);
                } else {
                    self.err_engine(EngineError::NotAnInteger, out);
                }
            }
            b"EXPIREAT" => {
                if args.len() != 3 {
                    self.err_arity("expireat", out);
                } else if let Some(ts) = parse_i64(args[2]) {
                    let target = if ts <= 0 {
                        0
                    } else {
                        (ts as u64).saturating_mul(1000)
                    };
                    let ok = self.engine.pexpire_at(args[1], target);
                    if ok {
                        mutated = true;
                    }
                    encode_integer(i64::from(ok), out);
                } else {
                    self.err_engine(EngineError::NotAnInteger, out);
                }
            }
            b"PEXPIREAT" => {
                if args.len() != 3 {
                    self.err_arity("pexpireat", out);
                } else if let Some(ts) = parse_i64(args[2]) {
                    let target = if ts <= 0 { 0 } else { ts as u64 };
                    let ok = self.engine.pexpire_at(args[1], target);
                    if ok {
                        mutated = true;
                    }
                    encode_integer(i64::from(ok), out);
                } else {
                    self.err_engine(EngineError::NotAnInteger, out);
                }
            }
            b"TTL" => {
                if args.len() != 2 {
                    self.err_arity("ttl", out);
                } else {
                    let pttl = self.engine.pttl(args[1]);
                    let ttl = if pttl < 0 { pttl } else { (pttl + 500) / 1000 };
                    encode_integer(ttl, out);
                }
            }
            b"PTTL" => {
                if args.len() != 2 {
                    self.err_arity("pttl", out);
                } else {
                    encode_integer(self.engine.pttl(args[1]), out);
                }
            }
            b"PERSIST" => {
                if args.len() != 2 {
                    self.err_arity("persist", out);
                } else {
                    let ok = self.engine.persist(args[1]);
                    if ok {
                        mutated = true;
                    }
                    encode_integer(i64::from(ok), out);
                }
            }

            // --- Scans ---
            b"SCAN" => {
                self.handle_scan(args, out);
            }
            b"KEYS" => {
                if args.len() != 2 {
                    self.err_arity("keys", out);
                } else {
                    let prefix = extract_glob_prefix(args[1]);
                    let mut all = Vec::new();
                    let mut cursor: Option<Vec<u8>> = None;
                    loop {
                        let page = self.engine.scan(cursor.as_deref(), prefix, 1024);
                        for k in page.items {
                            if glob_match(args[1], &k) {
                                all.push(k);
                            }
                        }
                        if page.next_cursor.is_none() {
                            break;
                        }
                        cursor = page.next_cursor;
                    }
                    encode_array_header(all.len(), out);
                    for k in &all {
                        encode_bulk_string(k, out);
                    }
                }
            }

            // --- Hashes ---
            b"HSET" => {
                if args.len() < 4 || ((args.len() - 2) & 1) != 0 {
                    self.err_arity("hset", out);
                } else {
                    let mut pairs = Vec::with_capacity((args.len() - 2) / 2);
                    let mut i = 2;
                    while i + 1 < args.len() {
                        pairs.push((args[i], args[i + 1]));
                        i += 2;
                    }
                    match self.engine.hset(args[1], &pairs) {
                        Ok(added) => {
                            mutated = true;
                            encode_integer(added as i64, out);
                        }
                        Err(e) => self.err_engine(e, out),
                    }
                }
            }
            b"HGET" => {
                if args.len() != 3 {
                    self.err_arity("hget", out);
                } else {
                    match self.engine.hget(args[1], args[2]) {
                        Ok(Some(v)) => encode_bulk_string(&v, out),
                        Ok(None) => encode_nil(conn.resp3, out),
                        Err(e) => self.err_engine(e, out),
                    }
                }
            }
            b"HMGET" => {
                if args.len() < 3 {
                    self.err_arity("hmget", out);
                } else {
                    let mut vals = Vec::with_capacity(args.len() - 2);
                    for &field in &args[2..] {
                        match self.engine.hget(args[1], field) {
                            Ok(v) => vals.push(v),
                            Err(e) => {
                                self.err_engine(e, out);
                                return;
                            }
                        }
                    }
                    encode_array_header(vals.len(), out);
                    for v in vals {
                        match v {
                            Some(bytes) => encode_bulk_string(&bytes, out),
                            None => encode_nil(conn.resp3, out),
                        }
                    }
                }
            }
            b"HDEL" => {
                if args.len() < 3 {
                    self.err_arity("hdel", out);
                } else {
                    match self.engine.hdel(args[1], &args[2..]) {
                        Ok(removed) => {
                            if removed > 0 {
                                mutated = true;
                            }
                            encode_integer(removed as i64, out);
                        }
                        Err(e) => self.err_engine(e, out),
                    }
                }
            }
            b"HEXISTS" => {
                if args.len() != 3 {
                    self.err_arity("hexists", out);
                } else {
                    match self.engine.hget(args[1], args[2]) {
                        Ok(Some(_)) => encode_integer(1, out),
                        Ok(None) => encode_integer(0, out),
                        Err(e) => self.err_engine(e, out),
                    }
                }
            }
            b"HLEN" => {
                if args.len() != 2 {
                    self.err_arity("hlen", out);
                } else {
                    match self.engine.hlen(args[1]) {
                        Ok(n) => encode_integer(n as i64, out),
                        Err(e) => self.err_engine(e, out),
                    }
                }
            }
            b"HGETALL" => {
                if args.len() != 2 {
                    self.err_arity("hgetall", out);
                } else {
                    let mut all = Vec::new();
                    let mut cursor: Option<Vec<u8>> = None;
                    loop {
                        match self.engine.hscan(args[1], cursor.as_deref(), 1024) {
                            Ok(page) => {
                                all.extend(page.items);
                                if page.next_cursor.is_none() {
                                    break;
                                }
                                cursor = page.next_cursor;
                            }
                            Err(e) => {
                                self.err_engine(e, out);
                                return;
                            }
                        }
                    }
                    if conn.resp3 {
                        encode_map_header(all.len(), out);
                    } else {
                        encode_array_header(all.len() * 2, out);
                    }
                    for (f, v) in &all {
                        encode_bulk_string(f, out);
                        encode_bulk_string(v, out);
                    }
                }
            }
            b"HSCAN" => {
                self.handle_hscan(args, out);
            }

            // --- Sorted Sets ---
            b"ZADD" => {
                if args.len() < 4 || ((args.len() - 2) & 1) != 0 {
                    self.err_arity("zadd", out);
                } else {
                    let mut pairs = Vec::with_capacity((args.len() - 2) / 2);
                    let mut i = 2;
                    while i + 1 < args.len() {
                        match parse_f64(args[i]) {
                            Some(score) => pairs.push((score, args[i + 1])),
                            None => {
                                self.err_engine(EngineError::InvalidScore, out);
                                return;
                            }
                        }
                        i += 2;
                    }
                    match self.engine.zadd(args[1], &pairs) {
                        Ok(added) => {
                            mutated = true;
                            encode_integer(added as i64, out);
                        }
                        Err(e) => self.err_engine(e, out),
                    }
                }
            }
            b"ZREM" => {
                if args.len() < 3 {
                    self.err_arity("zrem", out);
                } else {
                    match self.engine.zrem(args[1], &args[2..]) {
                        Ok(removed) => {
                            if removed > 0 {
                                mutated = true;
                            }
                            encode_integer(removed as i64, out);
                        }
                        Err(e) => self.err_engine(e, out),
                    }
                }
            }
            b"ZSCORE" => {
                if args.len() != 3 {
                    self.err_arity("zscore", out);
                } else {
                    match self.engine.zscore(args[1], args[2]) {
                        Ok(Some(s)) => {
                            if conn.resp3 {
                                encode_double(s, out);
                            } else {
                                let formatted = format_score(s);
                                encode_bulk_string(formatted.as_bytes(), out);
                            }
                        }
                        Ok(None) => encode_nil(conn.resp3, out),
                        Err(e) => self.err_engine(e, out),
                    }
                }
            }
            b"ZCARD" => {
                if args.len() != 2 {
                    self.err_arity("zcard", out);
                } else {
                    match self.engine.zcard(args[1]) {
                        Ok(n) => encode_integer(n as i64, out),
                        Err(e) => self.err_engine(e, out),
                    }
                }
            }
            b"ZRANGE" => {
                self.handle_zrange(args, conn.resp3, false, out);
            }
            b"ZREVRANGE" => {
                self.handle_zrange(args, conn.resp3, true, out);
            }
            b"ZRANGEBYSCORE" => {
                self.handle_zrangebyscore(args, conn.resp3, out);
            }
            b"ZREMRANGEBYSCORE" => {
                if args.len() != 4 {
                    self.err_arity("zremrangebyscore", out);
                } else {
                    let Some(min) = parse_score_bound(args[2]) else {
                        self.err_engine(EngineError::InvalidScore, out);
                        return;
                    };
                    let Some(max) = parse_score_bound(args[3]) else {
                        self.err_engine(EngineError::InvalidScore, out);
                        return;
                    };
                    match self.engine.zremrangebyscore(args[1], &min, &max) {
                        Ok(removed) => {
                            if removed > 0 {
                                mutated = true;
                            }
                            encode_integer(removed as i64, out);
                        }
                        Err(e) => self.err_engine(e, out),
                    }
                }
            }

            // --- Bitmaps ---
            b"SETBIT" => {
                if args.len() != 4 {
                    self.err_arity("setbit", out);
                } else {
                    let Some(offset) = parse_u64(args[2]) else {
                        self.err_engine(EngineError::BitOffsetOutOfRange, out);
                        return;
                    };
                    let bit = match args[3] {
                        b"0" => false,
                        b"1" => true,
                        _ => {
                            self.metrics.errors_total.fetch_add(1, Ordering::Relaxed);
                            encode_simple_error(b"ERR bit is not an integer or out of range", out);
                            return;
                        }
                    };
                    match self.engine.setbit(args[1], offset, bit) {
                        Ok(prev) => {
                            mutated = true;
                            encode_integer(i64::from(prev), out);
                        }
                        Err(e) => self.err_engine(e, out),
                    }
                }
            }
            b"GETBIT" => {
                if args.len() != 3 {
                    self.err_arity("getbit", out);
                } else if let Some(offset) = parse_u64(args[2]) {
                    match self.engine.getbit(args[1], offset) {
                        Ok(bit) => encode_integer(i64::from(bit), out),
                        Err(e) => self.err_engine(e, out),
                    }
                } else {
                    self.err_engine(EngineError::BitOffsetOutOfRange, out);
                }
            }
            b"BITCOUNT" => {
                if args.len() != 2 {
                    self.err_arity("bitcount", out);
                } else {
                    match self.engine.bitcount(args[1]) {
                        Ok(count) => encode_integer(count as i64, out),
                        Err(e) => self.err_engine(e, out),
                    }
                }
            }

            _ => {
                self.metrics.errors_total.fetch_add(1, Ordering::Relaxed);
                let msg = format!("ERR unknown command '{}'", String::from_utf8_lossy(args[0]));
                encode_simple_error(msg.as_bytes(), out);
            }
        }

        if log_aof && mutated {
            self.append_aof(args);
        }
    }

    fn handle_hello(&self, args: &[&[u8]], conn: &mut ConnState, out: &mut Vec<u8>) {
        let mut proto = if conn.resp3 { 3u8 } else { 2u8 };
        let mut i = 1usize;
        if i < args.len() {
            match args[i] {
                b"2" => {
                    proto = 2;
                    i += 1;
                }
                b"3" => {
                    proto = 3;
                    i += 1;
                }
                _ => {
                    self.metrics.errors_total.fetch_add(1, Ordering::Relaxed);
                    encode_simple_error(b"NOPROTO unsupported protocol version", out);
                    return;
                }
            }
        }
        while i < args.len() {
            let opt = ascii_upper(args[i]);
            match opt.as_slice() {
                b"AUTH" if i + 2 < args.len() => {
                    let supplied = args[i + 2];
                    if let Some(expected) = &self.requirepass
                        && supplied != expected.as_slice()
                    {
                        self.metrics.errors_total.fetch_add(1, Ordering::Relaxed);
                        encode_simple_error(b"WRONGPASS invalid password", out);
                        return;
                    }
                    conn.authenticated = true;
                    i += 3;
                }
                b"SETNAME" if i + 1 < args.len() => {
                    conn.client_name = Some(args[i + 1].to_vec());
                    i += 2;
                }
                _ => {
                    self.err_syntax(out);
                    return;
                }
            }
        }
        if !conn.authenticated && self.requirepass.is_some() {
            self.metrics.errors_total.fetch_add(1, Ordering::Relaxed);
            encode_simple_error(b"NOAUTH Authentication required.", out);
            return;
        }
        conn.resp3 = proto == 3;
        if conn.resp3 {
            encode_map_header(4, out);
        } else {
            encode_array_header(8, out);
        }
        encode_bulk_string(b"server", out);
        encode_bulk_string(b"expanse-kv", out);
        encode_bulk_string(b"version", out);
        encode_bulk_string(env!("CARGO_PKG_VERSION").as_bytes(), out);
        encode_bulk_string(b"proto", out);
        encode_integer(i64::from(proto), out);
        encode_bulk_string(b"id", out);
        encode_integer(conn.id as i64, out);
    }

    fn handle_set(
        &self,
        args: &[&[u8]],
        resp3: bool,
        reader: &mut EngineReader<'_>,
        out: &mut Vec<u8>,
    ) -> bool {
        if args.len() < 3 {
            self.err_arity("set", out);
            return false;
        }
        let mut opts = SetOptions::default();
        let mut i = 3usize;
        while i < args.len() {
            let flag = ascii_upper(args[i]);
            match flag.as_slice() {
                b"NX" => opts.nx = true,
                b"XX" => opts.xx = true,
                b"KEEPTTL" => opts.keep_ttl = true,
                b"GET" => opts.get_prev = true,
                b"EX" => {
                    i += 1;
                    let Some(secs) = args.get(i).and_then(|s| parse_i64(s)) else {
                        self.err_syntax(out);
                        return false;
                    };
                    if secs <= 0 {
                        self.metrics.errors_total.fetch_add(1, Ordering::Relaxed);
                        encode_simple_error(b"ERR invalid expire time in 'set' command", out);
                        return false;
                    }
                    opts.expiry_ms = Some(
                        self.engine
                            .now_ms()
                            .saturating_add((secs as u64).saturating_mul(1000)),
                    );
                }
                b"PX" => {
                    i += 1;
                    let Some(ms) = args.get(i).and_then(|s| parse_i64(s)) else {
                        self.err_syntax(out);
                        return false;
                    };
                    if ms <= 0 {
                        self.metrics.errors_total.fetch_add(1, Ordering::Relaxed);
                        encode_simple_error(b"ERR invalid expire time in 'set' command", out);
                        return false;
                    }
                    opts.expiry_ms = Some(self.engine.now_ms().saturating_add(ms as u64));
                }
                b"EXAT" => {
                    i += 1;
                    let Some(ts) = args.get(i).and_then(|s| parse_i64(s)) else {
                        self.err_syntax(out);
                        return false;
                    };
                    opts.expiry_ms = Some(if ts <= 0 {
                        0
                    } else {
                        (ts as u64).saturating_mul(1000)
                    });
                }
                b"PXAT" => {
                    i += 1;
                    let Some(ts) = args.get(i).and_then(|s| parse_i64(s)) else {
                        self.err_syntax(out);
                        return false;
                    };
                    opts.expiry_ms = Some(if ts <= 0 { 0 } else { ts as u64 });
                }
                _ => {
                    self.err_syntax(out);
                    return false;
                }
            }
            i += 1;
        }

        match self.engine.set_with_reader(reader, args[1], args[2], opts) {
            Ok((written, prev)) => {
                if opts.get_prev {
                    match prev {
                        Some(p) => encode_bulk_string(&p, out),
                        None => encode_nil(resp3, out),
                    }
                } else if written {
                    encode_simple_string(b"OK", out);
                } else {
                    encode_nil(resp3, out);
                }
                written
            }
            Err(e) => {
                self.err_engine(e, out);
                false
            }
        }
    }

    fn handle_scan(&self, args: &[&[u8]], out: &mut Vec<u8>) {
        if args.len() < 2 {
            self.err_arity("scan", out);
            return;
        }
        let cursor = match decode_cursor(args[1]) {
            Ok(c) => c,
            Err(()) => {
                self.metrics.errors_total.fetch_add(1, Ordering::Relaxed);
                encode_simple_error(b"ERR invalid cursor", out);
                return;
            }
        };
        let mut pattern: Option<&[u8]> = None;
        let mut count: usize = 10;
        let mut i = 2usize;
        while i < args.len() {
            let opt = ascii_upper(args[i]);
            match opt.as_slice() {
                b"MATCH" if i + 1 < args.len() => {
                    pattern = Some(args[i + 1]);
                    i += 2;
                }
                b"COUNT" if i + 1 < args.len() => {
                    match parse_u64(args[i + 1]) {
                        Some(n) if n > 0 => count = n as usize,
                        _ => {
                            self.err_syntax(out);
                            return;
                        }
                    }
                    i += 2;
                }
                _ => {
                    self.err_syntax(out);
                    return;
                }
            }
        }

        let prefix = pattern.map_or(b"".as_slice(), extract_glob_prefix);
        let page = self.engine.scan(cursor.as_deref(), prefix, count);
        let next_cur = encode_cursor(page.next_cursor.as_deref());
        let matched: Vec<Vec<u8>> = match pattern {
            Some(pat) => page
                .items
                .into_iter()
                .filter(|k| glob_match(pat, k))
                .collect(),
            None => page.items,
        };

        encode_array_header(2, out);
        encode_bulk_string(next_cur.as_bytes(), out);
        encode_array_header(matched.len(), out);
        for k in &matched {
            encode_bulk_string(k, out);
        }
    }

    fn handle_hscan(&self, args: &[&[u8]], out: &mut Vec<u8>) {
        if args.len() < 3 {
            self.err_arity("hscan", out);
            return;
        }
        let cursor = match decode_cursor(args[2]) {
            Ok(c) => c,
            Err(()) => {
                self.metrics.errors_total.fetch_add(1, Ordering::Relaxed);
                encode_simple_error(b"ERR invalid cursor", out);
                return;
            }
        };
        let mut pattern: Option<&[u8]> = None;
        let mut count: usize = 10;
        let mut i = 3usize;
        while i < args.len() {
            let opt = ascii_upper(args[i]);
            match opt.as_slice() {
                b"MATCH" if i + 1 < args.len() => {
                    pattern = Some(args[i + 1]);
                    i += 2;
                }
                b"COUNT" if i + 1 < args.len() => {
                    match parse_u64(args[i + 1]) {
                        Some(n) if n > 0 => count = n as usize,
                        _ => {
                            self.err_syntax(out);
                            return;
                        }
                    }
                    i += 2;
                }
                _ => {
                    self.err_syntax(out);
                    return;
                }
            }
        }

        match self.engine.hscan(args[1], cursor.as_deref(), count) {
            Ok(page) => {
                let next_cur = encode_cursor(page.next_cursor.as_deref());
                let matched: Vec<(Vec<u8>, Vec<u8>)> = match pattern {
                    Some(pat) => page
                        .items
                        .into_iter()
                        .filter(|(f, _)| glob_match(pat, f))
                        .collect(),
                    None => page.items,
                };
                encode_array_header(2, out);
                encode_bulk_string(next_cur.as_bytes(), out);
                encode_array_header(matched.len() * 2, out);
                for (f, v) in &matched {
                    encode_bulk_string(f, out);
                    encode_bulk_string(v, out);
                }
            }
            Err(e) => self.err_engine(e, out),
        }
    }

    fn handle_zrange(&self, args: &[&[u8]], resp3: bool, default_rev: bool, out: &mut Vec<u8>) {
        if args.len() < 4 {
            self.err_arity("zrange", out);
            return;
        }
        let mut by_score = false;
        let mut rev = default_rev;
        let mut with_scores = false;
        let mut offset = 0usize;
        let mut limit: Option<usize> = None;

        let mut i = 4usize;
        while i < args.len() {
            let opt = ascii_upper(args[i]);
            match opt.as_slice() {
                b"BYSCORE" => {
                    by_score = true;
                    i += 1;
                }
                b"REV" => {
                    rev = true;
                    i += 1;
                }
                b"WITHSCORES" => {
                    with_scores = true;
                    i += 1;
                }
                b"LIMIT" if i + 2 < args.len() => {
                    let Some(off) = parse_u64(args[i + 1]) else {
                        self.err_syntax(out);
                        return;
                    };
                    let Some(cnt) = parse_i64(args[i + 2]) else {
                        self.err_syntax(out);
                        return;
                    };
                    offset = off as usize;
                    limit = if cnt < 0 { None } else { Some(cnt as usize) };
                    i += 3;
                }
                _ => {
                    self.err_syntax(out);
                    return;
                }
            }
        }

        let res = if by_score {
            let (min_raw, max_raw) = if rev {
                (args[3], args[2])
            } else {
                (args[2], args[3])
            };
            let Some(min) = parse_score_bound(min_raw) else {
                self.err_engine(EngineError::InvalidScore, out);
                return;
            };
            let Some(max) = parse_score_bound(max_raw) else {
                self.err_engine(EngineError::InvalidScore, out);
                return;
            };
            self.engine
                .zrange_by_score(args[1], &min, &max, rev, offset, limit)
        } else {
            let Some(start) = parse_i64(args[2]) else {
                self.err_engine(EngineError::NotAnInteger, out);
                return;
            };
            let Some(stop) = parse_i64(args[3]) else {
                self.err_engine(EngineError::NotAnInteger, out);
                return;
            };
            self.engine.zrange_by_rank(args[1], start, stop, rev)
        };

        match res {
            Ok(items) => encode_zrange_items(&items, with_scores, resp3, out),
            Err(e) => self.err_engine(e, out),
        }
    }

    fn handle_zrangebyscore(&self, args: &[&[u8]], resp3: bool, out: &mut Vec<u8>) {
        if args.len() < 4 {
            self.err_arity("zrangebyscore", out);
            return;
        }
        let Some(min) = parse_score_bound(args[2]) else {
            self.err_engine(EngineError::InvalidScore, out);
            return;
        };
        let Some(max) = parse_score_bound(args[3]) else {
            self.err_engine(EngineError::InvalidScore, out);
            return;
        };
        let mut with_scores = false;
        let mut offset = 0usize;
        let mut limit: Option<usize> = None;
        let mut i = 4usize;
        while i < args.len() {
            let opt = ascii_upper(args[i]);
            match opt.as_slice() {
                b"WITHSCORES" => {
                    with_scores = true;
                    i += 1;
                }
                b"LIMIT" if i + 2 < args.len() => {
                    let Some(off) = parse_u64(args[i + 1]) else {
                        self.err_syntax(out);
                        return;
                    };
                    let Some(cnt) = parse_i64(args[i + 2]) else {
                        self.err_syntax(out);
                        return;
                    };
                    offset = off as usize;
                    limit = if cnt < 0 { None } else { Some(cnt as usize) };
                    i += 3;
                }
                _ => {
                    self.err_syntax(out);
                    return;
                }
            }
        }
        match self
            .engine
            .zrange_by_score(args[1], &min, &max, false, offset, limit)
        {
            Ok(items) => encode_zrange_items(&items, with_scores, resp3, out),
            Err(e) => self.err_engine(e, out),
        }
    }

    fn err_arity(&self, cmd: &str, out: &mut Vec<u8>) {
        self.metrics.errors_total.fetch_add(1, Ordering::Relaxed);
        let msg = format!("ERR wrong number of arguments for '{cmd}' command");
        encode_simple_error(msg.as_bytes(), out);
    }

    fn err_syntax(&self, out: &mut Vec<u8>) {
        self.metrics.errors_total.fetch_add(1, Ordering::Relaxed);
        encode_simple_error(b"ERR syntax error", out);
    }

    fn err_engine(&self, err: EngineError, out: &mut Vec<u8>) {
        self.metrics.errors_total.fetch_add(1, Ordering::Relaxed);
        let msg = err.to_string();
        encode_simple_error(msg.as_bytes(), out);
    }
}

/// Per-connection protocol and transaction state.
#[derive(Debug)]
pub struct ConnState {
    /// Connection identifier.
    pub id: u64,
    /// `true` when negotiated to RESP3 via `HELLO 3`.
    pub resp3: bool,
    /// `true` once authenticated (or when no `requirepass` is configured).
    pub authenticated: bool,
    /// `true` between `MULTI` and `EXEC`/`DISCARD`.
    pub in_multi: bool,
    /// Queued commands inside `MULTI`.
    pub queued: Vec<Vec<Vec<u8>>>,
    /// Optional client name set via `CLIENT SETNAME`.
    pub client_name: Option<Vec<u8>>,
    /// Set to `true` after `QUIT` to close the socket cleanly.
    pub should_close: bool,
}

impl ConnState {
    /// Creates a new connection state.
    #[must_use]
    pub const fn new(authenticated: bool) -> Self {
        Self {
            id: 0,
            resp3: false,
            authenticated,
            in_multi: false,
            queued: Vec::new(),
            client_name: None,
            should_close: false,
        }
    }
}

fn encode_nil(resp3: bool, out: &mut Vec<u8>) {
    if resp3 {
        encode_null(out);
    } else {
        encode_null_bulk(out);
    }
}

fn encode_zrange_items(
    items: &[(Vec<u8>, f64)],
    with_scores: bool,
    resp3: bool,
    out: &mut Vec<u8>,
) {
    if with_scores {
        if resp3 {
            encode_array_header(items.len(), out);
            for (member, score) in items {
                encode_array_header(2, out);
                encode_bulk_string(member, out);
                encode_double(*score, out);
            }
        } else {
            encode_array_header(items.len() * 2, out);
            for (member, score) in items {
                encode_bulk_string(member, out);
                let s = format_score(*score);
                encode_bulk_string(s.as_bytes(), out);
            }
        }
    } else {
        encode_array_header(items.len(), out);
        for (member, _) in items {
            encode_bulk_string(member, out);
        }
    }
}

fn format_score(score: f64) -> String {
    if score == f64::INFINITY {
        "inf".to_string()
    } else if score == f64::NEG_INFINITY {
        "-inf".to_string()
    } else {
        format!("{score}")
    }
}

#[inline]
fn uppercase_stack<'a>(bytes: &[u8], buf: &'a mut [u8; 24]) -> &'a [u8] {
    if bytes.len() > buf.len() {
        return b"";
    }
    for (dst, &src) in buf.iter_mut().zip(bytes.iter()) {
        *dst = src.to_ascii_uppercase();
    }
    &buf[..bytes.len()]
}

fn ascii_upper(bytes: &[u8]) -> Vec<u8> {
    bytes.iter().map(u8::to_ascii_uppercase).collect()
}

fn parse_i64(bytes: &[u8]) -> Option<i64> {
    match std::str::from_utf8(bytes) {
        Ok(s) => s.parse::<i64>().ok(),
        Err(_) => None,
    }
}

fn parse_u64(bytes: &[u8]) -> Option<u64> {
    match std::str::from_utf8(bytes) {
        Ok(s) => s.parse::<u64>().ok(),
        Err(_) => None,
    }
}

fn parse_f64(bytes: &[u8]) -> Option<f64> {
    let s = match std::str::from_utf8(bytes) {
        Ok(s) => s,
        Err(_) => return None,
    };
    let lower = s.to_ascii_lowercase();
    let v = match lower.as_str() {
        "inf" | "+inf" => f64::INFINITY,
        "-inf" => f64::NEG_INFINITY,
        _ => match lower.parse::<f64>() {
            Ok(x) => x,
            Err(_) => return None,
        },
    };
    if v.is_nan() { None } else { Some(v) }
}

fn parse_score_bound(bytes: &[u8]) -> Option<ScoreBound> {
    if bytes == b"-inf" || bytes == b"+inf" || bytes == b"inf" {
        return Some(ScoreBound::Unbounded);
    }
    if let Some(rest) = bytes.strip_prefix(b"(") {
        parse_f64(rest).map(ScoreBound::Exclusive)
    } else {
        parse_f64(bytes).map(ScoreBound::Inclusive)
    }
}

/// Encodes a stateless bookmark cursor as hex (`"0"` when scan is complete).
fn encode_cursor(cursor: Option<&[u8]>) -> String {
    match cursor {
        None => "0".to_string(),
        Some(bytes) => {
            let mut s = String::with_capacity(2 + bytes.len() * 2);
            s.push_str("1:");
            for &b in bytes {
                s.push_str(&format!("{b:02x}"));
            }
            s
        }
    }
}

/// Decodes a `SCAN`/`HSCAN` cursor (`"0"` -> start, `"1:<hex>"` -> bookmark).
fn decode_cursor(raw: &[u8]) -> Result<Option<Vec<u8>>, ()> {
    if raw == b"0" {
        return Ok(None);
    }
    let Some(hex) = raw.strip_prefix(b"1:") else {
        return Err(());
    };
    if (hex.len() & 1) != 0 {
        return Err(());
    }
    let mut out = Vec::with_capacity(hex.len() / 2);
    let mut i = 0;
    while i < hex.len() {
        let hi = hex_nibble(hex[i])?;
        let lo = hex_nibble(hex[i + 1])?;
        out.push((hi << 4) | lo);
        i += 2;
    }
    Ok(Some(out))
}

fn hex_nibble(b: u8) -> Result<u8, ()> {
    match b {
        b'0'..=b'9' => Ok(b - b'0'),
        b'a'..=b'f' => Ok(b - b'a' + 10),
        b'A'..=b'F' => Ok(b - b'A' + 10),
        _ => Err(()),
    }
}

/// Extracts the literal byte prefix before the first glob metacharacter
/// (`*`, `?`, `[`, `\`) so `SCAN MATCH prefix*` uses trie prefix acceleration.
fn extract_glob_prefix(pattern: &[u8]) -> &[u8] {
    for (idx, &b) in pattern.iter().enumerate() {
        if matches!(b, b'*' | b'?' | b'[' | b'\\') {
            return &pattern[..idx];
        }
    }
    pattern
}

/// Simple Redis-compatible `*` / `?` glob matcher.
fn glob_match(pattern: &[u8], text: &[u8]) -> bool {
    let (mut p, mut t) = (0usize, 0usize);
    let (mut star_p, mut star_t) = (None, 0usize);
    while t < text.len() {
        if p < pattern.len() && (pattern[p] == b'?' || pattern[p] == text[t]) {
            p += 1;
            t += 1;
        } else if p < pattern.len() && pattern[p] == b'*' {
            star_p = Some(p);
            star_t = t;
            p += 1;
        } else if let Some(sp) = star_p {
            p = sp + 1;
            star_t += 1;
            t = star_t;
        } else {
            return false;
        }
    }
    while p < pattern.len() && pattern[p] == b'*' {
        p += 1;
    }
    p == pattern.len()
}
