//! Versioned, CRC64-checksummed binary snapshot format (`docs/PRD.md` §7 Phase 4).
//!
//! Format layout (version 1, all integers big-endian):
//! - Header: magic `b"EKVS"` (4 bytes) + version `u16` (`1`) + flags `u16` (`0`)
//! - Records: each starts with a 1-byte tag:
//!   - `0x01`: String (`key`, `value`, optional `expiry_ms`)
//!   - `0x02`: Hash (`key`, `[(field, value)]`, optional `expiry_ms`)
//!   - `0x03`: ZSet (`key`, `[(score_f64_bits, member)]`, optional `expiry_ms`)
//!   - `0x04`: Bitmap (`key`, `[bit_offset_u64]`, optional `expiry_ms`)
//!   - `0xFF`: End-of-stream marker
//! - Trailer: 8-byte CRC64-Jones checksum covering header through EOF marker.

use std::fmt;
use std::fs::{File, OpenOptions};
use std::io::{self, BufReader, BufWriter, Read, Write};
use std::path::Path;

use crate::crc64::Crc64;

/// 4-byte file header magic (`expanse-kv` snapshot).
pub const SNAPSHOT_MAGIC: [u8; 4] = *b"EKVS";

/// Current binary snapshot format version.
pub const SNAPSHOT_VERSION: u16 = 1;

const TAG_STRING: u8 = 0x01;
const TAG_HASH: u8 = 0x02;
const TAG_ZSET: u8 = 0x03;
const TAG_BITMAP: u8 = 0x04;
const TAG_EOF: u8 = 0xFF;

/// Maximum byte length accepted for a single snapshot key or value (512 MiB).
const MAX_BLOB_LEN: usize = 512 * 1024 * 1024;

/// A logical keyspace record serialized in a binary snapshot.
#[derive(Debug, Clone, PartialEq)]
pub enum SnapshotRecord {
    /// String key-value entry with optional absolute expiry timestamp (ms).
    String {
        /// Key bytes.
        key: Vec<u8>,
        /// Value bytes.
        value: Vec<u8>,
        /// Optional absolute expiry time in Unix milliseconds.
        expiry_ms: Option<u64>,
    },
    /// Hash key with its field-value pairs and optional expiry timestamp (ms).
    Hash {
        /// Key bytes.
        key: Vec<u8>,
        /// Field-value pairs.
        fields: Vec<(Vec<u8>, Vec<u8>)>,
        /// Optional absolute expiry time in Unix milliseconds.
        expiry_ms: Option<u64>,
    },
    /// Sorted-set key with `(score, member)` entries and optional expiry timestamp (ms).
    ZSet {
        /// Key bytes.
        key: Vec<u8>,
        /// `(score, member)` entries.
        entries: Vec<(f64, Vec<u8>)>,
        /// Optional absolute expiry time in Unix milliseconds.
        expiry_ms: Option<u64>,
    },
    /// Sparse bitmap key with sorted set-bit offsets and optional expiry timestamp (ms).
    Bitmap {
        /// Key bytes.
        key: Vec<u8>,
        /// Set bit offsets.
        bits: Vec<u64>,
        /// Optional absolute expiry time in Unix milliseconds.
        expiry_ms: Option<u64>,
    },
}

/// Errors returned when saving or loading a binary snapshot.
#[derive(Debug)]
pub enum SnapshotError {
    /// Filesystem I/O error.
    Io(io::Error),
    /// Invalid magic header bytes.
    BadMagic([u8; 4]),
    /// Unsupported format version.
    UnsupportedVersion(u16),
    /// Unknown record type tag.
    UnknownTag(u8),
    /// Checksum mismatch between computed and stored CRC64 trailer.
    ChecksumMismatch {
        /// CRC64 stored in the snapshot trailer.
        expected: u64,
        /// CRC64 computed over the snapshot bytes.
        computed: u64,
    },
    /// Unexpected extra bytes after the CRC64 trailer.
    TrailingBytes,
    /// Invalid value encoding (e.g. NaN score, oversized blob, or invalid TTL flag).
    CorruptFormat(&'static str),
    /// Callback rejected a loaded record.
    Apply(String),
}

impl fmt::Display for SnapshotError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io(e) => write!(f, "snapshot I/O error: {e}"),
            Self::BadMagic(m) => write!(f, "invalid snapshot magic: {m:02x?}"),
            Self::UnsupportedVersion(v) => write!(f, "unsupported snapshot version: {v}"),
            Self::UnknownTag(t) => write!(f, "unknown snapshot record tag: 0x{t:02x}"),
            Self::ChecksumMismatch { expected, computed } => write!(
                f,
                "snapshot CRC64 mismatch: expected 0x{expected:016x}, computed 0x{computed:016x}"
            ),
            Self::TrailingBytes => write!(f, "unexpected trailing bytes after snapshot trailer"),
            Self::CorruptFormat(msg) => write!(f, "corrupt snapshot format: {msg}"),
            Self::Apply(msg) => write!(f, "snapshot apply error: {msg}"),
        }
    }
}

