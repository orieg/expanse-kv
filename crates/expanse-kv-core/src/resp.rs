//! Zero-copy RESP2/RESP3 streaming parser, inline command parser, and encoder
//! (`docs/PRD.md` §4.1, §7 Phase 1).
//!
//! Strictly `#![no_std]` + `alloc` with `#![forbid(unsafe_code)]` and zero
//! external dependencies (`AGENTS.md` §2.6, §2.7).
//!
//! # Two Entry Points
//!
//! 1. [`parse_command_into`]: zero-allocation streaming command parser for
//!    pipelined Redis client requests (both RESP multibulk `*N\r\n$L\r\n...`
//!    and inline commands `PING\r\n`). Reuses a caller-owned `Vec<&'a [u8]>`
//!    so pipelined `GET`/`SET` parsing allocates zero bytes per frame.
//! 2. [`parse_frame`] / [`parse_frame_with_limits`]: general-purpose recursive
//!    RESP2 + RESP3 streaming parser supporting all 16 frame variants with
//!    strict depth, bulk-size, and collection-size limits.

use alloc::vec::Vec;
use core::fmt;

/// Default maximum container nesting depth (`Array`, `Map`, `Set`, `Push`).
pub const DEFAULT_MAX_DEPTH: usize = 64;

/// Default maximum single bulk payload length (512 MiB, matching Redis proto-max-bulk-len).
pub const DEFAULT_MAX_BULK_LEN: usize = 512 * 1024 * 1024;

/// Default maximum number of elements in one aggregate (`Array`, `Map`, `Set`, `Push`).
pub const DEFAULT_MAX_COLLECTION_LEN: usize = 1_048_576;

/// Default maximum inline command line length (64 KiB, matching Redis proto-max-inline).
pub const DEFAULT_MAX_INLINE_LEN: usize = 65_536;

/// Safety limits enforced by the RESP parser against crafted or adversarial inputs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ParseLimits {
    /// Maximum recursive container nesting depth.
    pub max_depth: usize,
    /// Maximum byte length of a single bulk string, bulk error, or verbatim string.
    pub max_bulk_len: usize,
    /// Maximum element count (or pair count for `Map`) in a single aggregate.
    pub max_collection_len: usize,
    /// Maximum byte length of a single inline command line.
    pub max_inline_len: usize,
}

impl Default for ParseLimits {
    fn default() -> Self {
        Self {
            max_depth: DEFAULT_MAX_DEPTH,
            max_bulk_len: DEFAULT_MAX_BULK_LEN,
            max_collection_len: DEFAULT_MAX_COLLECTION_LEN,
            max_inline_len: DEFAULT_MAX_INLINE_LEN,
        }
    }
}

