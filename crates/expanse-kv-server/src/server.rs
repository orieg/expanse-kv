//! Async `tokio` TCP listener, pipelined connection handler, background active
//! TTL expiry loop, and optional HTTP `/metrics` endpoint (`docs/PRD.md` §7 Phase 3).

use std::net::SocketAddr;
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::Duration;

use expanse_kv_core::resp::{
    CommandStatus, ParseLimits, encode_bulk_string, encode_null, encode_null_bulk,
    encode_simple_error, encode_simple_string, parse_command_into,
};
use expanse_kv_engine::SetOptions;
use rustls::ServerConfig as RustlsServerConfig;
use rustls_pki_types::pem::PemObject;
use rustls_pki_types::{CertificateDer, PrivateKeyDer};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{mpsc, watch};
use tokio::task::LocalSet;
use tokio_rustls::TlsAcceptor;

use crate::config::ServerConfig;
use crate::router::{ConnState, ServerState};

/// Bound and initialized `expanse-kv` server ready to accept connections.
pub struct RunningServer {
    listener: TcpListener,
    local_addr: SocketAddr,
    tls_acceptor: Option<TlsAcceptor>,
    metrics_listener: Option<TcpListener>,
    metrics_addr: Option<SocketAddr>,
    state: Arc<ServerState>,
    active_expire_interval_ms: u64,
}

fn load_tls_acceptor(cert_path: &Path, key_path: &Path) -> Result<TlsAcceptor, String> {
    let certs: Vec<CertificateDer<'static>> = CertificateDer::pem_file_iter(cert_path)
        .map_err(|e| format!("failed to open TLS cert `{}`: {e}", cert_path.display()))?
        .collect::<Result<Vec<_>, _>>()
        .map_err(|e| format!("failed to parse TLS cert `{}`: {e}", cert_path.display()))?;
    if certs.is_empty() {
        return Err(format!(
            "no PEM certificates found in `{}`",
            cert_path.display()
        ));
    }
    let key = PrivateKeyDer::from_pem_file(key_path)
        .map_err(|e| format!("failed to load TLS key `{}`: {e}", key_path.display()))?;
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let tls_cfg = RustlsServerConfig::builder_with_provider(provider)
        .with_safe_default_protocol_versions()
        .map_err(|e| format!("TLS protocol config error: {e}"))?
        .with_no_client_auth()
        .with_single_cert(certs, key)
        .map_err(|e| format!("invalid TLS cert/key pair: {e}"))?;
    Ok(TlsAcceptor::from(Arc::new(tls_cfg)))
}

impl RunningServer {
    /// Binds TCP listener(s) and initializes storage/AOF state from `config`.
    ///
    /// # Errors
    /// Returns an error string if AOF replay or TCP binding fails.
    pub async fn bind(config: &ServerConfig) -> Result<Self, String> {
        let tls_acceptor = match (&config.tls_cert, &config.tls_key) {
            (Some(cert), Some(key)) => Some(load_tls_acceptor(cert, key)?),
            (None, None) => None,
            _ => {
                return Err("both tls_cert and tls_key must be configured together".to_string());
            }
        };
        let state = Arc::new(ServerState::from_config(config)?);
        let addr_str = format!("{}:{}", config.bind, config.port);
        let listener = TcpListener::bind(&addr_str)
            .await
            .map_err(|e| format!("failed to bind `{addr_str}`: {e}"))?;
        let local_addr = listener
            .local_addr()
            .map_err(|e| format!("failed to query local_addr: {e}"))?;

        let (metrics_listener, metrics_addr) = match config.metrics_port {
            Some(mp) => {
                let m_addr_str = format!("{}:{mp}", config.bind);
                let ml = TcpListener::bind(&m_addr_str)
                    .await
                    .map_err(|e| format!("failed to bind metrics `{m_addr_str}`: {e}"))?;
                let ma = ml
                    .local_addr()
                    .map_err(|e| format!("failed to query metrics local_addr: {e}"))?;
                (Some(ml), Some(ma))
            }
            None => (None, None),
        };

        Ok(Self {
            listener,
            local_addr,
            tls_acceptor,
            metrics_listener,
            metrics_addr,
            state,
            active_expire_interval_ms: config.active_expire_interval_ms,
        })
    }

    /// Returns the bound RESP TCP socket address.
    #[must_use]
    pub const fn local_addr(&self) -> SocketAddr {
        self.local_addr
    }

