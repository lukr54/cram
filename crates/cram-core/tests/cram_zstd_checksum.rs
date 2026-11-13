//! A flipped bit inside a zstd pack of a `.cram` must be reported, not handed back as content.
//!
//! zstd packs used to be written without the frame's content checksum, and the index stores no hash
//! of a pack, so a flip that still decoded to the right length passed `cram t` and extracted wrong
//! bytes (314 of the 619 one-bit-per-byte flips of the fixture below, with the reader's check taken
//! out). The writer now sets the checksum and both readers verify it.
//!
//! The fixture below is a `.cram` written by a `zstd-c` build after that change, embedded as bytes so
//! the default-feature build, which has no zstd encoder, still tests the reader against a real
//! checksummed frame. `print_fixture` regenerates it.

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use cram_core::engine::verify;
use cram_core::format::{Codec, Format};
use cram_core::formats;
use cram_core::progress::NullSink;
use cram_core::secret::NoPassword;

/// zstd frame magic, little-endian on disk.
const ZSTD_MAGIC: &[u8] = &[0x28, 0xB5, 0x2F, 0xFD];
/// Frame header descriptor bit 2: the frame ends in a 4-byte content checksum.
const CHECKSUM_FLAG: u8 = 0x04;
/// `.cram` pack codec id for zstd.
const CODEC_ZSTD: u8 = 2;
/// `.cram` trailer: index_offset(8) + index_len(8) + magic(6).
const TRAILER_LEN: usize = 22;

/// A fresh directory under cargo's per-target scratch folder, so the test writes nothing outside
/// the build tree.
fn scratch(tag: &str) -> PathBuf {
    let dir = Path::new(env!("CARGO_TARGET_TMPDIR")).join(format!(
        "cram-zck-{}-{}",
        std::process::id(),
        tag
    ));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).unwrap();
    dir
}

/// The fixture's two files, as (archive path, bytes). Varied enough that the pack is mostly
/// compressed block data rather than repeats.
fn fixture_files() -> Vec<(&'static str, Vec<u8>)> {
    let mut a = String::new();
    for i in 0..60u32 {
        a.push_str(&format!(
            "line {i}: pack {} holds chunk {} at offset {}\n",
            i.wrapping_mul(7_919) % 97,
            i.wrapping_mul(31) % 53,
            i.wrapping_mul(2_654_435_761) >> 20
        ));
    }
    let mut b = String::new();
    for i in 0..40u32 {
        b.push_str(&format!("{} {}\n", i * i, (i * 37) % 11));
    }
    vec![("fx/a.txt", a.into_bytes()), ("fx/b.txt", b.into_bytes())]
}