/// A borrowed RESP2 or RESP3 protocol frame referencing slices of the input buffer.
#[derive(Debug, Clone, PartialEq)]
pub enum RespFrame<'a> {
    /// RESP2/RESP3 Simple String (`+OK\r\n`).
    SimpleString(&'a [u8]),
    /// RESP2/RESP3 Simple Error (`-ERR msg\r\n`).
    SimpleError(&'a [u8]),
    /// RESP2/RESP3 64-bit signed integer (`:1000\r\n`).
    Integer(i64),
    /// RESP2/RESP3 Bulk String (`$5\r\nhello\r\n`).
    BulkString(&'a [u8]),
    /// RESP2 Null Bulk String (`$-1\r\n`).
    NullBulkString,
    /// RESP2/RESP3 Array (`*2\r\n...`).
    Array(Vec<RespFrame<'a>>),
    /// RESP2 Null Array (`*-1\r\n`).
    NullArray,
    /// RESP3 Null (`_\r\n`).
    Null,
    /// RESP3 Boolean (`#t\r\n` / `#f\r\n`).
    Boolean(bool),
    /// RESP3 Double (`,3.14\r\n`, `,inf\r\n`, `,-inf\r\n`, `,nan\r\n`).
    Double(f64),
    /// RESP3 BigNumber (`(3492890328409238509324850943850943825024385\r\n`).
    BigNumber(&'a [u8]),
    /// RESP3 Bulk Error (`!21\r\nSYNTAX invalid syntax\r\n`).
    BulkError(&'a [u8]),
    /// RESP3 Verbatim String (`=15\r\ntxt:Some string\r\n`).
    VerbatimString {
        /// 3-byte format tag (e.g. `b"txt"`, `b"mkd"`).
        format: [u8; 3],
        /// Payload bytes following the `:` separator.
        data: &'a [u8],
    },
    /// RESP3 Map (`%2\r\n...`, sequence of `(key, value)` pairs).
    Map(Vec<(RespFrame<'a>, RespFrame<'a>)>),
    /// RESP3 Set (`~3\r\n...`).
    Set(Vec<RespFrame<'a>>),
    /// RESP3 Push (`>2\r\n...`).
    Push(Vec<RespFrame<'a>>),
}

/// Streaming parse outcome for [`parse_frame`] / [`parse_frame_with_limits`].
#[derive(Debug, Clone, PartialEq)]
pub enum ParseStatus<'a> {
    /// A complete frame was parsed, consuming `consumed` bytes from `input`.
    Complete {
        /// Parsed RESP frame borrowing from `input`.
        frame: RespFrame<'a>,
        /// Number of bytes consumed from the start of `input`.
        consumed: usize,
    },
    /// More bytes are required before a complete frame can be parsed.
    Incomplete,
}

/// Streaming parse outcome for [`parse_command_into`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CommandStatus {
    /// A complete command was parsed into `args`, consuming `consumed` bytes.
    Complete {
        /// Number of bytes consumed from the start of `input`.
        consumed: usize,
    },
    /// More bytes are required before a complete command can be parsed.
    Incomplete,
}

/// Protocol error returned when the input stream violates RESP2/RESP3 framing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RespError {
    /// Unrecognized leading RESP type byte.
    InvalidPrefix(u8),
    /// Expected `\r\n` terminator was missing or malformed.
    InvalidCrlf,
    /// Integer, length, or double payload could not be parsed.
    InvalidNumber,
    /// Negative length other than `-1` (where null is allowed).
    InvalidLength,
    /// Boolean byte was neither `t` nor `f`.
    InvalidBoolean,
    /// Verbatim string was shorter than 4 bytes or lacked `:` at index 3.
    InvalidVerbatim,
    /// BigNumber payload was empty or contained non-digit bytes.
    InvalidBigNumber,
    /// RESP3 Null frame had trailing bytes before `\r\n`.
    InvalidNull,
    /// Container nesting exceeded [`ParseLimits::max_depth`].
    MaxDepthExceeded,
    /// Bulk payload exceeded [`ParseLimits::max_bulk_len`].
    MaxBulkLenExceeded,
    /// Collection element count exceeded [`ParseLimits::max_collection_len`].
    MaxCollectionLenExceeded,
    /// Inline command line exceeded [`ParseLimits::max_inline_len`].
    MaxInlineLenExceeded,
    /// Multibulk command element was not a bulk string (`$`).
    ExpectedBulkString,
    /// Inline command contained an unclosed quote.
    UnclosedQuote,
}

impl fmt::Display for RespError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidPrefix(b) => write!(f, "invalid RESP type prefix byte 0x{b:02x}"),
            Self::InvalidCrlf => f.write_str("invalid or missing CRLF terminator"),
            Self::InvalidNumber => f.write_str("invalid numeric value in RESP frame"),
            Self::InvalidLength => f.write_str("invalid negative length in RESP frame"),
            Self::InvalidBoolean => f.write_str("invalid RESP3 boolean value"),
            Self::InvalidVerbatim => f.write_str("invalid RESP3 verbatim string"),
            Self::InvalidBigNumber => f.write_str("invalid RESP3 big number"),
            Self::InvalidNull => f.write_str("invalid RESP3 null frame"),
            Self::MaxDepthExceeded => f.write_str("maximum RESP container nesting depth exceeded"),
            Self::MaxBulkLenExceeded => f.write_str("maximum RESP bulk string length exceeded"),
            Self::MaxCollectionLenExceeded => {
                f.write_str("maximum RESP collection element count exceeded")
            }
            Self::MaxInlineLenExceeded => f.write_str("maximum inline command length exceeded"),
            Self::ExpectedBulkString => f.write_str("expected bulk string argument in command"),
            Self::UnclosedQuote => f.write_str("unclosed quote in inline command"),
        }
    }
}

impl core::error::Error for RespError {}

/// Finds `\r\n` starting at `pos`, returning `(line_slice, next_pos_after_crlf)`.
/// Returns `Ok(None)` if `\r\n` has not arrived yet.
#[inline]
fn read_line(input: &[u8], pos: usize) -> Result<Option<(&[u8], usize)>, RespError> {
    let slice = match input.get(pos..) {
        Some(s) => s,
        None => return Ok(None),
    };
    for i in 0..slice.len() {
        if slice[i] == b'\r' {
            if i + 1 >= slice.len() {
                return Ok(None);
            }
            if slice[i + 1] != b'\n' {
                return Err(RespError::InvalidCrlf);
            }
            return Ok(Some((&slice[..i], pos + i + 2)));
        }
        if slice[i] == b'\n' {
            return Err(RespError::InvalidCrlf);
        }
    }
    Ok(None)
}

/// Fast ASCII signed `i64` parser (no UTF-8 or `core::str::FromStr` overhead).
#[inline]
fn parse_i64_ascii(bytes: &[u8]) -> Result<i64, RespError> {
    if bytes.is_empty() {
        return Err(RespError::InvalidNumber);
    }
    let (neg, digits) = if bytes[0] == b'-' {
        (true, &bytes[1..])
    } else if bytes[0] == b'+' {
        (false, &bytes[1..])
    } else {
        (false, bytes)
    };
    if digits.is_empty() {
        return Err(RespError::InvalidNumber);
    }
    let mut acc: i64 = 0;
    for &b in digits {
        if !b.is_ascii_digit() {
            return Err(RespError::InvalidNumber);
        }
        let d = i64::from(b - b'0');
        acc = if neg {
            acc.checked_mul(10)
                .and_then(|v| v.checked_sub(d))
                .ok_or(RespError::InvalidNumber)?
        } else {
            acc.checked_mul(10)
                .and_then(|v| v.checked_add(d))
                .ok_or(RespError::InvalidNumber)?
        };
    }
    Ok(acc)
}

/// Fast non-negative ASCII `usize` parser for pipelined multibulk lengths.
#[inline]
fn parse_usize_line(input: &[u8], pos: usize) -> Result<Option<(usize, usize)>, RespError> {
    let slice = match input.get(pos..) {
        Some(s) => s,
        None => return Ok(None),
    };
    let slen = slice.len();
    if slen == 0 {
        return Ok(None);
    }
    // Fast path for 1-digit (`*2\r\n`, `$3\r\n`) and 2-digit (`$39\r\n`, `$64\r\n`) lengths.
    let b0 = slice[0];
    if b0.is_ascii_digit() {
        let d0 = usize::from(b0 - b'0');
        if slen >= 3 && slice[1] == b'\r' && slice[2] == b'\n' {
            return Ok(Some((d0, pos + 3)));
        }
        if slen >= 4 && slice[1].is_ascii_digit() && slice[2] == b'\r' && slice[3] == b'\n' {
            let d1 = usize::from(slice[1] - b'0');
            return Ok(Some((d0 * 10 + d1, pos + 4)));
        }
        if slen >= 5
            && slice[1].is_ascii_digit()
            && slice[2].is_ascii_digit()
            && slice[3] == b'\r'
            && slice[4] == b'\n'
        {
            let d1 = usize::from(slice[1] - b'0');
            let d2 = usize::from(slice[2] - b'0');
            return Ok(Some((d0 * 100 + d1 * 10 + d2, pos + 5)));
        }
    }
    let mut acc: usize = 0;
    let mut i = 0;
    while i < slen {
        let b = slice[i];
        if b.is_ascii_digit() {
            acc = acc
                .checked_mul(10)
                .and_then(|v| v.checked_add(usize::from(b - b'0')))
                .ok_or(RespError::InvalidNumber)?;
            i += 1;
        } else if b == b'\r' {
            if i == 0 {
                return Err(RespError::InvalidNumber);
            }
            if i + 1 >= slice.len() {
                return Ok(None);
            }
            if slice[i + 1] != b'\n' {
                return Err(RespError::InvalidCrlf);
            }
            return Ok(Some((acc, pos + i + 2)));
        } else {
            return Err(RespError::InvalidNumber);
        }
    }
    Ok(None)
}

/// Parses a single RESP2/RESP3 frame from `input` using [`ParseLimits::default`].
///
/// # Errors
///
/// Returns [`RespError`] if `input` contains malformed RESP framing or violates
/// a safety limit.
#[inline]
pub fn parse_frame(input: &[u8]) -> Result<ParseStatus<'_>, RespError> {
    parse_frame_with_limits(input, ParseLimits::default())
}