impl std::error::Error for SnapshotError {}

impl From<io::Error> for SnapshotError {
    fn from(e: io::Error) -> Self {
        Self::Io(e)
    }
}

/// Summary statistics from saving or loading a snapshot.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SnapshotStats {
    /// Number of key records written or loaded.
    pub records: u64,
    /// Total snapshot size in bytes (including header and CRC64 trailer).
    pub bytes: u64,
    /// CRC64-Jones checksum of the snapshot.
    pub crc64: u64,
}

/// Streaming writer for version-1 CRC64-checksummed binary snapshots.
pub struct SnapshotWriter<W: Write> {
    writer: W,
    crc: Crc64,
    records: u64,
    bytes: u64,
}

impl<W: Write> SnapshotWriter<W> {
    /// Initializes a snapshot stream by writing the header.
    ///
    /// # Errors
    /// Returns [`SnapshotError::Io`] on write failure.
    pub fn start(mut writer: W) -> Result<Self, SnapshotError> {
        let mut header = [0u8; 8];
        header[..4].copy_from_slice(&SNAPSHOT_MAGIC);
        header[4..6].copy_from_slice(&SNAPSHOT_VERSION.to_be_bytes());
        header[6..8].copy_from_slice(&0u16.to_be_bytes());

        let mut crc = Crc64::new();
        crc.update(&header);
        writer.write_all(&header)?;
        Ok(Self {
            writer,
            crc,
            records: 0,
            bytes: header.len() as u64,
        })
    }

    fn write_hashed(&mut self, data: &[u8]) -> Result<(), SnapshotError> {
        self.crc.update(data);
        self.writer.write_all(data)?;
        self.bytes += data.len() as u64;
        Ok(())
    }

    fn write_u8(&mut self, v: u8) -> Result<(), SnapshotError> {
        self.write_hashed(&[v])
    }

    fn write_u32(&mut self, v: u32) -> Result<(), SnapshotError> {
        self.write_hashed(&v.to_be_bytes())
    }

    fn write_u64(&mut self, v: u64) -> Result<(), SnapshotError> {
        self.write_hashed(&v.to_be_bytes())
    }

    fn write_blob(&mut self, bytes: &[u8]) -> Result<(), SnapshotError> {
        let len = u32::try_from(bytes.len())
            .map_err(|_| SnapshotError::CorruptFormat("blob length exceeds u32::MAX"))?;
        self.write_u32(len)?;
        self.write_hashed(bytes)
    }

    fn write_expiry(&mut self, expiry_ms: Option<u64>) -> Result<(), SnapshotError> {
        match expiry_ms {
            Some(ts) => {
                self.write_u8(1)?;
                self.write_u64(ts)
            }
            None => self.write_u8(0),
        }
    }

    /// Writes a single [`SnapshotRecord`] into the snapshot stream.
    ///
    /// # Errors
    /// Returns [`SnapshotError::CorruptFormat`] on invalid data (such as `NaN`
    /// score) or [`SnapshotError::Io`] on write failure.
    pub fn write_record(&mut self, record: &SnapshotRecord) -> Result<(), SnapshotError> {
        match record {
            SnapshotRecord::String {
                key,
                value,
                expiry_ms,
            } => {
                self.write_u8(TAG_STRING)?;
                self.write_blob(key)?;
                self.write_blob(value)?;
                self.write_expiry(*expiry_ms)?;
            }
            SnapshotRecord::Hash {
                key,
                fields,
                expiry_ms,
            } => {
                self.write_u8(TAG_HASH)?;
                self.write_blob(key)?;
                let count = u32::try_from(fields.len())
                    .map_err(|_| SnapshotError::CorruptFormat("hash field count exceeds u32"))?;
                self.write_u32(count)?;
                for (field, value) in fields {
                    self.write_blob(field)?;
                    self.write_blob(value)?;
                }
                self.write_expiry(*expiry_ms)?;
            }
            SnapshotRecord::ZSet {
                key,
                entries,
                expiry_ms,
            } => {
                self.write_u8(TAG_ZSET)?;
                self.write_blob(key)?;
                let count = u32::try_from(entries.len())
                    .map_err(|_| SnapshotError::CorruptFormat("zset member count exceeds u32"))?;
                self.write_u32(count)?;
                for (score, member) in entries {
                    if score.is_nan() {
                        return Err(SnapshotError::CorruptFormat("NaN score in zset snapshot"));
                    }
                    let canonical = if *score == 0.0 { 0.0f64 } else { *score };
                    self.write_u64(canonical.to_bits())?;
                    self.write_blob(member)?;
                }
                self.write_expiry(*expiry_ms)?;
            }
            SnapshotRecord::Bitmap {
                key,
                bits,
                expiry_ms,
            } => {
                self.write_u8(TAG_BITMAP)?;
                self.write_blob(key)?;
                let count = u32::try_from(bits.len())
                    .map_err(|_| SnapshotError::CorruptFormat("bitmap bit count exceeds u32"))?;
                self.write_u32(count)?;
                for &bit in bits {
                    self.write_u64(bit)?;
                }
                self.write_expiry(*expiry_ms)?;
            }
        }
        self.records += 1;
        Ok(())
    }

