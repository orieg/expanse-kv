//! Server configuration and CLI argument parsing for `expanse-kv`.

use std::fmt;
use std::path::PathBuf;

use expanse_kv_engine::{EngineConfig, EvictionPolicy};
use expanse_kv_persist::aof::FsyncPolicy;

/// Runtime configuration for the `expanse-kv` TCP server.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ServerConfig {
    /// Interface address to bind (default `"127.0.0.1"`).
    pub bind: String,
    /// TCP port to listen on (default `6379`; `0` lets the OS pick an ephemeral port).
    pub port: u16,
    /// Optional TCP port for HTTP Prometheus `/metrics` endpoint.
    pub metrics_port: Option<u16>,
    /// Underlying storage engine configuration.
    pub engine: EngineConfig,
    /// Optional password required via `AUTH` / `HELLO ... AUTH`.
    pub requirepass: Option<String>,
    /// Optional path to an Append-Only File for write logging and startup replay.
    pub aof_path: Option<PathBuf>,
    /// Fsync policy for the AOF writer.
    pub fsync_policy: FsyncPolicy,
    /// Optional path for binary CRC64 snapshots (`SAVE` / `BGSAVE`).
    pub snapshot_path: Option<PathBuf>,
    /// Optional PEM certificate chain path to enable TLS 1.3/1.2 via `rustls`.
    pub tls_cert: Option<PathBuf>,
    /// Optional PEM private key path paired with `tls_cert`.
    pub tls_key: Option<PathBuf>,
    /// Interval in milliseconds between background active TTL expiry sweeps.
    pub active_expire_interval_ms: u64,
}

impl Default for ServerConfig {
    fn default() -> Self {
        Self {
            bind: "127.0.0.1".to_string(),
            port: 6379,
            metrics_port: None,
            engine: EngineConfig::default(),
            requirepass: None,
            aof_path: None,
            fsync_policy: FsyncPolicy::EverySec,
            snapshot_path: None,
            tls_cert: None,
            tls_key: None,
            active_expire_interval_ms: 100,
        }
    }
}

/// Parsed top-level CLI action.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CliAction {
    /// Start the TCP server with the parsed configuration.
    Serve(ServerConfig),
    /// Print CLI help text and exit 0.
    Help,
    /// Print version information and exit 0.
    Version,
}

/// CLI argument parsing error.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CliError(pub String);

impl fmt::Display for CliError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for CliError {}

/// Usage banner printed by `--help` or invalid invocations.
pub const USAGE: &str = "\
expanse-kv — memory-safe, multi-threaded RESP2/RESP3 key-value store on expanse-trie

USAGE:
  expanse-kv serve [OPTIONS]
  expanse-kv --help | --version

OPTIONS:
  --bind <ADDR>             Bind address (default: 127.0.0.1)
  --port <PORT>             TCP listen port (default: 6379)
  --metrics-port <PORT>     Optional HTTP /metrics listen port
  --maxmemory <BYTES>       Memory cap in bytes (0 = unbounded)
  --eviction <POLICY>       noeviction | volatile-ttl | allkeys-lru
  --requirepass <PASS>      Require AUTH password on client connections
  --aof <PATH>              Enable Append-Only File persistence at PATH
  --fsync <POLICY>          always | everysec | no (default: everysec)
  --snapshot <PATH>         Binary CRC64 snapshot path for SAVE/BGSAVE
  --tls-cert <PATH>         PEM certificate chain path to enable TLS
  --tls-key <PATH>          PEM private key path (required with --tls-cert)
";