/// Parses a single RESP2/RESP3 frame from `input` enforcing `limits`.
///
/// # Errors
///
/// Returns [`RespError`] on malformed RESP framing or limit violations.
#[inline]
pub fn parse_frame_with_limits(
    input: &[u8],
    limits: ParseLimits,
) -> Result<ParseStatus<'_>, RespError> {
    match parse_at(input, 0, 0, limits)? {
        Some((frame, consumed)) => Ok(ParseStatus::Complete { frame, consumed }),
        None => Ok(ParseStatus::Incomplete),
    }
}

fn parse_at<'a>(
    input: &'a [u8],
    pos: usize,
    depth: usize,
    limits: ParseLimits,
) -> Result<Option<(RespFrame<'a>, usize)>, RespError> {
    let Some(&prefix) = input.get(pos) else {
        return Ok(None);
    };
    let body_pos = pos + 1;

    match prefix {
        b'+' => {
            let Some((line, next)) = read_line(input, body_pos)? else {
                return Ok(None);
            };
            Ok(Some((RespFrame::SimpleString(line), next)))
        }
        b'-' => {
            let Some((line, next)) = read_line(input, body_pos)? else {
                return Ok(None);
            };
            Ok(Some((RespFrame::SimpleError(line), next)))
        }
        b':' => {
            let Some((line, next)) = read_line(input, body_pos)? else {
                return Ok(None);
            };
            let n = parse_i64_ascii(line)?;
            Ok(Some((RespFrame::Integer(n), next)))
        }
        b'$' => {
            let Some((line, after_len)) = read_line(input, body_pos)? else {
                return Ok(None);
            };
            let len = parse_i64_ascii(line)?;
            if len == -1 {
                return Ok(Some((RespFrame::NullBulkString, after_len)));
            }
            if len < -1 {
                return Err(RespError::InvalidLength);
            }
            let len = usize::try_from(len).map_err(|_| RespError::MaxBulkLenExceeded)?;
            if len > limits.max_bulk_len {
                return Err(RespError::MaxBulkLenExceeded);
            }
            let end = after_len
                .checked_add(len)
                .ok_or(RespError::MaxBulkLenExceeded)?;
            let next = end.checked_add(2).ok_or(RespError::MaxBulkLenExceeded)?;
            if next > input.len() {
                return Ok(None);
            }
            if input[end] != b'\r' || input[end + 1] != b'\n' {
                return Err(RespError::InvalidCrlf);
            }
            Ok(Some((RespFrame::BulkString(&input[after_len..end]), next)))
        }
        b'*' => {
            if depth >= limits.max_depth {
                return Err(RespError::MaxDepthExceeded);
            }
            let Some((line, mut cur)) = read_line(input, body_pos)? else {
                return Ok(None);
            };
            let count = parse_i64_ascii(line)?;
            if count == -1 {
                return Ok(Some((RespFrame::NullArray, cur)));
            }
            if count < -1 {
                return Err(RespError::InvalidLength);
            }
            let count = usize::try_from(count).map_err(|_| RespError::MaxCollectionLenExceeded)?;
            if count > limits.max_collection_len {
                return Err(RespError::MaxCollectionLenExceeded);
            }
            // Cap initial allocation to avoid OOM on truncated headers claiming huge counts.
            let mut items = Vec::with_capacity(count.min(64));
            for _ in 0..count {
                let Some((item, next)) = parse_at(input, cur, depth + 1, limits)? else {
                    return Ok(None);
                };
                items.push(item);
                cur = next;
            }
            Ok(Some((RespFrame::Array(items), cur)))
        }
        b'_' => {
            let Some((line, next)) = read_line(input, body_pos)? else {
                return Ok(None);
            };
            if !line.is_empty() {
                return Err(RespError::InvalidNull);
            }
            Ok(Some((RespFrame::Null, next)))
        }
        b'#' => {
            let Some((line, next)) = read_line(input, body_pos)? else {
                return Ok(None);
            };
            match line {
                b"t" => Ok(Some((RespFrame::Boolean(true), next))),
                b"f" => Ok(Some((RespFrame::Boolean(false), next))),
                _ => Err(RespError::InvalidBoolean),
            }
        }
        b',' => {
            let Some((line, next)) = read_line(input, body_pos)? else {
                return Ok(None);
            };
            let s = core::str::from_utf8(line).map_err(|_| RespError::InvalidNumber)?;
            let val = match s {
                "inf" => f64::INFINITY,
                "-inf" => f64::NEG_INFINITY,
                "nan" => f64::NAN,
                _ => s.parse::<f64>().map_err(|_| RespError::InvalidNumber)?,
            };
            Ok(Some((RespFrame::Double(val), next)))
        }
        b'(' => {
            let Some((line, next)) = read_line(input, body_pos)? else {
                return Ok(None);
            };
            let digits = match line.first() {
                Some(b'-' | b'+') => &line[1..],
                Some(_) => line,
                None => return Err(RespError::InvalidBigNumber),
            };
            if digits.is_empty() || !digits.iter().all(u8::is_ascii_digit) {
                return Err(RespError::InvalidBigNumber);
            }
            Ok(Some((RespFrame::BigNumber(line), next)))
        }
        b'!' => {
            let Some((line, after_len)) = read_line(input, body_pos)? else {
                return Ok(None);
            };
            let len = parse_i64_ascii(line)?;
            if len < 0 {
                return Err(RespError::InvalidLength);
            }
            let len = usize::try_from(len).map_err(|_| RespError::MaxBulkLenExceeded)?;
            if len > limits.max_bulk_len {
                return Err(RespError::MaxBulkLenExceeded);
            }
            let end = after_len
                .checked_add(len)
                .ok_or(RespError::MaxBulkLenExceeded)?;
            let next = end.checked_add(2).ok_or(RespError::MaxBulkLenExceeded)?;
            if next > input.len() {
                return Ok(None);
            }
            if input[end] != b'\r' || input[end + 1] != b'\n' {
                return Err(RespError::InvalidCrlf);
            }
            Ok(Some((RespFrame::BulkError(&input[after_len..end]), next)))
        }
        b'=' => {
            let Some((line, after_len)) = read_line(input, body_pos)? else {
                return Ok(None);
            };
            let len = parse_i64_ascii(line)?;
            if len < 4 {
                return Err(RespError::InvalidVerbatim);
            }
            let len = usize::try_from(len).map_err(|_| RespError::MaxBulkLenExceeded)?;
            if len > limits.max_bulk_len {
                return Err(RespError::MaxBulkLenExceeded);
            }
            let end = after_len
                .checked_add(len)
                .ok_or(RespError::MaxBulkLenExceeded)?;
            let next = end.checked_add(2).ok_or(RespError::MaxBulkLenExceeded)?;
            if next > input.len() {
                return Ok(None);
            }
            if input[end] != b'\r' || input[end + 1] != b'\n' {
                return Err(RespError::InvalidCrlf);
            }
            let payload = &input[after_len..end];
            if payload[3] != b':' {
                return Err(RespError::InvalidVerbatim);
            }
            let format = [payload[0], payload[1], payload[2]];
            Ok(Some((
                RespFrame::VerbatimString {
                    format,
                    data: &payload[4..],
                },
                next,
            )))
        }
        b'%' => {
            if depth >= limits.max_depth {
                return Err(RespError::MaxDepthExceeded);
            }
            let Some((line, mut cur)) = read_line(input, body_pos)? else {
                return Ok(None);
            };
            let pairs = parse_i64_ascii(line)?;
            if pairs < 0 {
                return Err(RespError::InvalidLength);
            }
            let pairs = usize::try_from(pairs).map_err(|_| RespError::MaxCollectionLenExceeded)?;
            if pairs > limits.max_collection_len {
                return Err(RespError::MaxCollectionLenExceeded);
            }
            let mut entries = Vec::with_capacity(pairs.min(64));
            for _ in 0..pairs {
                let Some((k, next_k)) = parse_at(input, cur, depth + 1, limits)? else {
                    return Ok(None);
                };
                let Some((v, next_v)) = parse_at(input, next_k, depth + 1, limits)? else {
                    return Ok(None);
                };
                entries.push((k, v));
                cur = next_v;
            }
            Ok(Some((RespFrame::Map(entries), cur)))
        }
        b'~' | b'>' => {
            if depth >= limits.max_depth {
                return Err(RespError::MaxDepthExceeded);
            }
            let Some((line, mut cur)) = read_line(input, body_pos)? else {
                return Ok(None);
            };
            let count = parse_i64_ascii(line)?;
            if count < 0 {
                return Err(RespError::InvalidLength);
            }
            let count = usize::try_from(count).map_err(|_| RespError::MaxCollectionLenExceeded)?;
            if count > limits.max_collection_len {
                return Err(RespError::MaxCollectionLenExceeded);
            }
            let mut items = Vec::with_capacity(count.min(64));
            for _ in 0..count {
                let Some((item, next)) = parse_at(input, cur, depth + 1, limits)? else {
                    return Ok(None);
                };
                items.push(item);
                cur = next;
            }
            let frame = if prefix == b'~' {
                RespFrame::Set(items)
            } else {
                RespFrame::Push(items)
            };
            Ok(Some((frame, cur)))
        }
        other => Err(RespError::InvalidPrefix(other)),
    }
}