    /// Returns the bound HTTP `/metrics` socket address, if enabled.
    #[must_use]
    pub const fn metrics_addr(&self) -> Option<SocketAddr> {
        self.metrics_addr
    }

    /// Returns a shared handle to [`ServerState`].
    #[must_use]
    pub fn state(&self) -> Arc<ServerState> {
        Arc::clone(&self.state)
    }

    /// Runs the accept loop, active TTL expiry background task, and optional
    /// `/metrics` HTTP server until `shutdown_rx` signals `true`.
    ///
    /// On shutdown, flushes and fsyncs the AOF file if enabled.
    ///
    /// # Errors
    /// Returns an error string if flushing persistence on shutdown fails.
    pub async fn serve(self, mut shutdown_rx: watch::Receiver<bool>) -> Result<(), String> {
        // Spawn active TTL expiry background task.
        let expire_state = Arc::clone(&self.state);
        let interval_ms = self.active_expire_interval_ms.max(10);
        let expire_handle = tokio::spawn(async move {
            let mut ticker = tokio::time::interval(Duration::from_millis(interval_ms));
            loop {
                ticker.tick().await;
                let _evicted = expire_state.engine.active_expire_cycle(20);
            }
        });

        // Spawn optional HTTP /metrics listener.
        let metrics_handle = self.metrics_listener.map(|ml| {
            let m_state = Arc::clone(&self.state);
            tokio::spawn(async move {
                while let Ok((stream, _)) = ml.accept().await {
                    let st = Arc::clone(&m_state);
                    tokio::spawn(async move {
                        handle_metrics_conn(stream, st).await;
                    });
                }
            })
        });

        // Spawn thread-per-core single-threaded tokio LocalSet I/O workers so
        // connections stay pinned to one worker thread with zero work-stealing
        // task migration while sharing the single unified `ServerState`.
        let num_workers = std::thread::available_parallelism()
            .map(std::num::NonZeroUsize::get)
            .unwrap_or(4)
            .clamp(1, 32);
        let mut worker_txs: Vec<mpsc::UnboundedSender<(std::net::TcpStream, u64)>> =
            Vec::with_capacity(num_workers);
        let mut worker_threads = Vec::with_capacity(num_workers);

        for _ in 0..num_workers {
            let (tx, mut rx) = mpsc::unbounded_channel::<(std::net::TcpStream, u64)>();
            worker_txs.push(tx);
            let w_state = Arc::clone(&self.state);
            let w_tls = self.tls_acceptor.clone();
            let handle = std::thread::spawn(move || {
                let rt = match tokio::runtime::Builder::new_current_thread()
                    .enable_io()
                    .build()
                {
                    Ok(rt) => rt,
                    Err(_) => return,
                };
                let local = LocalSet::new();
                rt.block_on(local.run_until(async move {
                    while let Some((std_stream, conn_id)) = rx.recv().await {
                        let Ok(stream) = TcpStream::from_std(std_stream) else {
                            w_state
                                .metrics
                                .connections_active
                                .fetch_sub(1, Ordering::Relaxed);
                            continue;
                        };
                        let st = Arc::clone(&w_state);
                        let acceptor = w_tls.clone();
                        tokio::task::spawn_local(async move {
                            match acceptor {
                                Some(tls) => match tls.accept(stream).await {
                                    Ok(tls_stream) => {
                                        handle_resp_conn(tls_stream, st, conn_id).await;
                                    }
                                    Err(_) => {
                                        st.metrics
                                            .connections_active
                                            .fetch_sub(1, Ordering::Relaxed);
                                    }
                                },
                                None => handle_resp_conn(stream, st, conn_id).await,
                            }
                        });
                    }
                }));
            });
            worker_threads.push(handle);
        }

        // Spawn main RESP TCP accept loop dispatching round-robin to workers.
        let listener = self.listener;
        let accept_state = Arc::clone(&self.state);
        let accept_handle = tokio::spawn(async move {
            loop {
                match listener.accept().await {
                    Ok((stream, _peer)) => {
                        if stream.set_nodelay(true).is_err() {
                            continue;
                        }
                        let Ok(std_stream) = stream.into_std() else {
                            continue;
                        };
                        let conn_id = accept_state
                            .metrics
                            .connections_total
                            .fetch_add(1, Ordering::Relaxed)
                            + 1;
                        accept_state
                            .metrics
                            .connections_active
                            .fetch_add(1, Ordering::Relaxed);
                        let idx = (conn_id as usize) % worker_txs.len();
                        if worker_txs[idx].send((std_stream, conn_id)).is_err() {
                            accept_state
                                .metrics
                                .connections_active
                                .fetch_sub(1, Ordering::Relaxed);
                        }
                    }
                    Err(_) => {
                        tokio::time::sleep(Duration::from_millis(5)).await;
                    }
                }
            }
        });

        // Wait for shutdown signal.
        while !*shutdown_rx.borrow() {
            if shutdown_rx.changed().await.is_err() {
                break;
            }
        }

        accept_handle.abort();
        drop(accept_handle.await);
        expire_handle.abort();
        if let Some(mh) = metrics_handle {
            mh.abort();
        }
        for h in worker_threads {
            drop(h.join());
        }
        self.state.flush_aof()?;
        Ok(())
    }
}

