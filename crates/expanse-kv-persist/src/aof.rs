//! Append-Only File (AOF) writer, crash-safe streaming replayer, and atomic
//! rewrite (`docs/PRD.md` §7 Phase 4).
//!
//! Commands are framed as standard RESP multibulk arrays using
//! [`encode_aof_command`]. On replay, a partial trailing
//! record (as produced by `SIGKILL` mid-write) is detected cleanly at the last
//! complete command boundary; malformed frames in the interior of the file
//! return [`AofError::Protocol`].

use std::fmt;
use std::fs::{File, OpenOptions};
use std::io::{self, BufReader, BufWriter, Read, Write};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use expanse_kv_core::resp::{
    CommandStatus, ParseLimits, RespError, encode_array_header, encode_bulk_string,
    parse_command_into,
};

/// Encodes a RESP multibulk command into `out`.
pub fn encode_aof_command(args: &[&[u8]], out: &mut Vec<u8>) {
    encode_array_header(args.len(), out);
    for arg in args {
        encode_bulk_string(arg, out);
    }
}

/// Fsync durability policy for [`AofWriter`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum FsyncPolicy {
    /// Call `sync_data()` after every appended command. Guarantees zero loss
    /// of acknowledged writes across process `SIGKILL`.
    Always,
    /// Call `sync_data()` at most once per second.
    #[default]
    EverySec,
    /// Flush to the OS page cache without calling `sync_data()`.
    No,
}

/// Errors returned by AOF write, replay, and rewrite operations.
#[derive(Debug)]
pub enum AofError {
    /// Underlying filesystem I/O error.
    Io(io::Error),
    /// Malformed RESP framing encountered inside the AOF stream.
    Protocol {
        /// Byte offset in the AOF file where the malformed frame started.
        offset: u64,
        /// Parser error detail.
        error: RespError,
    },
    /// Callback rejected a replayed command.
    Apply(String),
}

impl fmt::Display for AofError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io(e) => write!(f, "AOF I/O error: {e}"),
            Self::Protocol { offset, error } => {
                write!(f, "AOF protocol error at byte offset {offset}: {error}")
            }
            Self::Apply(msg) => write!(f, "AOF apply error: {msg}"),
        }
    }
}

impl std::error::Error for AofError {}

impl From<io::Error> for AofError {
    fn from(e: io::Error) -> Self {
        Self::Io(e)
    }
}

/// Statistics returned after replaying an AOF file.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AofReplayStats {
    /// Number of complete commands replayed.
    pub commands_replayed: u64,
    /// Total valid bytes consumed up to the last complete command boundary.
    pub bytes_consumed: u64,
    /// Trailing incomplete bytes ignored (and truncated if requested) at EOF.
    pub truncated_tail_bytes: u64,
}

/// Append-only command log writer with configurable [`FsyncPolicy`].
pub struct AofWriter {
    path: PathBuf,
    writer: BufWriter<File>,
    policy: FsyncPolicy,
    last_sync: Instant,
    scratch: Vec<u8>,
    commands_appended: u64,
    bytes_appended: u64,
}