/// Zero-allocation streaming command parser for pipelined Redis client requests
/// (RESP multibulk arrays `*N\r\n$L\r\n...` and inline commands `PING\r\n`).
///
/// Clears `args` and appends borrowed argument slices from `input`. On
/// [`CommandStatus::Incomplete`], `args` may be cleared and the caller should
/// retry once more bytes arrive.
///
/// # Errors
///
/// Returns [`RespError`] on malformed framing or limit violations.
#[inline]
pub fn parse_command_into<'a>(
    input: &'a [u8],
    args: &mut Vec<&'a [u8]>,
    limits: ParseLimits,
) -> Result<CommandStatus, RespError> {
    args.clear();
    let Some(&first) = input.first() else {
        return Ok(CommandStatus::Incomplete);
    };

    if first == b'*' {
        let Some((count, mut cur)) = parse_usize_line(input, 1)? else {
            return Ok(CommandStatus::Incomplete);
        };
        if count > limits.max_collection_len {
            return Err(RespError::MaxCollectionLenExceeded);
        }
        for _ in 0..count {
            let Some(&b) = input.get(cur) else {
                return Ok(CommandStatus::Incomplete);
            };
            if b != b'$' {
                return Err(RespError::ExpectedBulkString);
            }
            let Some((len, after_len)) = parse_usize_line(input, cur + 1)? else {
                return Ok(CommandStatus::Incomplete);
            };
            if len > limits.max_bulk_len {
                return Err(RespError::MaxBulkLenExceeded);
            }
            let end = after_len
                .checked_add(len)
                .ok_or(RespError::MaxBulkLenExceeded)?;
            let next = end.checked_add(2).ok_or(RespError::MaxBulkLenExceeded)?;
            if next > input.len() {
                return Ok(CommandStatus::Incomplete);
            }
            if input[end] != b'\r' || input[end + 1] != b'\n' {
                return Err(RespError::InvalidCrlf);
            }
            args.push(&input[after_len..end]);
            cur = next;
        }
        Ok(CommandStatus::Complete { consumed: cur })
    } else {
        parse_inline_command_into(input, args, limits)
    }
}

