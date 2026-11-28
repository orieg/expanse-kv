//! End-to-end TCP integration tests for `expanse-kv` (`docs/PRD.md` §7 Phase 3).

use std::sync::atomic::{AtomicU64, Ordering};

use expanse_kv::config::ServerConfig;
use expanse_kv::server::RunningServer;
use expanse_kv_core::resp::{
    ParseStatus, RespFrame, encode_array_header, encode_bulk_string, parse_frame,
};
use expanse_kv_persist::aof::FsyncPolicy;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio::sync::watch;

fn encode_cmd(args: &[&[u8]]) -> Vec<u8> {
    let mut out = Vec::new();
    encode_array_header(args.len(), &mut out);
    for a in args {
        encode_bulk_string(a, &mut out);
    }
    out
}

async fn send_and_recv(stream: &mut TcpStream, cmd: &[&[u8]]) -> Vec<u8> {
    let wire = encode_cmd(cmd);
    stream.write_all(&wire).await.unwrap();
    read_one_frame_raw(stream).await
}

async fn read_one_frame_raw(stream: &mut TcpStream) -> Vec<u8> {
    let mut buf = Vec::new();
    let mut chunk = [0u8; 4096];
    loop {
        let n = stream.read(&mut chunk).await.unwrap();
        assert!(n > 0, "unexpected EOF while reading RESP frame");
        buf.extend_from_slice(&chunk[..n]);
        if let Ok(ParseStatus::Complete { consumed, .. }) = parse_frame(&buf) {
            return buf[..consumed].to_vec();
        }
    }
}

fn build_rt() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .unwrap()
}

#[test]
fn tcp_pipelining_strings_hashes_zsets_bitmaps_and_scan() {
    let rt = build_rt();
    rt.block_on(async {
        let cfg = ServerConfig {
            port: 0,
            metrics_port: Some(0),
            ..ServerConfig::default()
        };
        let server = RunningServer::bind(&cfg).await.unwrap();
        let addr = server.local_addr();
        let metrics_addr = server.metrics_addr().unwrap();
        let (tx, rx) = watch::channel(false);
        let handle = tokio::spawn(async move { server.serve(rx).await });

        let mut client = TcpStream::connect(addr).await.unwrap();

        // Pipeline 6 commands in a single TCP write
        let mut pipe = Vec::new();
        pipe.extend_from_slice(&encode_cmd(&[b"PING"]));
        pipe.extend_from_slice(&encode_cmd(&[b"SET", b"org:1:a", b"hello"]));
        pipe.extend_from_slice(&encode_cmd(&[b"GET", b"org:1:a"]));
        pipe.extend_from_slice(&encode_cmd(&[b"INCRBY", b"counter", b"41"]));
        pipe.extend_from_slice(&encode_cmd(&[b"INCR", b"counter"]));
        pipe.extend_from_slice(&encode_cmd(&[
            b"HSET", b"user:1", b"name", b"ada", b"role", b"eng",
        ]));
        client.write_all(&pipe).await.unwrap();

        let mut resp_buf = Vec::new();
        let mut chunk = [0u8; 4096];
        let mut frames_count = 0usize;
        while frames_count < 6 {
            let n = client.read(&mut chunk).await.unwrap();
            assert!(n > 0);
            resp_buf.extend_from_slice(&chunk[..n]);
            let mut off = 0;
            let mut count = 0;
            while let Ok(ParseStatus::Complete { consumed, .. }) = parse_frame(&resp_buf[off..]) {
                off += consumed;
                count += 1;
            }
            frames_count = count;
        }

        let mut off = 0;
        let expected = [
            RespFrame::SimpleString(b"PONG"),
            RespFrame::SimpleString(b"OK"),
            RespFrame::BulkString(b"hello"),
            RespFrame::Integer(41),
            RespFrame::Integer(42),
            RespFrame::Integer(2),
        ];
        for exp in &expected {
            let ParseStatus::Complete { frame, consumed } = parse_frame(&resp_buf[off..]).unwrap()
            else {
                panic!("incomplete frame");
            };
            assert_eq!(&frame, exp);
            off += consumed;
        }

        // ZSet + ZRANGE BYSCORE REV WITHSCORES
        let r = send_and_recv(
            &mut client,
            &[
                b"ZADD", b"scores", b"10.5", b"alice", b"25.0", b"bob", b"5.0", b"carol",
            ],
        )
        .await;
        assert_eq!(r, b":3\r\n");

        let r = send_and_recv(
            &mut client,
            &[
                b"ZRANGE",
                b"scores",
                b"25",
                b"10",
                b"BYSCORE",
                b"REV",
                b"WITHSCORES",
            ],
        )
        .await;
        let ParseStatus::Complete { frame, .. } = parse_frame(&r).unwrap() else {
            panic!("incomplete zrange");
        };
        assert_eq!(
            frame,
            RespFrame::Array(vec![
                RespFrame::BulkString(b"bob"),
                RespFrame::BulkString(b"25"),
                RespFrame::BulkString(b"alice"),
                RespFrame::BulkString(b"10.5"),
            ])
        );

        // Bitmap
        assert_eq!(
            send_and_recv(&mut client, &[b"SETBIT", b"bmp", b"100000", b"1"]).await,
            b":0\r\n"
        );
        assert_eq!(
            send_and_recv(&mut client, &[b"GETBIT", b"bmp", b"100000"]).await,
            b":1\r\n"
        );
        assert_eq!(
            send_and_recv(&mut client, &[b"BITCOUNT", b"bmp"]).await,
            b":1\r\n"
        );

        // SCAN MATCH org:1:*
        assert_eq!(
            send_and_recv(&mut client, &[b"SET", b"org:1:b", b"world"]).await,
            b"+OK\r\n"
        );
        let scan_raw = send_and_recv(
            &mut client,
            &[b"SCAN", b"0", b"MATCH", b"org:1:*", b"COUNT", b"10"],
        )
        .await;
        let ParseStatus::Complete {
            frame: scan_frame, ..
        } = parse_frame(&scan_raw).unwrap()
        else {
            panic!("incomplete scan");
        };
        assert_eq!(
            scan_frame,
            RespFrame::Array(vec![
                RespFrame::BulkString(b"0"),
                RespFrame::Array(vec![
                    RespFrame::BulkString(b"org:1:a"),
                    RespFrame::BulkString(b"org:1:b"),
                ]),
            ])
        );

        // Prometheus /metrics HTTP check
        let mut m_stream = TcpStream::connect(metrics_addr).await.unwrap();
        m_stream
            .write_all(b"GET /metrics HTTP/1.1\r\nHost: localhost\r\n\r\n")
            .await
            .unwrap();
        let mut m_resp = String::new();
        m_stream.read_to_string(&mut m_resp).await.unwrap();
        assert!(m_resp.starts_with("HTTP/1.1 200 OK"));
        assert!(m_resp.contains("expanse_kv_commands_total"));
        assert!(m_resp.contains("expanse_kv_keys "));

        tx.send(true).unwrap();
        handle.await.unwrap().unwrap();
    });
}

