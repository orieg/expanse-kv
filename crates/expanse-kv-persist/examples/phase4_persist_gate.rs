//! Phase 4 persistence gate runner (`docs/PRD.md` §7 Phase 4).
//!
//! Verifies:
//! 1. Local AOF replay of 1,000,000 mixed commands completes with 100% CRC64
//!    state checksum match.
//! 2. Zero data loss under `FsyncPolicy::Always` across simulated `SIGKILL`
//!    truncation at every byte boundary of an in-flight tail command.
//! 3. CRC64 binary snapshot save/load roundtrip preserves 100% state checksum
//!    and rejects single-bit corruption.
//! 4. Atomic AOF rewrite compacts the log with 100% state checksum match.

use std::collections::{BTreeMap, BTreeSet};
use std::env;
use std::fs;
use std::path::PathBuf;
use std::time::Instant;

use expanse_kv_persist::aof::{
    AofWriter, FsyncPolicy, encode_aof_command, replay_aof, rewrite_aof_atomic,
};
use expanse_kv_persist::crc64::Crc64;
use expanse_kv_persist::snapshot::{SnapshotRecord, load_snapshot, save_snapshot_atomic};

/// Deterministic reference state used to validate AOF replay, rewrite, and snapshots.
#[derive(Default)]
struct RefState {
    strings: BTreeMap<Vec<u8>, Vec<u8>>,
    hashes: BTreeMap<Vec<u8>, BTreeMap<Vec<u8>, Vec<u8>>>,
    zsets: BTreeMap<Vec<u8>, BTreeMap<Vec<u8>, f64>>,
    bitmaps: BTreeMap<Vec<u8>, BTreeSet<u64>>,
}

impl RefState {
    fn apply_cmd(&mut self, args: &[&[u8]]) -> Result<(), String> {
        let Some(&cmd) = args.first() else {
            return Ok(());
        };
        match cmd {
            b"SET" if args.len() == 3 => {
                self.strings.insert(args[1].to_vec(), args[2].to_vec());
            }
            b"DEL" if args.len() == 2 => {
                self.strings.remove(args[1]);
                self.hashes.remove(args[1]);
                self.zsets.remove(args[1]);
                self.bitmaps.remove(args[1]);
            }
            b"HSET" if args.len() == 4 => {
                self.hashes
                    .entry(args[1].to_vec())
                    .or_default()
                    .insert(args[2].to_vec(), args[3].to_vec());
            }
            b"HDEL" if args.len() == 3 => {
                if let Some(h) = self.hashes.get_mut(args[1]) {
                    h.remove(args[2]);
                    if h.is_empty() {
                        self.hashes.remove(args[1]);
                    }
                }
            }
            b"ZADD" if args.len() == 4 => {
                let s = std::str::from_utf8(args[2]).map_err(|e| e.to_string())?;
                let score: f64 = s
                    .parse()
                    .map_err(|e: std::num::ParseFloatError| e.to_string())?;
                self.zsets
                    .entry(args[1].to_vec())
                    .or_default()
                    .insert(args[3].to_vec(), score);
            }
            b"SETBIT" if args.len() == 4 => {
                let off_str = std::str::from_utf8(args[2]).map_err(|e| e.to_string())?;
                let off: u64 = off_str
                    .parse()
                    .map_err(|e: std::num::ParseIntError| e.to_string())?;
                let bit = args[3] == b"1";
                if bit {
                    self.bitmaps
                        .entry(args[1].to_vec())
                        .or_default()
                        .insert(off);
                } else if let Some(entry) = self.bitmaps.get_mut(args[1]) {
                    entry.remove(&off);
                    if entry.is_empty() {
                        self.bitmaps.remove(args[1]);
                    }
                }
            }
            _ => {
                return Err(format!(
                    "unexpected command in gate stream: {:?}",
                    String::from_utf8_lossy(cmd)
                ));
            }
        }
        Ok(())
    }

    fn apply_record(&mut self, rec: SnapshotRecord) {
        match rec {
            SnapshotRecord::String { key, value, .. } => {
                self.strings.insert(key, value);
            }
            SnapshotRecord::Hash { key, fields, .. } => {
                let mut m = BTreeMap::new();
                for (f, v) in fields {
                    m.insert(f, v);
                }
                self.hashes.insert(key, m);
            }
            SnapshotRecord::ZSet { key, entries, .. } => {
                let mut m = BTreeMap::new();
                for (score, member) in entries {
                    m.insert(member, score);
                }
                self.zsets.insert(key, m);
            }
            SnapshotRecord::Bitmap { key, bits, .. } => {
                let s: BTreeSet<u64> = bits.into_iter().collect();
                self.bitmaps.insert(key, s);
            }
        }
    }