/// Parses a Redis inline command (`PING\r\n`, `SET k v\r\n`, `GET "my key"\r\n`).
fn parse_inline_command_into<'a>(
    input: &'a [u8],
    args: &mut Vec<&'a [u8]>,
    limits: ParseLimits,
) -> Result<CommandStatus, RespError> {
    let search_end = input.len().min(limits.max_inline_len.saturating_add(2));
    let mut nl_pos = None;
    for (i, &b) in input[..search_end].iter().enumerate() {
        if b == b'\n' {
            nl_pos = Some(i);
            break;
        }
    }
    let Some(nl) = nl_pos else {
        if input.len() > limits.max_inline_len {
            return Err(RespError::MaxInlineLenExceeded);
        }
        return Ok(CommandStatus::Incomplete);
    };
    let consumed = nl + 1;
    let line_end = if nl > 0 && input[nl - 1] == b'\r' {
        nl - 1
    } else {
        nl
    };
    if line_end > limits.max_inline_len {
        return Err(RespError::MaxInlineLenExceeded);
    }
    let line = &input[..line_end];

    let mut i = 0;
    while i < line.len() {
        while i < line.len() && (line[i] == b' ' || line[i] == b'\t') {
            i += 1;
        }
        if i >= line.len() {
            break;
        }
        if line[i] == b'"' || line[i] == b'\'' {
            let quote = line[i];
            i += 1;
            let start = i;
            while i < line.len() && line[i] != quote {
                i += 1;
            }
            if i >= line.len() {
                return Err(RespError::UnclosedQuote);
            }
            args.push(&line[start..i]);
            i += 1;
        } else {
            let start = i;
            while i < line.len() && line[i] != b' ' && line[i] != b'\t' {
                i += 1;
            }
            args.push(&line[start..i]);
        }
    }
    Ok(CommandStatus::Complete { consumed })
}