impl AofWriter {
    /// Opens or creates an AOF file at `path` in append mode.
    ///
    /// # Errors
    /// Returns [`AofError::Io`] if the file cannot be opened or created.
    pub fn open(path: impl AsRef<Path>, policy: FsyncPolicy) -> Result<Self, AofError> {
        let path_buf = path.as_ref().to_path_buf();
        let file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path_buf)?;
        Ok(Self {
            path: path_buf,
            writer: BufWriter::with_capacity(64 * 1024, file),
            policy,
            last_sync: Instant::now(),
            scratch: Vec::with_capacity(256),
            commands_appended: 0,
            bytes_appended: 0,
        })
    }

    /// Appends a single RESP command (e.g. `&[b"SET", b"k", b"v"]`) to the log.
    ///
    /// Under [`FsyncPolicy::Always`], flushes the buffer and calls
    /// [`File::sync_data`] before returning.
    ///
    /// # Errors
    /// Returns [`AofError::Io`] on write, flush, or fsync failure.
    pub fn append(&mut self, args: &[&[u8]]) -> Result<(), AofError> {
        self.scratch.clear();
        encode_aof_command(args, &mut self.scratch);
        self.writer.write_all(&self.scratch)?;
        self.commands_appended += 1;
        self.bytes_appended += self.scratch.len() as u64;

        match self.policy {
            FsyncPolicy::Always => {
                self.writer.flush()?;
                self.writer.get_ref().sync_data()?;
                self.last_sync = Instant::now();
            }
            FsyncPolicy::EverySec => {
                if self.last_sync.elapsed() >= Duration::from_secs(1) {
                    self.writer.flush()?;
                    self.writer.get_ref().sync_data()?;
                    self.last_sync = Instant::now();
                }
            }
            FsyncPolicy::No => {}
        }
        Ok(())
    }

    /// Appends a batch of commands in one buffered write, applying the
    /// configured [`FsyncPolicy`] once at the end of the batch.
    ///
    /// # Errors
    /// Returns [`AofError::Io`] on write, flush, or fsync failure.
    pub fn append_batch<S: AsRef<[u8]>>(&mut self, commands: &[&[S]]) -> Result<(), AofError> {
        let mut refs: Vec<&[u8]> = Vec::new();
        for cmd in commands {
            refs.clear();
            for arg in *cmd {
                refs.push(arg.as_ref());
            }
            self.scratch.clear();
            encode_aof_command(&refs, &mut self.scratch);
            self.writer.write_all(&self.scratch)?;
            self.commands_appended += 1;
            self.bytes_appended += self.scratch.len() as u64;
        }
        match self.policy {
            FsyncPolicy::Always => {
                self.writer.flush()?;
                self.writer.get_ref().sync_data()?;
                self.last_sync = Instant::now();
            }
            FsyncPolicy::EverySec => {
                if self.last_sync.elapsed() >= Duration::from_secs(1) {
                    self.writer.flush()?;
                    self.writer.get_ref().sync_data()?;
                    self.last_sync = Instant::now();
                }
            }
            FsyncPolicy::No => {}
        }
        Ok(())
    }

    /// Flushes all buffered bytes and fsyncs the file to disk (graceful shutdown).
    ///
    /// # Errors
    /// Returns [`AofError::Io`] if flushing or syncing fails.
    pub fn flush_and_sync(&mut self) -> Result<(), AofError> {
        self.writer.flush()?;
        self.writer.get_ref().sync_all()?;
        self.last_sync = Instant::now();
        Ok(())
    }

    /// Returns the filesystem path of this AOF file.
    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Returns the total number of commands appended since this writer opened.
    #[must_use]
    pub const fn commands_appended(&self) -> u64 {
        self.commands_appended
    }

    /// Returns the total number of bytes appended since this writer opened.
    #[must_use]
    pub const fn bytes_appended(&self) -> u64 {
        self.bytes_appended
    }
}

/// Replays all complete RESP commands from `path`, invoking `on_command` for
/// each command in order.
///
/// If `truncate_incomplete_tail` is `true` and the file ends with an incomplete
/// trailing record (e.g. from `SIGKILL` mid-write), the file is truncated to
/// the exact byte offset of the last complete command so subsequent appends
/// remain valid.
///
/// # Errors
/// Returns [`AofError::Io`] on filesystem failure, [`AofError::Protocol`] if
/// malformed bytes appear inside the log, or [`AofError::Apply`] if
/// `on_command` returns an error.
pub fn replay_aof<F>(
    path: impl AsRef<Path>,
    truncate_incomplete_tail: bool,
    mut on_command: F,
) -> Result<AofReplayStats, AofError>
where
    F: FnMut(&[&[u8]]) -> Result<(), String>,
{
    let path_ref = path.as_ref();
    let file = File::open(path_ref)?;
    let mut reader = BufReader::with_capacity(256 * 1024, file);
    let limits = ParseLimits::default();

    let mut buf: Vec<u8> = Vec::with_capacity(256 * 1024);
    let mut read_chunk = vec![0u8; 128 * 1024];

    let mut commands_replayed: u64 = 0;
    let mut bytes_consumed: u64 = 0;
    let mut eof = false;

    loop {
        if !eof && buf.len() < 64 * 1024 {
            let n = reader.read(&mut read_chunk)?;
            if n == 0 {
                eof = true;
            } else {
                buf.extend_from_slice(&read_chunk[..n]);
            }
        }

        let mut offset = 0usize;
        let mut need_more = false;
        {
            let mut args: Vec<&[u8]> = Vec::with_capacity(16);
            while offset < buf.len() {
                match parse_command_into(&buf[offset..], &mut args, limits) {
                    Ok(CommandStatus::Complete { consumed }) => {
                        if !args.is_empty() {
                            on_command(&args).map_err(AofError::Apply)?;
                            commands_replayed += 1;
                        }
                        offset += consumed;
                        bytes_consumed += consumed as u64;
                    }
                    Ok(CommandStatus::Incomplete) => {
                        need_more = true;
                        break;
                    }
                    Err(error) => {
                        return Err(AofError::Protocol {
                            offset: bytes_consumed,
                            error,
                        });
                    }
                }
            }
        }

        if offset > 0 {
            buf.drain(..offset);
        }

        if need_more {
            if eof {
                break;
            }
            let n = reader.read(&mut read_chunk)?;
            if n == 0 {
                eof = true;
            } else {
                buf.extend_from_slice(&read_chunk[..n]);
            }
        } else if eof && buf.is_empty() {
            break;
        }
    }

    let truncated_tail_bytes = buf.len() as u64;
    drop(reader);

    if truncate_incomplete_tail && truncated_tail_bytes > 0 {
        let f = OpenOptions::new().write(true).open(path_ref)?;
        f.set_len(bytes_consumed)?;
        f.sync_all()?;
    }

    Ok(AofReplayStats {
        commands_replayed,
        bytes_consumed,
        truncated_tail_bytes,
    })
}