#[test]
fn tcp_resp3_multi_exec_and_auth() {
    let rt = build_rt();
    rt.block_on(async {
        let cfg = ServerConfig {
            port: 0,
            requirepass: Some("s3cr3t".to_string()),
            ..ServerConfig::default()
        };
        let server = RunningServer::bind(&cfg).await.unwrap();
        let addr = server.local_addr();
        let (tx, rx) = watch::channel(false);
        let handle = tokio::spawn(async move { server.serve(rx).await });

        let mut client = TcpStream::connect(addr).await.unwrap();

        // Unauthenticated GET must fail with NOAUTH
        let r = send_and_recv(&mut client, &[b"GET", b"k"]).await;
        assert!(r.starts_with(b"-NOAUTH"));

        // Wrong password fails
        let r = send_and_recv(&mut client, &[b"AUTH", b"wrong"]).await;
        assert!(r.starts_with(b"-WRONGPASS"));

        // HELLO 3 AUTH default s3cr3t authenticates and switches to RESP3
        let r = send_and_recv(
            &mut client,
            &[b"HELLO", b"3", b"AUTH", b"default", b"s3cr3t"],
        )
        .await;
        assert!(r.starts_with(b"%4\r\n"));

        // RESP3 null is `_\r\n`
        assert_eq!(
            send_and_recv(&mut client, &[b"GET", b"missing"]).await,
            b"_\r\n"
        );

        // MULTI / EXEC
        assert_eq!(send_and_recv(&mut client, &[b"MULTI"]).await, b"+OK\r\n");
        assert_eq!(
            send_and_recv(&mut client, &[b"SET", b"tx:k", b"10"]).await,
            b"+QUEUED\r\n"
        );
        assert_eq!(
            send_and_recv(&mut client, &[b"INCR", b"tx:k"]).await,
            b"+QUEUED\r\n"
        );
        assert_eq!(
            send_and_recv(&mut client, &[b"GET", b"tx:k"]).await,
            b"+QUEUED\r\n"
        );
        let exec_raw = send_and_recv(&mut client, &[b"EXEC"]).await;
        let ParseStatus::Complete { frame, .. } = parse_frame(&exec_raw).unwrap() else {
            panic!("incomplete exec");
        };
        assert_eq!(
            frame,
            RespFrame::Array(vec![
                RespFrame::SimpleString(b"OK"),
                RespFrame::Integer(11),
                RespFrame::BulkString(b"11"),
            ])
        );

        // DISCARD clears queued ops
        assert_eq!(send_and_recv(&mut client, &[b"MULTI"]).await, b"+OK\r\n");
        assert_eq!(
            send_and_recv(&mut client, &[b"SET", b"tx:k", b"999"]).await,
            b"+QUEUED\r\n"
        );
        assert_eq!(send_and_recv(&mut client, &[b"DISCARD"]).await, b"+OK\r\n");
        assert_eq!(
            send_and_recv(&mut client, &[b"GET", b"tx:k"]).await,
            b"$2\r\n11\r\n"
        );

        tx.send(true).unwrap();
        handle.await.unwrap().unwrap();
    });
}