// ---------------------------------------------------------------------------
// Fast RESP2 / RESP3 Encoders
// ---------------------------------------------------------------------------

/// Appends decimal ASCII representation of `n` to `out` without allocating.
#[inline]
fn push_i64_ascii(n: i64, out: &mut Vec<u8>) {
    let mut buf = [0u8; 20];
    let neg = n < 0;
    let mut u = if neg {
        n.wrapping_neg() as u64
    } else {
        n as u64
    };
    let mut idx = buf.len();
    loop {
        idx -= 1;
        buf[idx] = b'0' + (u % 10) as u8;
        u /= 10;
        if u == 0 {
            break;
        }
    }
    if neg {
        out.push(b'-');
    }
    out.extend_from_slice(&buf[idx..]);
}

/// Appends decimal ASCII representation of `n` to `out` without allocating.
#[inline]
fn push_usize_ascii(mut n: usize, out: &mut Vec<u8>) {
    let mut buf = [0u8; 20];
    let mut idx = buf.len();
    loop {
        idx -= 1;
        buf[idx] = b'0' + (n % 10) as u8;
        n /= 10;
        if n == 0 {
            break;
        }
    }
    out.extend_from_slice(&buf[idx..]);
}

/// Encodes a RESP Simple String (`+<s\r\n>`).
#[inline]
pub fn encode_simple_string(s: &[u8], out: &mut Vec<u8>) {
    out.reserve(s.len() + 3);
    out.push(b'+');
    out.extend_from_slice(s);
    out.extend_from_slice(b"\r\n");
}

/// Encodes a RESP Simple Error (`-<s\r\n>`).
#[inline]
pub fn encode_simple_error(s: &[u8], out: &mut Vec<u8>) {
    out.reserve(s.len() + 3);
    out.push(b'-');
    out.extend_from_slice(s);
    out.extend_from_slice(b"\r\n");
}

/// Encodes a RESP Integer (`:<n>\r\n`).
#[inline]
pub fn encode_integer(n: i64, out: &mut Vec<u8>) {
    out.push(b':');
    push_i64_ascii(n, out);
    out.extend_from_slice(b"\r\n");
}

/// Encodes a RESP Bulk String (`$<len>\r\n<bytes>\r\n`).
#[inline]
pub fn encode_bulk_string(bytes: &[u8], out: &mut Vec<u8>) {
    let len = bytes.len();
    out.reserve(len + 12);
    if len < 10 {
        out.extend_from_slice(&[b'$', b'0' + len as u8, b'\r', b'\n']);
    } else if len < 100 {
        out.extend_from_slice(&[
            b'$',
            b'0' + (len / 10) as u8,
            b'0' + (len % 10) as u8,
            b'\r',
            b'\n',
        ]);
    } else if len < 1000 {
        out.extend_from_slice(&[
            b'$',
            b'0' + (len / 100) as u8,
            b'0' + ((len / 10) % 10) as u8,
            b'0' + (len % 10) as u8,
            b'\r',
            b'\n',
        ]);
    } else {
        out.push(b'$');
        push_usize_ascii(len, out);
        out.extend_from_slice(b"\r\n");
    }
    out.extend_from_slice(bytes);
    out.extend_from_slice(b"\r\n");
}

/// Encodes a RESP2 Null Bulk String (`$-1\r\n`).
#[inline]
pub fn encode_null_bulk(out: &mut Vec<u8>) {
    out.extend_from_slice(b"$-1\r\n");
}

/// Encodes a RESP3 Null (`_\r\n`).
#[inline]
pub fn encode_null(out: &mut Vec<u8>) {
    out.extend_from_slice(b"_\r\n");
}

/// Encodes a RESP Array header (`*<len>\r\n`).
#[inline]
pub fn encode_array_header(len: usize, out: &mut Vec<u8>) {
    out.push(b'*');
    push_usize_ascii(len, out);
    out.extend_from_slice(b"\r\n");
}