/// Atomically rewrites `target_path` by writing all compacted commands yielded
/// by `write_commands` to a sibling temporary file, fsyncing it, and renaming
/// it over `target_path`.
///
/// # Errors
/// Returns [`AofError::Io`] on write, sync, or rename failure.
pub fn rewrite_aof_atomic<F>(
    target_path: impl AsRef<Path>,
    mut write_commands: F,
) -> Result<u64, AofError>
where
    F: FnMut(&mut AofRewriteSink) -> Result<(), AofError>,
{
    let target = target_path.as_ref();
    let mut tmp_name = target
        .file_name()
        .map_or_else(|| "aof".to_string(), |s| s.to_string_lossy().into_owned());
    tmp_name.push_str(&format!(".rewrite.{}.tmp", std::process::id()));
    let tmp_path = target.with_file_name(tmp_name);

    let file = OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(true)
        .open(&tmp_path)?;
    let mut sink = AofRewriteSink {
        writer: BufWriter::with_capacity(256 * 1024, file),
        scratch: Vec::with_capacity(256),
        commands_written: 0,
    };

    if let Err(err) = write_commands(&mut sink) {
        drop(sink);
        if tmp_path.exists() {
            let remove_res = std::fs::remove_file(&tmp_path);
            if let Err(rm_err) = remove_res {
                return Err(AofError::Io(rm_err));
            }
        }
        return Err(err);
    }

    sink.writer.flush()?;
    sink.writer.get_ref().sync_all()?;
    let total = sink.commands_written;
    drop(sink);

    std::fs::rename(&tmp_path, target)?;
    Ok(total)
}

/// Sink passed to the closure in [`rewrite_aof_atomic`].
pub struct AofRewriteSink {
    writer: BufWriter<File>,
    scratch: Vec<u8>,
    commands_written: u64,
}

impl AofRewriteSink {
    /// Writes one compacted command into the temporary rewrite file.
    ///
    /// # Errors
    /// Returns [`AofError::Io`] on write failure.
    pub fn write_command(&mut self, args: &[&[u8]]) -> Result<(), AofError> {
        self.scratch.clear();
        encode_aof_command(args, &mut self.scratch);
        self.writer.write_all(&self.scratch)?;
        self.commands_written += 1;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};

    fn unique_temp_path(label: &str) -> PathBuf {
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let id = COUNTER.fetch_add(1, Ordering::Relaxed);
        std::env::temp_dir().join(format!(
            "expanse-kv-aof-{label}-{}-{id}.aof",
            std::process::id()
        ))
    }