#[test]
fn tcp_aof_restart_recovery() {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let id = COUNTER.fetch_add(1, Ordering::Relaxed);
    let aof_path = std::env::temp_dir().join(format!(
        "expanse-kv-server-aof-{}-{id}.aof",
        std::process::id()
    ));

    let rt = build_rt();
    rt.block_on(async {
        // First server lifetime: write data with fsync=always, then shut down.
        {
            let cfg = ServerConfig {
                port: 0,
                aof_path: Some(aof_path.clone()),
                fsync_policy: FsyncPolicy::Always,
                ..ServerConfig::default()
            };
            let server = RunningServer::bind(&cfg).await.unwrap();
            let addr = server.local_addr();
            let (tx, rx) = watch::channel(false);
            let handle = tokio::spawn(async move { server.serve(rx).await });

            let mut c = TcpStream::connect(addr).await.unwrap();
            assert_eq!(
                send_and_recv(&mut c, &[b"SET", b"persist:k", b"v1"]).await,
                b"+OK\r\n"
            );
            assert_eq!(
                send_and_recv(&mut c, &[b"HSET", b"persist:h", b"f", b"hv"]).await,
                b":1\r\n"
            );
            assert_eq!(
                send_and_recv(&mut c, &[b"ZADD", b"persist:z", b"42.5", b"m"]).await,
                b":1\r\n"
            );
            assert_eq!(
                send_and_recv(&mut c, &[b"SETBIT", b"persist:b", b"77", b"1"]).await,
                b":0\r\n"
            );

            tx.send(true).unwrap();
            handle.await.unwrap().unwrap();
        }

        // Second server lifetime: replay AOF and verify every key is restored.
        {
            let cfg = ServerConfig {
                port: 0,
                aof_path: Some(aof_path.clone()),
                fsync_policy: FsyncPolicy::Always,
                ..ServerConfig::default()
            };
            let server = RunningServer::bind(&cfg).await.unwrap();
            let addr = server.local_addr();
            let (tx, rx) = watch::channel(false);
            let handle = tokio::spawn(async move { server.serve(rx).await });

            let mut c = TcpStream::connect(addr).await.unwrap();
            assert_eq!(
                send_and_recv(&mut c, &[b"GET", b"persist:k"]).await,
                b"$2\r\nv1\r\n"
            );
            assert_eq!(
                send_and_recv(&mut c, &[b"HGET", b"persist:h", b"f"]).await,
                b"$2\r\nhv\r\n"
            );
            assert_eq!(
                send_and_recv(&mut c, &[b"ZSCORE", b"persist:z", b"m"]).await,
                b"$4\r\n42.5\r\n"
            );
            assert_eq!(
                send_and_recv(&mut c, &[b"GETBIT", b"persist:b", b"77"]).await,
                b":1\r\n"
            );

            tx.send(true).unwrap();
            handle.await.unwrap().unwrap();
        }
    });

    std::fs::remove_file(&aof_path).unwrap();
}