/// `.cram` written by `cram a` on a `zstd-c` build of this tree from [`fixture_files`] (default level,
/// one zstd pack of 619 bytes at offset 8, 3,010 bytes raw, content checksum set). 795 bytes.
const FIXTURE: &[u8] = &[
    0x43, 0x52, 0x41, 0x4d, 0x1b, 0x01, 0x01, 0x00, 0x28, 0xb5, 0x2f, 0xfd, 0x64, 0xc2, 0x0a, 0xed,
    0x12, 0x00, 0x06, 0xaf, 0x61, 0x14, 0xa0, 0x9b, 0x1b, 0x76, 0x9b, 0xe7, 0x7f, 0x26, 0xa8, 0x06,
    0x6c, 0x77, 0xf7, 0xa6, 0x9d, 0x3c, 0xf8, 0xa1, 0x15, 0x0a, 0x68, 0x00, 0x55, 0x00, 0x57, 0x00,
    0x53, 0xc7, 0x4b, 0x32, 0xca, 0xda, 0x86, 0xd3, 0xf6, 0x57, 0xe5, 0x33, 0x4f, 0x9d, 0xef, 0x6b,
    0x1a, 0xa2, 0x9a, 0x6b, 0xcb, 0x3c, 0x67, 0x22, 0x53, 0x91, 0xae, 0x8e, 0x34, 0xa5, 0x21, 0xbf,
    0x74, 0x79, 0x86, 0x64, 0xd4, 0xa2, 0x11, 0xb5, 0xec, 0x37, 0x93, 0x3e, 0x17, 0x2d, 0x8d, 0xf2,
    0xd9, 0x22, 0xca, 0x62, 0x5f, 0xc7, 0x3c, 0xcd, 0x13, 0xc9, 0x2c, 0xf3, 0xec, 0x52, 0xb9, 0x64,
    0xab, 0x10, 0x28, 0x1c, 0x24, 0xc4, 0x88, 0x03, 0x41, 0x43, 0x41, 0xc1, 0x82, 0x38, 0x04, 0xc8,
    0x08, 0x82, 0xc2, 0x83, 0x81, 0x80, 0x34, 0x0c, 0x24, 0x2c, 0x18, 0xc8, 0x08, 0x82, 0x40, 0x00,
    0x83, 0x00, 0x8c, 0x10, 0x28, 0x1c, 0x24, 0x04, 0xf7, 0xe9, 0x93, 0x79, 0x98, 0x75, 0xd1, 0xe8,
    0x2a, 0x3a, 0x91, 0xdd, 0x78, 0x4a, 0xb5, 0xc2, 0x72, 0xd9, 0xa8, 0xa8, 0xee, 0xbd, 0x63, 0xad,
    0xd6, 0x24, 0x63, 0x09, 0xe9, 0x77, 0xcb, 0x2e, 0xaf, 0x56, 0x3b, 0x7d, 0x1d, 0x1a, 0x3c, 0x6a,
    0xb9, 0x32, 0xef, 0x8e, 0x0f, 0x91, 0xea, 0x97, 0xcf, 0xe8, 0x76, 0xcc, 0xec, 0x58, 0xa2, 0xca,
    0x6a, 0x4f, 0x26, 0x7a, 0x1e, 0xde, 0x68, 0xf4, 0xdf, 0x76, 0x33, 0xce, 0xbc, 0xda, 0xf7, 0xee,
    0x3d, 0xb2, 0x1b, 0xf7, 0x43, 0xf6, 0xe6, 0xb4, 0xd0, 0x79, 0xfc, 0x8d, 0x03, 0x72, 0x0b, 0x93,
    0x7c, 0x58, 0x8c, 0x22, 0xa5, 0x1c, 0x25, 0xd6, 0xa8, 0x64, 0xa0, 0x3c, 0x2e, 0x8d, 0x91, 0x69,
    0x5a, 0x96, 0xfd, 0xb0, 0xfc, 0x2b, 0xdf, 0x99, 0x6d, 0x5e, 0xad, 0xea, 0xd4, 0xab, 0x72, 0xa8,
    0x90, 0xca, 0xca, 0x26, 0x1b, 0x5d, 0xff, 0x58, 0xa5, 0x34, 0xa2, 0x52, 0x4e, 0x59, 0x5b, 0x54,
    0x4a, 0xf3, 0xf9, 0x66, 0xf8, 0xac, 0xf9, 0x97, 0x75, 0xc5, 0x27, 0xd9, 0x4d, 0x74, 0xfd, 0x95,
    0xf3, 0x92, 0xdc, 0x5f, 0x91, 0xf1, 0x4e, 0x75, 0xd2, 0x90, 0xd1, 0x93, 0x54, 0xd7, 0xb8, 0xaf,
    0x74, 0x5d, 0x35, 0x01, 0x61, 0xa9, 0xab, 0x91, 0xf2, 0xbf, 0x51, 0x92, 0x46, 0x8d, 0x8a, 0xa5,
    0x1b, 0x28, 0x65, 0x37, 0x2e, 0x56, 0x6d, 0x8c, 0x24, 0xce, 0x6d, 0x19, 0x53, 0x99, 0xc9, 0x30,
    0xe9, 0x31, 0x2c, 0xc7, 0x8b, 0x94, 0xf4, 0x45, 0x89, 0x9d, 0x45, 0xa5, 0x6c, 0x81, 0x62, 0x57,
    0x71, 0x49, 0x57, 0x8c, 0x1c, 0x1f, 0x27, 0xfd, 0x83, 0x64, 0xe6, 0x31, 0x95, 0x1a, 0x85, 0xc9,
    0x4d, 0x61, 0x49, 0x74, 0xa4, 0x58, 0x39, 0x4a, 0xca, 0x8e, 0x8a, 0x65, 0x03, 0x25, 0xad, 0x71,
    0xf9, 0x1b, 0x23, 0x75, 0xe3, 0x64, 0xa6, 0x41, 0x52, 0x8c, 0x69, 0x80, 0xe7, 0xa8, 0x21, 0x48,
    0x30, 0x75, 0xff, 0x6b, 0x0d, 0x40, 0x33, 0x4e, 0x5a, 0x03, 0x11, 0x20, 0x04, 0x14, 0x5c, 0x40,
    0x04, 0x5e, 0xf0, 0xfa, 0xd3, 0x01, 0x8a, 0x0d, 0x46, 0x2c, 0x31, 0x08, 0x02, 0x3d, 0xd3, 0xd0,
    0xf5, 0x08, 0x3c, 0x3a, 0x44, 0x04, 0x75, 0x26, 0x95, 0x6b, 0x0b, 0x88, 0x33, 0x11, 0x19, 0x1b,
    0x81, 0x66, 0x9a, 0x86, 0x07, 0x94, 0x99, 0x54, 0x43, 0xac, 0x95, 0xb9, 0x00, 0xbb, 0xfc, 0xfc,
    0x88, 0xea, 0x8d, 0x14, 0x19, 0xf9, 0xc8, 0x55, 0xd9, 0xe4, 0x87, 0xb3, 0xf7, 0xb7, 0x96, 0x89,
    0xd3, 0x45, 0xba, 0x8e, 0xa6, 0x4e, 0xb4, 0xe5, 0x6b, 0xf9, 0x93, 0x77, 0x23, 0x5d, 0x1d, 0x99,
    0xd0, 0xfa, 0xf5, 0xfc, 0x88, 0xea, 0xf3, 0x51, 0xfa, 0x54, 0x3e, 0xb5, 0xc9, 0xeb, 0x77, 0x89,
    0x1b, 0xd2, 0xf1, 0x7f, 0x95, 0xcd, 0x7a, 0xf5, 0xce, 0xbb, 0x42, 0x90, 0x11, 0x2a, 0xef, 0xa6,
    0x27, 0x47, 0xe8, 0x18, 0x9b, 0x1c, 0x77, 0x24, 0x41, 0xa6, 0x54, 0xf5, 0xa1, 0xf4, 0xdf, 0x2a,
    0x86, 0x26, 0x63, 0xb5, 0xb7, 0x41, 0xa6, 0xbe, 0x7c, 0x66, 0xe9, 0x8c, 0xd2, 0x19, 0x55, 0x99,
    0x28, 0x8a, 0x4f, 0xa5, 0xf1, 0x95, 0x86, 0x32, 0x64, 0xe3, 0xe3, 0x7a, 0xc9, 0x22, 0x13, 0xbd,
    0x2a, 0x6c, 0x95, 0x87, 0x19, 0x85, 0x50, 0xe5, 0x4d, 0x7d, 0xbb, 0xba, 0x9a, 0x25, 0x4f, 0xe2,
    0xb3, 0xf3, 0x4c, 0xa1, 0x5b, 0xab, 0xc7, 0x8e, 0xeb, 0xb4, 0xfe, 0x9c, 0xc9, 0x4d, 0x0d, 0x50,
    0x85, 0x92, 0xcb, 0x01, 0x00, 0x00, 0x00, 0x08, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x6b,
    0x02, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0xc2, 0x0b, 0x00, 0x00, 0x02, 0x02, 0x00, 0x00, 0x00,
    0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0xd5, 0x0a, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
    0xd5, 0x0a, 0x00, 0x00, 0xed, 0x00, 0x00, 0x00, 0x03, 0x00, 0x00, 0x00, 0x01, 0x02, 0x00, 0x00,
    0x00, 0x66, 0x78, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
    0x00, 0x00, 0x00, 0x00, 0x08, 0x00, 0x00, 0x00, 0x66, 0x78, 0x2f, 0x61, 0x2e, 0x74, 0x78, 0x74,
    0xd5, 0x0a, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00,
    0x00, 0x00, 0x00, 0x00, 0x00, 0x08, 0x00, 0x00, 0x00, 0x66, 0x78, 0x2f, 0x62, 0x2e, 0x74, 0x78,
    0x74, 0xed, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x01, 0x00, 0x00,
    0x00, 0x01, 0x00, 0x00, 0x00, 0x73, 0x02, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x92, 0x00, 0x00,
    0x00, 0x00, 0x00, 0x00, 0x00, 0x43, 0x52, 0x41, 0x4d, 0x1b, 0x01,
];