/// Encodes any [`RespFrame`] into `out`.
pub fn encode_frame(frame: &RespFrame<'_>, out: &mut Vec<u8>) {
    match frame {
        RespFrame::SimpleString(s) => encode_simple_string(s, out),
        RespFrame::SimpleError(s) => encode_simple_error(s, out),
        RespFrame::Integer(n) => encode_integer(*n, out),
        RespFrame::BulkString(b) => encode_bulk_string(b, out),
        RespFrame::NullBulkString => encode_null_bulk(out),
        RespFrame::NullArray => out.extend_from_slice(b"*-1\r\n"),
        RespFrame::Array(items) => {
            encode_array_header(items.len(), out);
            for item in items {
                encode_frame(item, out);
            }
        }
        RespFrame::Null => encode_null(out),
        RespFrame::Boolean(b) => {
            out.extend_from_slice(if *b { b"#t\r\n" } else { b"#f\r\n" });
        }
        RespFrame::Double(v) => {
            out.push(b',');
            if v.is_nan() {
                out.extend_from_slice(b"nan");
            } else if *v == f64::INFINITY {
                out.extend_from_slice(b"inf");
            } else if *v == f64::NEG_INFINITY {
                out.extend_from_slice(b"-inf");
            } else {
                use alloc::string::ToString;
                out.extend_from_slice(v.to_string().as_bytes());
            }
            out.extend_from_slice(b"\r\n");
        }
        RespFrame::BigNumber(digits) => {
            out.push(b'(');
            out.extend_from_slice(digits);
            out.extend_from_slice(b"\r\n");
        }
        RespFrame::BulkError(msg) => {
            out.push(b'!');
            push_usize_ascii(msg.len(), out);
            out.extend_from_slice(b"\r\n");
            out.extend_from_slice(msg);
            out.extend_from_slice(b"\r\n");
        }
        RespFrame::VerbatimString { format, data } => {
            out.push(b'=');
            push_usize_ascii(4 + data.len(), out);
            out.extend_from_slice(b"\r\n");
            out.extend_from_slice(format);
            out.push(b':');
            out.extend_from_slice(data);
            out.extend_from_slice(b"\r\n");
        }
        RespFrame::Map(entries) => {
            out.push(b'%');
            push_usize_ascii(entries.len(), out);
            out.extend_from_slice(b"\r\n");
            for (k, v) in entries {
                encode_frame(k, out);
                encode_frame(v, out);
            }
        }
        RespFrame::Set(items) => {
            out.push(b'~');
            push_usize_ascii(items.len(), out);
            out.extend_from_slice(b"\r\n");
            for item in items {
                encode_frame(item, out);
            }
        }
        RespFrame::Push(items) => {
            out.push(b'>');
            push_usize_ascii(items.len(), out);
            out.extend_from_slice(b"\r\n");
            for item in items {
                encode_frame(item, out);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::vec;

    #[test]
    fn round_trip_all_resp2_and_resp3_frame_variants() {
        let frames = vec![
            RespFrame::SimpleString(b"OK"),
            RespFrame::SimpleError(b"ERR unknown command"),
            RespFrame::Integer(0),
            RespFrame::Integer(-1),
            RespFrame::Integer(i64::MAX),
            RespFrame::Integer(i64::MIN),
            RespFrame::BulkString(b""),
            RespFrame::BulkString(b"hello\r\n\x00world"),
            RespFrame::NullBulkString,
            RespFrame::NullArray,
            RespFrame::Null,
            RespFrame::Boolean(true),
            RespFrame::Boolean(false),
            RespFrame::Double(3.5),
            RespFrame::Double(f64::INFINITY),
            RespFrame::Double(f64::NEG_INFINITY),
            RespFrame::BigNumber(b"-3492890328409238509324850943850943825024385"),
            RespFrame::BulkError(b"SYNTAX invalid syntax"),
            RespFrame::VerbatimString {
                format: *b"txt",
                data: b"Some string",
            },
            RespFrame::Array(vec![
                RespFrame::BulkString(b"SET"),
                RespFrame::BulkString(b"k"),
                RespFrame::BulkString(b"v"),
            ]),
            RespFrame::Map(vec![
                (RespFrame::SimpleString(b"first"), RespFrame::Integer(1)),
                (RespFrame::SimpleString(b"second"), RespFrame::Integer(2)),
            ]),
            RespFrame::Set(vec![
                RespFrame::BulkString(b"a"),
                RespFrame::BulkString(b"b"),
            ]),
            RespFrame::Push(vec![
                RespFrame::SimpleString(b"message"),
                RespFrame::BulkString(b"chan"),
            ]),
        ];

        let mut buf = Vec::new();
        for frame in &frames {
            buf.clear();
            encode_frame(frame, &mut buf);
            // Every proper prefix must return Incomplete (streaming contract).
            for cut in 0..buf.len() {
                assert_eq!(
                    parse_frame(&buf[..cut]),
                    Ok(ParseStatus::Incomplete),
                    "prefix len {cut} of {frame:?} must be Incomplete"
                );
            }
            assert_eq!(
                parse_frame(&buf),
                Ok(ParseStatus::Complete {
                    frame: frame.clone(),
                    consumed: buf.len()
                })
            );
        }

        // Special NaN check (NaN != NaN).
        buf.clear();
        encode_frame(&RespFrame::Double(f64::NAN), &mut buf);
        match parse_frame(&buf).expect("nan parses") {
            ParseStatus::Complete {
                frame: RespFrame::Double(v),
                consumed,
            } => {
                assert!(v.is_nan());
                assert_eq!(consumed, buf.len());
            }
            other => panic!("unexpected parse result for NaN: {other:?}"),
        }
    }

    #[test]
    fn pipelined_command_and_inline_parsing() {
        let mut wire = Vec::new();
        wire.extend_from_slice(b"*2\r\n$3\r\nGET\r\n$4\r\nfoo1\r\n");
        wire.extend_from_slice(b"*3\r\n$3\r\nSET\r\n$4\r\nfoo2\r\n$3\r\nbar\r\n");
        wire.extend_from_slice(b"PING\r\n");
        wire.extend_from_slice(b"SET \"my key\" 'my val'\r\n");

        let limits = ParseLimits::default();
        let mut args = Vec::new();
        let mut off = 0;

        let CommandStatus::Complete { consumed } =
            parse_command_into(&wire[off..], &mut args, limits).expect("cmd 1")
        else {
            panic!("expected complete cmd 1");
        };
        assert_eq!(args, vec![b"GET".as_slice(), b"foo1".as_slice()]);
        off += consumed;

        let CommandStatus::Complete { consumed } =
            parse_command_into(&wire[off..], &mut args, limits).expect("cmd 2")
        else {
            panic!("expected complete cmd 2");
        };
        assert_eq!(
            args,
            vec![b"SET".as_slice(), b"foo2".as_slice(), b"bar".as_slice()]
        );
        off += consumed;

        let CommandStatus::Complete { consumed } =
            parse_command_into(&wire[off..], &mut args, limits).expect("cmd 3")
        else {
            panic!("expected complete cmd 3");
        };
        assert_eq!(args, vec![b"PING".as_slice()]);
        off += consumed;

        let CommandStatus::Complete { consumed } =
            parse_command_into(&wire[off..], &mut args, limits).expect("cmd 4")
        else {
            panic!("expected complete cmd 4");
        };
        assert_eq!(
            args,
            vec![
                b"SET".as_slice(),
                b"my key".as_slice(),
                b"my val".as_slice()
            ]
        );
        off += consumed;
        assert_eq!(off, wire.len());
    }

    #[test]
    fn safety_limits_and_malformed_inputs_never_panic() {
        let tight = ParseLimits {
            max_depth: 4,
            max_bulk_len: 16,
            max_collection_len: 8,
            max_inline_len: 16,
        };
        assert_eq!(
            parse_frame_with_limits(b"*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n:1\r\n", tight),
            Err(RespError::MaxDepthExceeded)
        );
        assert_eq!(
            parse_frame_with_limits(b"$17\r\n01234567890123456\r\n", tight),
            Err(RespError::MaxBulkLenExceeded)
        );
        assert_eq!(
            parse_frame_with_limits(b"*9\r\n", tight),
            Err(RespError::MaxCollectionLenExceeded)
        );
        assert_eq!(parse_frame(b"$-2\r\n"), Err(RespError::InvalidLength));
        assert_eq!(parse_frame(b"#x\r\n"), Err(RespError::InvalidBoolean));
        assert_eq!(parse_frame(b"_extra\r\n"), Err(RespError::InvalidNull));
        assert_eq!(
            parse_frame(b"=3\r\nabc\r\n"),
            Err(RespError::InvalidVerbatim)
        );
        assert_eq!(
            parse_frame(b"=5\r\ntxt!a\r\n"),
            Err(RespError::InvalidVerbatim)
        );
        assert_eq!(parse_frame(b"(\r\n"), Err(RespError::InvalidBigNumber));
        assert_eq!(parse_frame(b"(12a\r\n"), Err(RespError::InvalidBigNumber));

        let mut args = Vec::new();
        assert_eq!(
            parse_command_into(b"SET \"unclosed\r\n", &mut args, tight),
            Err(RespError::UnclosedQuote)
        );
        assert_eq!(
            parse_command_into(b"PING 01234567890123456789\r\n", &mut args, tight),
            Err(RespError::MaxInlineLenExceeded)
        );
    }

    #[test]
    fn deterministic_fuzz_corpus_100k_inputs_zero_panics() {
        let mut state: u64 = 0xF022_2026_CAFE_BABE;
        let mut next_u64 = || {
            state = state.wrapping_add(0x9E37_79B9_7F4A_7C15);
            let mut z = state;
            z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
            z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
            z ^ (z >> 31)
        };
        let alphabet = b"+-:$*_#,(!=%~>0123456789-\r\n tf\"' \x00\xFF";
        let limits = ParseLimits {
            max_depth: 16,
            max_bulk_len: 1024,
            max_collection_len: 64,
            max_inline_len: 256,
        };
        let mut buf = Vec::with_capacity(64);
        let mut completed = 0usize;
        let mut cmd_completed = 0usize;

        for _ in 0..100_000 {
            buf.clear();
            let len = (next_u64() as usize) % 64;
            for _ in 0..len {
                let idx = (next_u64() as usize) % alphabet.len();
                buf.push(alphabet[idx]);
            }
            if let Ok(ParseStatus::Complete { .. }) = parse_frame_with_limits(&buf, limits) {
                completed += 1;
            }
            let mut args = Vec::new();
            if let Ok(CommandStatus::Complete { .. }) = parse_command_into(&buf, &mut args, limits)
            {
                cmd_completed += 1;
            }
        }
        assert!(
            completed > 0,
            "fuzz corpus should exercise complete frames too"
        );
        assert!(
            cmd_completed > 0,
            "fuzz corpus should exercise complete commands too"
        );
    }
}