async fn handle_resp_conn<S>(mut stream: S, state: Arc<ServerState>, conn_id: u64)
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    let limits = ParseLimits::default();
    let mut conn = ConnState::new(state.requirepass.is_none());
    conn.id = conn_id;

    let mut accum: Vec<u8> = Vec::with_capacity(64 * 1024);
    let mut write_buf: Vec<u8> = Vec::with_capacity(64 * 1024);
    // Single reusable optimistic reader per connection: registers once on
    // connect and drops on disconnect, holding zero epoch pins when idle
    // (AGENTS.md §2.5).
    let mut reader = state.engine.reader();
    let mut local_cmds = 0u64;
    let mut local_read_bytes = 0u64;
    let mut local_write_bytes = 0u64;

    loop {
        if accum.capacity() - accum.len() < 4096 {
            accum.reserve(64 * 1024);
        }
        let n = match stream.read_buf(&mut accum).await {
            Ok(0) | Err(_) => break,
            Ok(n) => n,
        };
        local_read_bytes += n as u64;

        write_buf.clear();
        let mut offset = 0usize;
        let mut protocol_error = false;
        {
            // Lazily allocated: stays zero-alloc on the fast GET/SET paths.
            let mut args: Vec<&[u8]> = Vec::new();
            while offset < accum.len() {
                let slice = &accum[offset..];
                if conn.authenticated && !conn.in_multi {
                    if let Some((key, consumed)) = try_parse_fast_get(slice) {
                        match state.engine.get_with_reader(&mut reader, key, |v| {
                            encode_bulk_string(v, &mut write_buf);
                        }) {
                            Ok(Some(())) => {}
                            Ok(None) => {
                                if conn.resp3 {
                                    encode_null(&mut write_buf);
                                } else {
                                    encode_null_bulk(&mut write_buf);
                                }
                            }
                            Err(e) => {
                                state.metrics.errors_total.fetch_add(1, Ordering::Relaxed);
                                let msg = e.to_string();
                                encode_simple_error(msg.as_bytes(), &mut write_buf);
                            }
                        }
                        local_cmds += 1;
                        offset += consumed;
                        continue;
                    }
                    if state.aof.is_none()
                        && let Some((key, val, consumed)) = try_parse_fast_set(slice)
                    {
                        match state.engine.set_with_reader(
                            &mut reader,
                            key,
                            val,
                            SetOptions::default(),
                        ) {
                            Ok((true, _)) => encode_simple_string(b"OK", &mut write_buf),
                            Ok((false, _)) => {
                                if conn.resp3 {
                                    encode_null(&mut write_buf);
                                } else {
                                    encode_null_bulk(&mut write_buf);
                                }
                            }
                            Err(e) => {
                                state.metrics.errors_total.fetch_add(1, Ordering::Relaxed);
                                let msg = e.to_string();
                                encode_simple_error(msg.as_bytes(), &mut write_buf);
                            }
                        }
                        local_cmds += 1;
                        offset += consumed;
                        continue;
                    }
                }

                match parse_command_into(slice, &mut args, limits) {
                    Ok(CommandStatus::Complete { consumed }) => {
                        state.handle_command(&args, &mut conn, &mut reader, &mut write_buf);
                        local_cmds += 1;
                        offset += consumed;
                        if conn.should_close {
                            break;
                        }
                    }
                    Ok(CommandStatus::Incomplete) => break,
                    Err(e) => {
                        state.metrics.errors_total.fetch_add(1, Ordering::Relaxed);
                        let msg = format!("ERR Protocol error: {e}");
                        encode_simple_error(msg.as_bytes(), &mut write_buf);
                        protocol_error = true;
                        break;
                    }
                }
            }
        }

        if offset == accum.len() {
            accum.clear();
        } else if offset > 0 {
            accum.drain(..offset);
        }

        if !write_buf.is_empty() {
            if stream.write_all(&write_buf).await.is_err() {
                break;
            }
            local_write_bytes += write_buf.len() as u64;
        }

        // Flush per-connection counters periodically (every 1024 commands) to
        // keep global atomic contention near zero while keeping /metrics live.
        if local_cmds >= 1024 {
            state
                .metrics
                .commands_total
                .fetch_add(local_cmds, Ordering::Relaxed);
            state
                .metrics
                .read_bytes_total
                .fetch_add(local_read_bytes, Ordering::Relaxed);
            state
                .metrics
                .write_bytes_total
                .fetch_add(local_write_bytes, Ordering::Relaxed);
            local_cmds = 0;
            local_read_bytes = 0;
            local_write_bytes = 0;
        }

        if protocol_error || conn.should_close {
            break;
        }
    }

    if local_cmds > 0 {
        state
            .metrics
            .commands_total
            .fetch_add(local_cmds, Ordering::Relaxed);
    }
    if local_read_bytes > 0 {
        state
            .metrics
            .read_bytes_total
            .fetch_add(local_read_bytes, Ordering::Relaxed);
    }
    if local_write_bytes > 0 {
        state
            .metrics
            .write_bytes_total
            .fetch_add(local_write_bytes, Ordering::Relaxed);
    }

    state
        .metrics
        .connections_active
        .fetch_sub(1, Ordering::Relaxed);
}