fn u32_at(b: &[u8], at: usize) -> u32 {
    u32::from_le_bytes(b[at..at + 4].try_into().unwrap())
}

fn u64_at(b: &[u8], at: usize) -> u64 {
    u64::from_le_bytes(b[at..at + 8].try_into().unwrap())
}

/// One pack of an unencrypted `.cram`, read straight from its index (spec §6).
struct Pack {
    /// Byte offset of the index record's `comp_len` field, for rewriting it.
    record: usize,
    offset: usize,
    len: usize,
    codec: u8,
}

/// Every pack of an unencrypted `.cram`, with where its index begins.
fn packs(arc: &[u8]) -> (usize, Vec<Pack>) {
    let t = arc.len() - TRAILER_LEN;
    let index = u64_at(arc, t) as usize;
    let count = u32_at(arc, index) as usize;
    let packs = (0..count)
        .map(|i| {
            let r = index + 4 + 21 * i;
            Pack {
                record: r + 8,
                offset: u64_at(arc, r) as usize,
                len: u64_at(arc, r + 8) as usize,
                codec: arc[r + 20],
            }
        })
        .collect();
    (index, packs)
}

/// The one zstd pack of `arc`, after checking it is a frame that carries a content checksum.
fn zstd_pack(arc: &[u8]) -> Pack {
    let (_, mut all) = packs(arc);
    assert_eq!(all.len(), 1, "the archive must hold exactly one pack");
    let p = all.remove(0);
    assert_eq!(p.codec, CODEC_ZSTD, "the pack must be zstd");
    let frame = &arc[p.offset..p.offset + p.len];
    assert_eq!(
        &frame[..4],
        ZSTD_MAGIC,
        "the pack must begin with a zstd frame"
    );
    assert!(
        frame[4] & CHECKSUM_FLAG != 0,
        "the zstd frame must carry a content checksum (descriptor {:#04x})",
        frame[4]
    );
    p
}