    fn checksum(&self) -> u64 {
        let mut h = Crc64::new();
        h.update(b"STR");
        for (k, v) in &self.strings {
            h.update(&(k.len() as u64).to_be_bytes());
            h.update(k);
            h.update(&(v.len() as u64).to_be_bytes());
            h.update(v);
        }
        h.update(b"HASH");
        for (k, fields) in &self.hashes {
            h.update(&(k.len() as u64).to_be_bytes());
            h.update(k);
            for (f, v) in fields {
                h.update(&(f.len() as u64).to_be_bytes());
                h.update(f);
                h.update(&(v.len() as u64).to_be_bytes());
                h.update(v);
            }
        }
        h.update(b"ZSET");
        for (k, members) in &self.zsets {
            h.update(&(k.len() as u64).to_be_bytes());
            h.update(k);
            for (m, &score) in members {
                h.update(&score.to_bits().to_be_bytes());
                h.update(&(m.len() as u64).to_be_bytes());
                h.update(m);
            }
        }
        h.update(b"BMP");
        for (k, bits) in &self.bitmaps {
            h.update(&(k.len() as u64).to_be_bytes());
            h.update(k);
            for &b in bits {
                h.update(&b.to_be_bytes());
            }
        }
        h.finalize()
    }
}

struct XorShift64(u64);

impl XorShift64 {
    fn next(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        self.0 = x;
        x
    }
}