/// Parses a `$<1..3 digits>\r\n<bytes>\r\n` bulk string at `input[pos..]`,
/// returning `(slice, next_pos)`.
#[inline]
fn parse_fast_bulk(input: &[u8], pos: usize) -> Option<(&[u8], usize)> {
    if pos + 4 > input.len() || input[pos] != b'$' {
        return None;
    }
    let b0 = input[pos + 1];
    if !b0.is_ascii_digit() {
        return None;
    }
    let d0 = usize::from(b0 - b'0');
    let (len, start) = if input[pos + 2] == b'\r' && input[pos + 3] == b'\n' {
        (d0, pos + 4)
    } else if pos + 5 <= input.len()
        && input[pos + 2].is_ascii_digit()
        && input[pos + 3] == b'\r'
        && input[pos + 4] == b'\n'
    {
        (d0 * 10 + usize::from(input[pos + 2] - b'0'), pos + 5)
    } else if pos + 6 <= input.len()
        && input[pos + 2].is_ascii_digit()
        && input[pos + 3].is_ascii_digit()
        && input[pos + 4] == b'\r'
        && input[pos + 5] == b'\n'
    {
        (
            d0 * 100 + usize::from(input[pos + 2] - b'0') * 10 + usize::from(input[pos + 3] - b'0'),
            pos + 6,
        )
    } else {
        return None;
    };
    let end = start + len;
    let next = end + 2;
    if next > input.len() || input[end] != b'\r' || input[end + 1] != b'\n' {
        return None;
    }
    Some((&input[start..end], next))
}

/// Zero-allocation fast-path matcher for `*2\r\n$3\r\nGET\r\n$<len>\r\n<key>\r\n`.
#[inline]
fn try_parse_fast_get(input: &[u8]) -> Option<(&[u8], usize)> {
    if input.len() < 18 || &input[..13] != b"*2\r\n$3\r\nGET\r\n" {
        return None;
    }
    parse_fast_bulk(input, 13)
}

/// Zero-allocation fast-path matcher for `*3\r\n$3\r\nSET\r\n$<klen>\r\n<key>\r\n$<vlen>\r\n<val>\r\n`.
#[inline]
fn try_parse_fast_set(input: &[u8]) -> Option<(&[u8], &[u8], usize)> {
    if input.len() < 22 || &input[..13] != b"*3\r\n$3\r\nSET\r\n" {
        return None;
    }
    let (key, next) = parse_fast_bulk(input, 13)?;
    let (val, total) = parse_fast_bulk(input, next)?;
    Some((key, val, total))
}

async fn handle_metrics_conn(mut stream: TcpStream, state: Arc<ServerState>) {
    let mut req = [0u8; 1024];
    if stream.read(&mut req).await.is_err() {
        return;
    }
    let body = state.metrics.render_prometheus(&state.engine);
    let response = format!(
        "HTTP/1.1 200 OK\r\nContent-Type: text/plain; version=0.0.4\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
        body.len(),
        body
    );
    let _write_res = stream.write_all(response.as_bytes()).await;
}