/// Length of a zstd frame header (RFC 8878 §3.1.1.1): magic, descriptor, then an optional window
/// descriptor, dictionary id and content size, each sized by the descriptor.
fn frame_header_len(frame: &[u8]) -> usize {
    let d = frame[4];
    let single_segment = d & 0x20 != 0;
    let did = [0, 1, 2, 4][(d & 0x03) as usize];
    let fcs = match d >> 6 {
        0 => usize::from(single_segment),
        1 => 2,
        2 => 4,
        _ => 8,
    };
    5 + usize::from(!single_segment) + did + fcs
}

/// What the readers make of an archive.
enum Outcome {
    /// Every file entry came back through `copy_entry` and `verify` passed.
    Read(Vec<Vec<u8>>),
    /// At least one of the two reported a problem; the strings say which and how.
    Refused(Vec<String>),
}

/// Write `arc` to `path`, run `engine::verify` over it, and `copy_entry` every file entry.
///
/// Both are asked separately because they are separate claims: `cram t` is `verify`, and extraction
/// and conversion go through `copy_entry`. An archive counts as read only when both accept it.
fn read_both(arc: &[u8], path: &Path) -> (Outcome, Outcome) {
    fs::write(path, arc).unwrap();
    let by_verify = match verify::verify(path, Arc::new(NoPassword), &NullSink) {
        Ok(rep) if rep.ok() => Outcome::Read(Vec::new()),
        Ok(rep) => Outcome::Refused(
            rep.failures
                .iter()
                .map(|(n, w)| format!("{n}: {w}"))
                .collect(),
        ),
        Err(e) => Outcome::Refused(vec![format!("verify: {e}")]),
    };
    let by_copy = match formats::open(path, Format::cram(Codec::None), Arc::new(NoPassword)) {
        Err(e) => Outcome::Refused(vec![format!("open: {e}")]),
        Ok(reader) => {
            let ra = reader.as_random_access().expect("cram is random-access");
            let mut bodies = Vec::new();
            let mut errors = Vec::new();
            for (i, e) in ra.entries().iter().enumerate() {
                if e.is_dir() {
                    continue;
                }
                let mut body = Vec::new();
                match ra.copy_entry(i, &mut body) {
                    Ok(_) => bodies.push(body),
                    Err(err) => errors.push(format!("{}: {err}", e.name())),
                }
            }
            if errors.is_empty() {
                Outcome::Read(bodies)
            } else {
                Outcome::Refused(errors)
            }
        }
    };
    (by_verify, by_copy)
}