    #[test]
    fn append_and_replay_roundtrip() {
        let path = unique_temp_path("roundtrip");
        {
            let mut w = AofWriter::open(&path, FsyncPolicy::Always).unwrap();
            w.append(&[b"SET", b"alpha", b"1"]).unwrap();
            w.append(&[b"HSET", b"h1", b"f1", b"v1"]).unwrap();
            w.append(&[b"ZADD", b"z1", b"3.5", b"m1"]).unwrap();
            w.flush_and_sync().unwrap();
            assert_eq!(w.commands_appended(), 3);
        }

        let mut seen: Vec<Vec<Vec<u8>>> = Vec::new();
        let stats = replay_aof(&path, false, |args| {
            seen.push(args.iter().map(|s| s.to_vec()).collect());
            Ok(())
        })
        .unwrap();

        assert_eq!(stats.commands_replayed, 3);
        assert_eq!(stats.truncated_tail_bytes, 0);
        assert_eq!(
            seen[0],
            vec![b"SET".to_vec(), b"alpha".to_vec(), b"1".to_vec()]
        );
        assert_eq!(
            seen[1],
            vec![
                b"HSET".to_vec(),
                b"h1".to_vec(),
                b"f1".to_vec(),
                b"v1".to_vec()
            ]
        );
        assert_eq!(
            seen[2],
            vec![
                b"ZADD".to_vec(),
                b"z1".to_vec(),
                b"3.5".to_vec(),
                b"m1".to_vec()
            ]
        );
        std::fs::remove_file(&path).unwrap();
    }

    #[test]
    fn sigkill_truncated_tail_recovers_all_committed_commands() {
        let path = unique_temp_path("sigkill");
        let committed_len;
        {
            let mut w = AofWriter::open(&path, FsyncPolicy::Always).unwrap();
            w.append(&[b"SET", b"k1", b"v1"]).unwrap();
            w.append(&[b"SET", b"k2", b"v2"]).unwrap();
            w.flush_and_sync().unwrap();
            committed_len = w.bytes_appended();
        }

        // Simulate a partial in-flight write interrupted by SIGKILL.
        let mut partial = Vec::new();
        encode_aof_command(
            &[
                b"SET".as_slice(),
                b"k3".as_slice(),
                b"value-not-finished".as_slice(),
            ],
            &mut partial,
        );
        for cut in 1..partial.len() {
            let cut_path = unique_temp_path(&format!("cut-{cut}"));
            let mut full = std::fs::read(&path).unwrap();
            full.extend_from_slice(&partial[..cut]);
            std::fs::write(&cut_path, &full).unwrap();

            let mut replayed = Vec::new();
            let stats = replay_aof(&cut_path, true, |args| {
                replayed.push(args[1].to_vec());
                Ok(())
            })
            .unwrap();

            assert_eq!(stats.commands_replayed, 2, "cut={cut}");
            assert_eq!(stats.bytes_consumed, committed_len, "cut={cut}");
            assert_eq!(stats.truncated_tail_bytes, cut as u64, "cut={cut}");
            assert_eq!(replayed, vec![b"k1".to_vec(), b"k2".to_vec()]);
            assert_eq!(
                std::fs::metadata(&cut_path).unwrap().len(),
                committed_len,
                "file should be truncated cleanly at cut={cut}"
            );
            std::fs::remove_file(&cut_path).unwrap();
        }

        std::fs::remove_file(&path).unwrap();
    }

    #[test]
    fn atomic_rewrite_replaces_aof() {
        let path = unique_temp_path("rewrite");
        {
            let mut w = AofWriter::open(&path, FsyncPolicy::No).unwrap();
            w.append(&[b"SET", b"k", b"old1"]).unwrap();
            w.append(&[b"SET", b"k", b"old2"]).unwrap();
            w.append(&[b"SET", b"k", b"final"]).unwrap();
            w.flush_and_sync().unwrap();
        }

        let written =
            rewrite_aof_atomic(&path, |sink| sink.write_command(&[b"SET", b"k", b"final"]))
                .unwrap();
        assert_eq!(written, 1);

        let mut seen = Vec::new();
        let stats = replay_aof(&path, false, |args| {
            seen.push(args[2].to_vec());
            Ok(())
        })
        .unwrap();
        assert_eq!(stats.commands_replayed, 1);
        assert_eq!(seen, vec![b"final".to_vec()]);
        std::fs::remove_file(&path).unwrap();
    }
}