    /// Writes the EOF marker and trailing CRC64 checksum, returning the
    /// underlying writer and summary statistics.
    ///
    /// # Errors
    /// Returns [`SnapshotError::Io`] on write or flush failure.
    pub fn finish(mut self) -> Result<(W, SnapshotStats), SnapshotError> {
        self.write_u8(TAG_EOF)?;
        let digest = self.crc.finalize();
        self.writer.write_all(&digest.to_be_bytes())?;
        self.writer.flush()?;
        let stats = SnapshotStats {
            records: self.records,
            bytes: self.bytes + 8,
            crc64: digest,
        };
        Ok((self.writer, stats))
    }
}

/// Atomically writes a binary snapshot to `target_path` via a sibling temporary
/// file, `sync_all()`, and `rename()`.
///
/// # Errors
/// Returns [`SnapshotError`] if writing, syncing, or renaming fails.
pub fn save_snapshot_atomic<F>(
    target_path: impl AsRef<Path>,
    mut write_records: F,
) -> Result<SnapshotStats, SnapshotError>
where
    F: FnMut(&mut SnapshotWriter<BufWriter<File>>) -> Result<(), SnapshotError>,
{
    let target = target_path.as_ref();
    let mut tmp_name = target.file_name().map_or_else(
        || "snapshot".to_string(),
        |s| s.to_string_lossy().into_owned(),
    );
    tmp_name.push_str(&format!(".{}.tmp", std::process::id()));
    let tmp_path = target.with_file_name(tmp_name);

    let file = OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(true)
        .open(&tmp_path)?;
    let mut writer = SnapshotWriter::start(BufWriter::with_capacity(256 * 1024, file))?;

    if let Err(err) = write_records(&mut writer) {
        drop(writer);
        if tmp_path.exists() {
            let remove_res = std::fs::remove_file(&tmp_path);
            if let Err(rm_err) = remove_res {
                return Err(SnapshotError::Io(rm_err));
            }
        }
        return Err(err);
    }

    let (buf_writer, stats) = writer.finish()?;
    buf_writer.get_ref().sync_all()?;
    drop(buf_writer);

    std::fs::rename(&tmp_path, target)?;
    Ok(stats)
}

struct HashedReader<R: Read> {
    reader: R,
    crc: Crc64,
    bytes: u64,
}

impl<R: Read> HashedReader<R> {
    fn new(reader: R) -> Self {
        Self {
            reader,
            crc: Crc64::new(),
            bytes: 0,
        }
    }

    fn read_exact_hashed(&mut self, buf: &mut [u8]) -> Result<(), SnapshotError> {
        self.reader.read_exact(buf)?;
        self.crc.update(buf);
        self.bytes += buf.len() as u64;
        Ok(())
    }

    fn read_u8(&mut self) -> Result<u8, SnapshotError> {
        let mut b = [0u8; 1];
        self.read_exact_hashed(&mut b)?;
        Ok(b[0])
    }

    fn read_u16(&mut self) -> Result<u16, SnapshotError> {
        let mut b = [0u8; 2];
        self.read_exact_hashed(&mut b)?;
        Ok(u16::from_be_bytes(b))
    }

    fn read_u32(&mut self) -> Result<u32, SnapshotError> {
        let mut b = [0u8; 4];
        self.read_exact_hashed(&mut b)?;
        Ok(u32::from_be_bytes(b))
    }