fn expected_bodies() -> Vec<Vec<u8>> {
    fixture_files().into_iter().map(|(_, b)| b).collect()
}

/// What one bit flip at each byte of a zstd pack did.
struct Flips {
    /// Pack-relative byte positions whose flip both `verify` and `copy_entry` refused.
    refused: Vec<usize>,
    /// Positions whose flip both accepted and `copy_entry` returned the original bytes: the flipped
    /// frame decodes to the same content, so no content checksum can tell it apart.
    neutral: Vec<usize>,
    /// Length of the frame header, so a caller can tell header positions from block data.
    header_len: usize,
}

/// Flip one bit of every byte of `arc`'s zstd pack in turn, cycling through the eight bit
/// positions, and run both readers over each result.
///
/// Panics if any flip is accepted with wrong content (the defect this file exists for), or if the
/// two readers disagree. A flip both accept with the original content is returned in `neutral`
/// rather than failed: the checksum covers content, and some bits of a frame do not change it
/// (the unused descriptor bit, low bits of the declared content size, which the decoder only
/// uses as a window size, and a few bits inside compressed blocks). Callers decide which neutral
/// positions they accept.
fn flip_every_byte(arc: &[u8], tag: &str) -> Flips {
    let dir = scratch(tag);
    let p = zstd_pack(arc);
    let header_len = frame_header_len(&arc[p.offset..p.offset + p.len]);
    let want = expected_bodies();
    let path = dir.join("flip.cram");

    let mut out = Flips {
        refused: Vec::new(),
        neutral: Vec::new(),
        header_len,
    };
    let mut bad = Vec::new();
    for k in 0..p.len {
        let mut b = arc.to_vec();
        let bit = 1u8 << (k % 8);
        b[p.offset + k] ^= bit;
        match read_both(&b, &path) {
            (Outcome::Refused(_), Outcome::Refused(_)) => out.refused.push(k),
            (Outcome::Read(_), Outcome::Read(bodies)) if bodies == want => out.neutral.push(k),
            (v, c) => {
                let say = |o: &Outcome| match o {
                    Outcome::Refused(_) => "refused",
                    Outcome::Read(bodies) if *bodies == want => "accepted, same content",
                    Outcome::Read(_) => "accepted WRONG content",
                };
                let verify_said = match v {
                    Outcome::Refused(_) => "refused",
                    Outcome::Read(_) => "accepted",
                };
                bad.push(format!(
                    "pack byte {k} bit {bit:#04x}: verify {verify_said}, copy_entry {}",
                    say(&c)
                ));
            }
        }
    }
    let _ = fs::remove_dir_all(&dir);
    assert!(
        bad.is_empty(),
        "{} of {} single-bit flips in a checksummed zstd pack were not reported:\n{}",
        bad.len(),
        p.len,
        bad.join("\n")
    );
    out
}

/// The checksummed fixture reads back exactly, through both paths.
#[test]
fn checksummed_zstd_fixture_reads() {
    let dir = scratch("clean");
    let _ = zstd_pack(FIXTURE);
    let (v, c) = read_both(FIXTURE, &dir.join("clean.cram"));
    let _ = fs::remove_dir_all(&dir);
    match v {
        Outcome::Read(_) => {}
        Outcome::Refused(why) => panic!("verify refused the clean fixture: {why:?}"),
    }
    match c {
        Outcome::Read(bodies) => assert!(
            bodies == expected_bodies(),
            "the clean fixture read back different content"
        ),
        Outcome::Refused(why) => panic!("copy_entry refused the clean fixture: {why:?}"),
    }
}