/// Parses command-line arguments (excluding `argv[0]`).
///
/// # Errors
/// Returns [`CliError`] when the subcommand is missing or an option is invalid.
pub fn parse_cli_args(args: &[String]) -> Result<CliAction, CliError> {
    let Some(first) = args.first() else {
        return Err(CliError(format!(
            "missing subcommand (use `expanse-kv serve`)\n\n{USAGE}"
        )));
    };

    match first.as_str() {
        "-h" | "--help" | "help" => return Ok(CliAction::Help),
        "-V" | "--version" | "version" => return Ok(CliAction::Version),
        "serve" => {}
        other => {
            return Err(CliError(format!("unknown subcommand `{other}`\n\n{USAGE}")));
        }
    }

    let mut cfg = ServerConfig::default();
    let mut i = 1usize;
    while i < args.len() {
        let flag = args[i].as_str();
        match flag {
            "-h" | "--help" => return Ok(CliAction::Help),
            "--bind" => {
                i += 1;
                cfg.bind = next_value(args, i, "--bind")?;
            }
            "--port" => {
                i += 1;
                let v = next_value(args, i, "--port")?;
                cfg.port = v
                    .parse()
                    .map_err(|_| CliError(format!("invalid --port value: `{v}`")))?;
            }
            "--metrics-port" => {
                i += 1;
                let v = next_value(args, i, "--metrics-port")?;
                let p: u16 = v
                    .parse()
                    .map_err(|_| CliError(format!("invalid --metrics-port value: `{v}`")))?;
                cfg.metrics_port = Some(p);
            }
            "--maxmemory" => {
                i += 1;
                let v = next_value(args, i, "--maxmemory")?;
                cfg.engine.max_memory = v
                    .parse()
                    .map_err(|_| CliError(format!("invalid --maxmemory value: `{v}`")))?;
            }
            "--eviction" => {
                i += 1;
                let v = next_value(args, i, "--eviction")?;
                cfg.engine.eviction_policy = match v.as_str() {
                    "noeviction" => EvictionPolicy::NoEviction,
                    "volatile-ttl" => EvictionPolicy::VolatileTtl,
                    "allkeys-lru" => EvictionPolicy::AllKeysLru,
                    _ => return Err(CliError(format!("invalid --eviction policy: `{v}`"))),
                };
            }
            "--requirepass" => {
                i += 1;
                cfg.requirepass = Some(next_value(args, i, "--requirepass")?);
            }
            "--aof" => {
                i += 1;
                cfg.aof_path = Some(PathBuf::from(next_value(args, i, "--aof")?));
            }
            "--fsync" => {
                i += 1;
                let v = next_value(args, i, "--fsync")?;
                cfg.fsync_policy = match v.as_str() {
                    "always" => FsyncPolicy::Always,
                    "everysec" => FsyncPolicy::EverySec,
                    "no" => FsyncPolicy::No,
                    _ => return Err(CliError(format!("invalid --fsync policy: `{v}`"))),
                };
            }
            "--snapshot" => {
                i += 1;
                cfg.snapshot_path = Some(PathBuf::from(next_value(args, i, "--snapshot")?));
            }
            "--tls-cert" => {
                i += 1;
                cfg.tls_cert = Some(PathBuf::from(next_value(args, i, "--tls-cert")?));
            }
            "--tls-key" => {
                i += 1;
                cfg.tls_key = Some(PathBuf::from(next_value(args, i, "--tls-key")?));
            }
            other => {
                return Err(CliError(format!("unknown option `{other}`\n\n{USAGE}")));
            }
        }
        i += 1;
    }

    if cfg.tls_cert.is_some() != cfg.tls_key.is_some() {
        return Err(CliError(
            "both --tls-cert and --tls-key must be provided together".to_string(),
        ));
    }

    Ok(CliAction::Serve(cfg))
}

fn next_value(args: &[String], idx: usize, flag: &str) -> Result<String, CliError> {
    args.get(idx)
        .cloned()
        .ok_or_else(|| CliError(format!("missing value for `{flag}`")))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bare_invocation_is_error() {
        assert!(parse_cli_args(&[]).is_err());
    }

    #[test]
    fn serve_parses_options() {
        let args: Vec<String> = [
            "serve",
            "--bind",
            "0.0.0.0",
            "--port",
            "6380",
            "--metrics-port",
            "9121",
            "--maxmemory",
            "1048576",
            "--eviction",
            "volatile-ttl",
            "--requirepass",
            "secret",
            "--fsync",
            "always",
        ]
        .iter()
        .map(|s| (*s).to_string())
        .collect();

        let CliAction::Serve(cfg) = parse_cli_args(&args).unwrap() else {
            panic!("expected Serve action");
        };
        assert_eq!(cfg.bind, "0.0.0.0");
        assert_eq!(cfg.port, 6380);
        assert_eq!(cfg.metrics_port, Some(9121));
        assert_eq!(cfg.engine.max_memory, 1_048_576);
        assert_eq!(cfg.engine.eviction_policy, EvictionPolicy::VolatileTtl);
        assert_eq!(cfg.requirepass.as_deref(), Some("secret"));
        assert_eq!(cfg.fsync_policy, FsyncPolicy::Always);
    }
}