    fn read_u64(&mut self) -> Result<u64, SnapshotError> {
        let mut b = [0u8; 8];
        self.read_exact_hashed(&mut b)?;
        Ok(u64::from_be_bytes(b))
    }

    fn read_blob(&mut self) -> Result<Vec<u8>, SnapshotError> {
        let len = self.read_u32()? as usize;
        if len > MAX_BLOB_LEN {
            return Err(SnapshotError::CorruptFormat("blob length exceeds limit"));
        }
        let mut out = vec![0u8; len];
        self.read_exact_hashed(&mut out)?;
        Ok(out)
    }

    fn read_expiry(&mut self) -> Result<Option<u64>, SnapshotError> {
        match self.read_u8()? {
            0 => Ok(None),
            1 => Ok(Some(self.read_u64()?)),
            _ => Err(SnapshotError::CorruptFormat("invalid expiry flag")),
        }
    }
}

/// Loads and CRC64-verifies a binary snapshot from `path`, invoking `on_record`
/// for each decoded [`SnapshotRecord`].
///
/// The CRC64 trailer is verified before returning `Ok`; callers that require
/// atomic application should buffer or verify the snapshot before committing
/// state.
///
/// # Errors
/// Returns [`SnapshotError`] on I/O failure, bad magic/version, corrupt
/// framing, CRC64 mismatch, trailing bytes, or callback error.
pub fn load_snapshot<F>(
    path: impl AsRef<Path>,
    on_record: F,
) -> Result<SnapshotStats, SnapshotError>
where
    F: FnMut(SnapshotRecord) -> Result<(), String>,
{
    let file = File::open(path)?;
    load_snapshot_from_reader(BufReader::with_capacity(256 * 1024, file), on_record)
}