/// Pack-relative positions in [`FIXTURE`] whose flip (bit `k % 8`) decodes to the original
/// content, found by running [`flip_every_byte`] over it. Pinned so that any change in which
/// flips get through shows up here. 4 is the unused descriptor bit (0x10); 5 and 6 are bits of
/// the declared content size, which the decoder does not compare with what it decodes (the
/// pack's index `raw_len` is compared instead); 412 to 432 sit in compressed block data.
const FIXTURE_NEUTRAL: &[usize] = &[4, 5, 6, 412, 428, 429, 432];

/// One bit flipped at each of the fixture's 619 pack bytes: none returns wrong content, and every
/// flip outside the pinned content-neutral positions is reported by `verify` and by `copy_entry`.
#[test]
fn every_bit_flip_in_a_checksummed_zstd_pack_is_reported() {
    let flips = flip_every_byte(FIXTURE, "fixture");
    assert_eq!(
        flips.neutral, FIXTURE_NEUTRAL,
        "the flips that decode to unchanged content moved"
    );
    let body = flips
        .refused
        .iter()
        .filter(|&&k| k >= flips.header_len)
        .count();
    assert!(body >= 32, "only {body} block-data flips were reported");
}

/// The two header bits that decide whether there is anything to check. A flipped magic fails to
/// decode. A cleared checksum flag makes the decoder stop before the stored checksum, which then
/// sits after the frame as four unread bytes: that has to be refused, or one flip could switch
/// verification off.
#[test]
fn flipped_magic_or_checksum_flag_is_reported() {
    let dir = scratch("header");
    let p = zstd_pack(FIXTURE);
    for (at, bit, what) in [
        (0usize, 0x01u8, "magic"),
        (4, CHECKSUM_FLAG, "checksum flag"),
    ] {
        let mut b = FIXTURE.to_vec();
        b[p.offset + at] ^= bit;
        let (v, c) = read_both(&b, &dir.join("h.cram"));
        assert!(
            matches!(v, Outcome::Refused(_)),
            "verify accepted a flipped {what}"
        );
        assert!(
            matches!(c, Outcome::Refused(_)),
            "copy_entry accepted a flipped {what}"
        );
    }
    let _ = fs::remove_dir_all(&dir);
}

/// The fixture as a writer without the checksum would have made it: the same frame with the
/// checksum flag clear and its last four bytes gone, and the index and trailer moved to match.
/// Content checksums do not change the compressed blocks, so this is the frame zstd emits with the
/// checksum off.
fn without_checksum(arc: &[u8]) -> Vec<u8> {
    let p = zstd_pack(arc);
    let (index, _) = packs(arc);
    assert_eq!(
        p.offset + p.len,
        index,
        "the index must follow the pack directly"
    );
    let mut out = arc[..p.offset + p.len - 4].to_vec();
    out[p.offset + 4] &= !CHECKSUM_FLAG;
    let new_index = out.len();
    let mut idx = arc[index..arc.len() - TRAILER_LEN].to_vec();
    let rec = p.record - index;
    idx[rec..rec + 8].copy_from_slice(&((p.len - 4) as u64).to_le_bytes());
    out.extend_from_slice(&idx);
    out.extend_from_slice(&(new_index as u64).to_le_bytes());
    out.extend_from_slice(&(idx.len() as u64).to_le_bytes());
    out.extend_from_slice(&arc[arc.len() - 6..]);
    out
}

/// Control: a zstd frame without a checksum, which is every zstd pack written before this change,
/// still reads, with the same content, through both paths.
#[test]
fn zstd_pack_without_checksum_still_reads() {
    let dir = scratch("nochk");
    let old = without_checksum(FIXTURE);
    let (_, all) = packs(&old);
    assert_eq!(all[0].codec, CODEC_ZSTD);
    assert_eq!(
        old[all[0].offset + 4] & CHECKSUM_FLAG,
        0,
        "the control frame must carry no checksum"
    );
    let (v, c) = read_both(&old, &dir.join("old.cram"));
    let _ = fs::remove_dir_all(&dir);
    match v {
        Outcome::Read(_) => {}
        Outcome::Refused(why) => panic!("verify refused a frame without a checksum: {why:?}"),
    }
    match c {
        Outcome::Read(bodies) => assert!(
            bodies == expected_bodies(),
            "a frame without a checksum read back different content"
        ),
        Outcome::Refused(why) => panic!("copy_entry refused a frame without a checksum: {why:?}"),
    }
}

