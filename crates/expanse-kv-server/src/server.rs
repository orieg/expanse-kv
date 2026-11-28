//! Async `tokio` TCP listener, pipelined connection handler, background active
//! TTL expiry loop, and optional HTTP `/metrics` endpoint (`docs/PRD.md` §7 Phase 3).

use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::Duration;

use expanse_kv_core::resp::{CommandStatus, ParseLimits, encode_simple_error, parse_command_into};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::watch;

use crate::config::ServerConfig;
use crate::router::{ConnState, ServerState};

/// Bound and initialized `expanse-kv` server ready to accept connections.
pub struct RunningServer {
    listener: TcpListener,
    local_addr: SocketAddr,
    metrics_listener: Option<TcpListener>,
    metrics_addr: Option<SocketAddr>,
    state: Arc<ServerState>,
    active_expire_interval_ms: u64,
}

impl RunningServer {
    /// Binds TCP listener(s) and initializes storage/AOF state from `config`.
    ///
    /// # Errors
    /// Returns an error string if AOF replay or TCP binding fails.
    pub async fn bind(config: &ServerConfig) -> Result<Self, String> {
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

        // Spawn main RESP TCP accept loop.
        let listener = self.listener;
        let accept_state = Arc::clone(&self.state);
        let accept_handle = tokio::spawn(async move {
            loop {
                match listener.accept().await {
                    Ok((stream, _peer)) => {
                        if stream.set_nodelay(true).is_err() {
                            continue;
                        }
                        let conn_id = accept_state
                            .metrics
                            .connections_total
                            .fetch_add(1, Ordering::Relaxed)
                            + 1;
                        accept_state
                            .metrics
                            .connections_active
                            .fetch_add(1, Ordering::Relaxed);
                        let st = Arc::clone(&accept_state);
                        tokio::spawn(async move {
                            handle_resp_conn(stream, st, conn_id).await;
                        });
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
        expire_handle.abort();
        if let Some(mh) = metrics_handle {
            mh.abort();
        }
        self.state.flush_aof()?;
        Ok(())
    }
}

async fn handle_resp_conn(mut stream: TcpStream, state: Arc<ServerState>, conn_id: u64) {
    let limits = ParseLimits::default();
    let mut conn = ConnState::new(state.requirepass.is_none());
    conn.id = conn_id;

    let mut read_buf = vec![0u8; 64 * 1024];
    let mut accum: Vec<u8> = Vec::with_capacity(64 * 1024);
    let mut write_buf: Vec<u8> = Vec::with_capacity(64 * 1024);

    loop {
        let n = match stream.read(&mut read_buf).await {
            Ok(0) | Err(_) => break,
            Ok(n) => n,
        };
        state
            .metrics
            .read_bytes_total
            .fetch_add(n as u64, Ordering::Relaxed);
        accum.extend_from_slice(&read_buf[..n]);

        write_buf.clear();
        let mut offset = 0usize;
        let mut protocol_error = false;
        {
            let mut args: Vec<&[u8]> = Vec::with_capacity(16);
            while offset < accum.len() {
                match parse_command_into(&accum[offset..], &mut args, limits) {
                    Ok(CommandStatus::Complete { consumed }) => {
                        state.handle_command(&args, &mut conn, &mut write_buf);
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

        if offset > 0 {
            accum.drain(..offset);
        }

        if !write_buf.is_empty() {
            if stream.write_all(&write_buf).await.is_err() {
                break;
            }
            state
                .metrics
                .write_bytes_total
                .fetch_add(write_buf.len() as u64, Ordering::Relaxed);
        }

        if protocol_error || conn.should_close {
            break;
        }
    }

    state
        .metrics
        .connections_active
        .fetch_sub(1, Ordering::Relaxed);
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