fn main() {
    let args: Vec<String> = env::args().collect();
    let mut n_cmds: u64 = 1_000_000;
    let mut i = 1;
    while i < args.len() {
        if args[i] == "--commands" && i + 1 < args.len() {
            n_cmds = args[i + 1].parse().expect("invalid --commands");
            i += 2;
        } else {
            i += 1;
        }
    }

    let tmp_dir: PathBuf = env::temp_dir().join(format!("expanse-kv-p4-{}", std::process::id()));
    fs::create_dir_all(&tmp_dir).expect("create temp dir");
    let aof_path = tmp_dir.join("appendonly.aof");
    let snap_path = tmp_dir.join("dump.ekvs");
    let sigkill_path = tmp_dir.join("sigkill.aof");

    // 1. Write 1,000,000 mixed commands to AOF + Reference state
    let mut rng = XorShift64(0x0004_A0F1_2026_0001);
    let mut expected = RefState::default();
    let t_write = Instant::now();
    {
        let mut writer = AofWriter::open(&aof_path, FsyncPolicy::No).expect("open AOF");
        for idx in 0..n_cmds {
            let r = rng.next();
            let op = r % 10;
            let key_id = (r >> 8) % 20_000;
            match op {
                0..=4 => {
                    let k = format!("org:user:session:{key_id:06}");
                    let v = format!("val-{idx}");
                    let cmd = [b"SET".as_slice(), k.as_bytes(), v.as_bytes()];
                    expected.apply_cmd(&cmd).expect("apply SET");
                    writer.append(&cmd).expect("append SET");
                }
                5 => {
                    let k = format!("org:user:session:{key_id:06}");
                    let cmd = [b"DEL".as_slice(), k.as_bytes()];
                    expected.apply_cmd(&cmd).expect("apply DEL");
                    writer.append(&cmd).expect("append DEL");
                }
                6..=7 => {
                    let k = format!("hash:{:04}", key_id % 1000);
                    let f = format!("f{:02}", (r >> 24) % 32);
                    let v = format!("hv-{idx}");
                    let cmd = [b"HSET".as_slice(), k.as_bytes(), f.as_bytes(), v.as_bytes()];
                    expected.apply_cmd(&cmd).expect("apply HSET");
                    writer.append(&cmd).expect("append HSET");
                }
                8 => {
                    let k = format!("zset:{:03}", key_id % 200);
                    let m = format!("m{:03}", (r >> 24) % 200);
                    let score = format!("{:.2}", ((r >> 32) % 10_000) as f64 / 10.0);
                    let cmd = [
                        b"ZADD".as_slice(),
                        k.as_bytes(),
                        score.as_bytes(),
                        m.as_bytes(),
                    ];
                    expected.apply_cmd(&cmd).expect("apply ZADD");
                    writer.append(&cmd).expect("append ZADD");
                }
                _ => {
                    let k = format!("bmp:{:03}", key_id % 100);
                    let off = format!("{}", (r >> 24) % 65_536);
                    let bit = if (r & 1) == 0 {
                        b"1".as_slice()
                    } else {
                        b"0".as_slice()
                    };
                    let cmd = [b"SETBIT".as_slice(), k.as_bytes(), off.as_bytes(), bit];
                    expected.apply_cmd(&cmd).expect("apply SETBIT");
                    writer.append(&cmd).expect("append SETBIT");
                }
            }
        }
        writer.flush_and_sync().expect("flush_and_sync");
    }
    let write_ms = t_write.elapsed().as_secs_f64() * 1000.0;
    let aof_bytes = fs::metadata(&aof_path).expect("aof metadata").len();
    let expected_crc = expected.checksum();

    // 2. Replay 1,000,000 commands and verify 100% state checksum match
    let t_replay = Instant::now();
    let mut replayed = RefState::default();
    let replay_stats =
        replay_aof(&aof_path, false, |cmd| replayed.apply_cmd(cmd)).expect("replay AOF");
    let replay_ms = t_replay.elapsed().as_secs_f64() * 1000.0;
    let replayed_crc = replayed.checksum();
    assert_eq!(replay_stats.commands_replayed, n_cmds);
    assert_eq!(
        replayed_crc, expected_crc,
        "AOF replay state checksum mismatch"
    );

    // 3. Simulated SIGKILL under FsyncPolicy::Always across every tail byte boundary
    let fsync_always_committed = 200u64;
    let mut sigkill_expected = RefState::default();
    let committed_bytes: u64;
    {
        let mut w = AofWriter::open(&sigkill_path, FsyncPolicy::Always).expect("open sigkill AOF");
        for i in 0..fsync_always_committed {
            let k = format!("durability:k:{i:04}");
            let v = format!("committed-{i}");
            let cmd = [b"SET".as_slice(), k.as_bytes(), v.as_bytes()];
            sigkill_expected.apply_cmd(&cmd).expect("apply");
            w.append(&cmd).expect("append fsync=always");
        }
        committed_bytes = w.bytes_appended();
    }
    let sigkill_expected_crc = sigkill_expected.checksum();
    let base_bytes = fs::read(&sigkill_path).expect("read sigkill base");

    let mut inflight = Vec::new();
    encode_aof_command(
        &[
            b"SET".as_slice(),
            b"durability:uncommitted".as_slice(),
            b"partial-write-interrupted".as_slice(),
        ],
        &mut inflight,
    );
    let mut sigkill_cuts_tested = 0u64;
    let mut sigkill_losses = 0u64;
    for cut in 1..inflight.len() {
        let cut_file = tmp_dir.join(format!("sigkill_cut_{cut}.aof"));
        let mut combined = base_bytes.clone();
        combined.extend_from_slice(&inflight[..cut]);
        fs::write(&cut_file, &combined).expect("write cut file");

        let mut recovered = RefState::default();
        let st = replay_aof(&cut_file, true, |cmd| recovered.apply_cmd(cmd))
            .expect("replay sigkill cut");
        if st.commands_replayed != fsync_always_committed
            || st.bytes_consumed != committed_bytes
            || recovered.checksum() != sigkill_expected_crc
        {
            sigkill_losses += 1;
        }
        sigkill_cuts_tested += 1;
        fs::remove_file(&cut_file).expect("rm cut file");
    }
    assert_eq!(sigkill_losses, 0, "SIGKILL durability loss detected");

    // 4. CRC64 binary snapshot save/load roundtrip + single-bit corruption rejection
    let t_snap = Instant::now();
    let snap_save_stats = save_snapshot_atomic(&snap_path, |w| {
        for (k, v) in &expected.strings {
            w.write_record(&SnapshotRecord::String {
                key: k.clone(),
                value: v.clone(),
                expiry_ms: None,
            })?;
        }
        for (k, fields) in &expected.hashes {
            w.write_record(&SnapshotRecord::Hash {
                key: k.clone(),
                fields: fields.iter().map(|(f, v)| (f.clone(), v.clone())).collect(),
                expiry_ms: None,
            })?;
        }
        for (k, members) in &expected.zsets {
            w.write_record(&SnapshotRecord::ZSet {
                key: k.clone(),
                entries: members.iter().map(|(m, &s)| (s, m.clone())).collect(),
                expiry_ms: None,
            })?;
        }
        for (k, bits) in &expected.bitmaps {
            w.write_record(&SnapshotRecord::Bitmap {
                key: k.clone(),
                bits: bits.iter().copied().collect(),
                expiry_ms: None,
            })?;
        }
        Ok(())
    })
    .expect("save snapshot");
    let snap_save_ms = t_snap.elapsed().as_secs_f64() * 1000.0;

    let mut snap_loaded = RefState::default();
    let snap_load_stats = load_snapshot(&snap_path, |rec| {
        snap_loaded.apply_record(rec);
        Ok(())
    })
    .expect("load snapshot");
    assert_eq!(snap_load_stats, snap_save_stats);
    assert_eq!(snap_loaded.checksum(), expected_crc);

    // Corruption test: 64 distributed single-bit flips must all fail CRC64/format checks
    let raw_snap = fs::read(&snap_path).expect("read snapshot");
    let step = (raw_snap.len() / 64).max(1);
    let mut corruption_flips_tested = 0u64;
    let mut corruption_accepted = 0u64;
    let mut pos = 0usize;
    while pos < raw_snap.len() && corruption_flips_tested < 64 {
        let mut mutated = raw_snap.clone();
        mutated[pos] ^= 1u8 << (pos % 8);
        let res =
            expanse_kv_persist::snapshot::load_snapshot_from_reader(mutated.as_slice(), |_| Ok(()));
        if res.is_ok() {
            corruption_accepted += 1;
        }
        corruption_flips_tested += 1;
        pos += step;
    }
    assert_eq!(corruption_accepted, 0, "corrupted snapshot was accepted");

    // 5. Atomic AOF rewrite compaction + replay checksum verification
    let rewritten_cmds = rewrite_aof_atomic(&aof_path, |sink| {
        for (k, v) in &expected.strings {
            sink.write_command(&[b"SET", k, v])?;
        }
        for (k, fields) in &expected.hashes {
            for (f, v) in fields {
                sink.write_command(&[b"HSET", k, f, v])?;
            }
        }
        for (k, members) in &expected.zsets {
            for (m, &score) in members {
                let s = format!("{score}");
                sink.write_command(&[b"ZADD", k, s.as_bytes(), m])?;
            }
        }
        for (k, bits) in &expected.bitmaps {
            for &bit in bits {
                let off = format!("{bit}");
                sink.write_command(&[b"SETBIT", k, off.as_bytes(), b"1"])?;
            }
        }
        Ok(())
    })
    .expect("rewrite AOF");
    let rewritten_bytes = fs::metadata(&aof_path).expect("rewritten metadata").len();
    let mut rewritten_state = RefState::default();
    let rewritten_replay = replay_aof(&aof_path, false, |cmd| rewritten_state.apply_cmd(cmd))
        .expect("replay rewritten AOF");
    assert_eq!(rewritten_replay.commands_replayed, rewritten_cmds);
    assert_eq!(rewritten_state.checksum(), expected_crc);

    fs::remove_dir_all(&tmp_dir).expect("cleanup temp dir");

    println!(
        "{{\n  \"commands_replayed\": {},\n  \"aof_bytes\": {},\n  \"write_ms\": {:.2},\n  \"replay_ms\": {:.2},\n  \"state_crc64\": \"0x{:016x}\",\n  \"checksum_match\": true,\n  \"sigkill_fsync_always_committed\": {},\n  \"sigkill_cuts_tested\": {},\n  \"sigkill_losses\": {},\n  \"snapshot_records\": {},\n  \"snapshot_bytes\": {},\n  \"snapshot_save_ms\": {:.2},\n  \"snapshot_crc64\": \"0x{:016x}\",\n  \"snapshot_corruption_flips_tested\": {},\n  \"snapshot_corruption_accepted\": {},\n  \"aof_rewrite_commands\": {},\n  \"aof_rewrite_bytes\": {},\n  \"verdict\": \"PASS\"\n}}",
        replay_stats.commands_replayed,
        aof_bytes,
        write_ms,
        replay_ms,
        expected_crc,
        fsync_always_committed,
        sigkill_cuts_tested,
        sigkill_losses,
        snap_load_stats.records,
        snap_load_stats.bytes,
        snap_save_ms,
        snap_load_stats.crc64,
        corruption_flips_tested,
        corruption_accepted,
        rewritten_cmds,
        rewritten_bytes,
    );
}