/// Writes the fixture's source tree under `root` and returns the folder to archive.
#[cfg(feature = "zstd-c")]
fn write_fixture_tree(root: &Path) -> PathBuf {
    for (name, body) in fixture_files() {
        let p = root.join(name);
        fs::create_dir_all(p.parent().unwrap()).unwrap();
        fs::write(p, body).unwrap();
    }
    root.join("fx")
}

#[cfg(feature = "zstd-c")]
fn write_archive(tag: &str) -> Vec<u8> {
    write_archive_at(tag, cram_core::writer::CreateOptions::default())
}

#[cfg(feature = "zstd-c")]
fn write_archive_at(tag: &str, opts: cram_core::writer::CreateOptions) -> Vec<u8> {
    let dir = scratch(tag);
    let src = write_fixture_tree(&dir);
    let arc = dir.join("fresh.cram");
    cram_core::engine::create::create(&arc, Format::cram(Codec::None), &[src], opts, &NullSink)
        .unwrap();
    let bytes = fs::read(&arc).unwrap();
    let _ = fs::remove_dir_all(&dir);
    bytes
}

/// The writer side: a `zstd-c` build writes its zstd packs with the checksum set (`zstd_pack`
/// asserts the flag), and every flip in a freshly written archive is reported.
#[cfg(feature = "zstd-c")]
#[test]
fn freshly_written_zstd_pack_carries_a_checksum_and_every_flip_is_reported() {
    let fresh = write_archive("fresh");
    let flips = flip_every_byte(&fresh, "fresh-flips");
    let body = flips
        .refused
        .iter()
        .filter(|&&k| k >= flips.header_len)
        .count();
    assert!(body >= 32, "only {body} block-data flips were reported");
    // Content-neutral flips exist (see FIXTURE_NEUTRAL), but they are a handful; a larger number
    // would mean the checksum is not being compared.
    assert!(
        flips.neutral.len() <= 16,
        "{} flips decoded to unchanged content: {:?}",
        flips.neutral.len(),
        flips.neutral
    );
}

/// `--best`, `--small` and `--tiny` reach zstd through their own compress calls, after XZ, and keep
/// the zstd frame only when it is smaller. The fixture's pack is small enough that XZ's container
/// overhead decides it for zstd at every one of those levels, so each must hand back a checksummed
/// zstd pack. If XZ ever wins here the test fails on the codec, which says the input no longer
/// reaches the call it is meant to test.
#[cfg(feature = "zstd-c")]
#[test]
fn zstd_packs_written_at_best_small_and_tiny_carry_a_checksum() {
    use cram_core::writer::{CreateOptions, Level};
    for (level, tag) in [
        (Level::Best, "best"),
        (Level::Cold, "small"),
        (Level::Tiny, "tiny"),
    ] {
        let opts = CreateOptions {
            level,
            ..CreateOptions::default()
        };
        let arc = write_archive_at(tag, opts);
        let (_, all) = packs(&arc);
        assert_eq!(
            all.len(),
            1,
            "--{tag}: the archive must hold exactly one pack"
        );
        assert_eq!(
            all[0].codec, CODEC_ZSTD,
            "--{tag}: XZ won the fixture's pack, so the zstd call at this level was not tested"
        );
        let p = &all[0];
        assert!(
            arc[p.offset + 4] & CHECKSUM_FLAG != 0,
            "--{tag}: the zstd frame must carry a content checksum (descriptor {:#04x})",
            arc[p.offset + 4]
        );
    }
}

/// Prints a fresh archive of the fixture tree as a Rust byte-array literal, which is how
/// [`FIXTURE`] was made:
/// `cargo test -p cram-core --features zstd-c --test cram_zstd_checksum print_fixture -- --ignored --nocapture`
#[cfg(feature = "zstd-c")]
#[test]
#[ignore]
fn print_fixture() {
    let bytes = write_archive("gen");
    println!("FIXTURE_LEN {}", bytes.len());
    for row in bytes.chunks(16) {
        let line: Vec<String> = row.iter().map(|b| format!("0x{b:02x},")).collect();
        println!("    {}", line.join(" "));
    }
}