/// Loads and CRC64-verifies a binary snapshot from any [`Read`] stream.
///
/// # Errors
/// Returns [`SnapshotError`] on I/O failure, bad magic/version, corrupt
/// framing, CRC64 mismatch, trailing bytes, or callback error.
pub fn load_snapshot_from_reader<R, F>(
    reader: R,
    mut on_record: F,
) -> Result<SnapshotStats, SnapshotError>
where
    R: Read,
    F: FnMut(SnapshotRecord) -> Result<(), String>,
{
    let mut hr = HashedReader::new(reader);

    let mut magic = [0u8; 4];
    hr.read_exact_hashed(&mut magic)?;
    if magic != SNAPSHOT_MAGIC {
        return Err(SnapshotError::BadMagic(magic));
    }

    let version = hr.read_u16()?;
    if version != SNAPSHOT_VERSION {
        return Err(SnapshotError::UnsupportedVersion(version));
    }
    let _flags = hr.read_u16()?;

    let mut records = 0u64;
    loop {
        let tag = hr.read_u8()?;
        match tag {
            TAG_STRING => {
                let key = hr.read_blob()?;
                let value = hr.read_blob()?;
                let expiry_ms = hr.read_expiry()?;
                on_record(SnapshotRecord::String {
                    key,
                    value,
                    expiry_ms,
                })
                .map_err(SnapshotError::Apply)?;
                records += 1;
            }
            TAG_HASH => {
                let key = hr.read_blob()?;
                let count = hr.read_u32()? as usize;
                let mut fields = Vec::with_capacity(count.min(4096));
                for _ in 0..count {
                    let field = hr.read_blob()?;
                    let value = hr.read_blob()?;
                    fields.push((field, value));
                }
                let expiry_ms = hr.read_expiry()?;
                on_record(SnapshotRecord::Hash {
                    key,
                    fields,
                    expiry_ms,
                })
                .map_err(SnapshotError::Apply)?;
                records += 1;
            }
            TAG_ZSET => {
                let key = hr.read_blob()?;
                let count = hr.read_u32()? as usize;
                let mut entries = Vec::with_capacity(count.min(4096));
                for _ in 0..count {
                    let score = f64::from_bits(hr.read_u64()?);
                    if score.is_nan() {
                        return Err(SnapshotError::CorruptFormat("NaN score in snapshot"));
                    }
                    let member = hr.read_blob()?;
                    entries.push((score, member));
                }
                let expiry_ms = hr.read_expiry()?;
                on_record(SnapshotRecord::ZSet {
                    key,
                    entries,
                    expiry_ms,
                })
                .map_err(SnapshotError::Apply)?;
                records += 1;
            }
            TAG_BITMAP => {
                let key = hr.read_blob()?;
                let count = hr.read_u32()? as usize;
                let mut bits = Vec::with_capacity(count.min(4096));
                for _ in 0..count {
                    bits.push(hr.read_u64()?);
                }
                let expiry_ms = hr.read_expiry()?;
                on_record(SnapshotRecord::Bitmap {
                    key,
                    bits,
                    expiry_ms,
                })
                .map_err(SnapshotError::Apply)?;
                records += 1;
            }
            TAG_EOF => break,
            other => return Err(SnapshotError::UnknownTag(other)),
        }
    }

    let computed = hr.crc.finalize();
    let mut trailer = [0u8; 8];
    hr.reader.read_exact(&mut trailer)?;
    let expected = u64::from_be_bytes(trailer);
    if expected != computed {
        return Err(SnapshotError::ChecksumMismatch { expected, computed });
    }

    // Reject trailing garbage beyond the CRC64 trailer.
    let mut extra = [0u8; 1];
    if hr.reader.read(&mut extra)? != 0 {
        return Err(SnapshotError::TrailingBytes);
    }

    Ok(SnapshotStats {
        records,
        bytes: hr.bytes + 8,
        crc64: computed,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicU64, Ordering};

    fn unique_snapshot_path(label: &str) -> PathBuf {
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let id = COUNTER.fetch_add(1, Ordering::Relaxed);
        std::env::temp_dir().join(format!(
            "expanse-kv-snap-{label}-{}-{id}.ekvs",
            std::process::id()
        ))
    }

    fn sample_records() -> Vec<SnapshotRecord> {
        vec![
            SnapshotRecord::String {
                key: b"org:1:session".to_vec(),
                value: b"active".to_vec(),
                expiry_ms: Some(1_800_000_000_000),
            },
            SnapshotRecord::Hash {
                key: b"user:42".to_vec(),
                fields: vec![
                    (b"name".to_vec(), b"ada".to_vec()),
                    (b"role".to_vec(), b"admin".to_vec()),
                ],
                expiry_ms: None,
            },
            SnapshotRecord::ZSet {
                key: b"board".to_vec(),
                entries: vec![(-1.5, b"alice".to_vec()), (42.25, b"bob".to_vec())],
                expiry_ms: Some(1_900_000_000_000),
            },
            SnapshotRecord::Bitmap {
                key: b"flags".to_vec(),
                bits: vec![0, 7, 1024, 1_000_000],
                expiry_ms: None,
            },
        ]
    }

    #[test]
    fn save_and_load_roundtrip_all_types() {
        let path = unique_snapshot_path("roundtrip");
        let records = sample_records();

        let save_stats = save_snapshot_atomic(&path, |w| {
            for rec in &records {
                w.write_record(rec)?;
            }
            Ok(())
        })
        .unwrap();
        assert_eq!(save_stats.records, 4);

        let mut loaded = Vec::new();
        let load_stats = load_snapshot(&path, |rec| {
            loaded.push(rec);
            Ok(())
        })
        .unwrap();

        assert_eq!(load_stats, save_stats);
        assert_eq!(loaded, records);
        std::fs::remove_file(&path).unwrap();
    }

    #[test]
    fn rejects_single_bit_corruption_anywhere() {
        let path = unique_snapshot_path("corrupt");
        let records = sample_records();
        save_snapshot_atomic(&path, |w| {
            for rec in &records {
                w.write_record(rec)?;
            }
            Ok(())
        })
        .unwrap();

        let raw = std::fs::read(&path).unwrap();
        // Flip a bit in header, payload, and trailer and verify every flip is rejected.
        for &idx in &[0usize, 4, 12, raw.len() / 2, raw.len() - 9, raw.len() - 1] {
            let mut corrupted = raw.clone();
            corrupted[idx] ^= 0x01;
            let res = load_snapshot_from_reader(corrupted.as_slice(), |_| Ok(()));
            assert!(
                res.is_err(),
                "bit flip at offset {idx} was unexpectedly accepted"
            );
        }

        std::fs::remove_file(&path).unwrap();
    }

    #[test]
    fn rejects_trailing_bytes() {
        let mut buf = Vec::new();
        let mut w = SnapshotWriter::start(&mut buf).unwrap();
        w.write_record(&SnapshotRecord::String {
            key: b"k".to_vec(),
            value: b"v".to_vec(),
            expiry_ms: None,
        })
        .unwrap();
        let (_out, _stats) = w.finish().unwrap();
        buf.push(0x00);

        let err = load_snapshot_from_reader(buf.as_slice(), |_| Ok(())).unwrap_err();
        assert!(matches!(err, SnapshotError::TrailingBytes));
    }
}
