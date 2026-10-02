//! LUKS2 header: a 4 KiB binary header followed by JSON metadata, stored
//! twice (the second copy right after the first); the valid copy with the
//! higher sequence number wins.

use super::{Hash, Header, Kdf, KeyDigest, Keyslot, c_string, checksum};
use crate::crypto::base64;
use crate::device::BlockDevice;
use crate::error::{Error, Result};
use serde_json::{Map, Value};

const MAGIC_2ND: &[u8; 6] = b"SKUL\xba\xbe";
/// Where cryptsetup may put the second header when the first is damaged.
const SECONDARY_OFFSETS: [u64; 9] = [
    0x4000, 0x8000, 0x10000, 0x20000, 0x40000, 0x80000, 0x100000, 0x200000, 0x400000,
];

fn be64(b: &[u8], off: usize) -> u64 {
    u64::from_be_bytes(b[off..off + 8].try_into().unwrap())
}

fn bad(what: impl std::fmt::Display) -> Error {
    Error::corrupt(format!("LUKS2: {what}"))
}

struct Copy {
    seqid: u64,
    bin: Vec<u8>,
    json: Vec<u8>,
}

/// Read and verify the header copy at `offset`.
fn read_copy(dev: &dyn BlockDevice, offset: u64, magic: &[u8; 6]) -> Result<Option<Copy>> {
    if offset + 4096 > dev.size() {
        return Ok(None);
    }
    let mut bin = vec![0u8; 4096];
    dev.read_at(offset, &mut bin)?;
    if &bin[..6] != magic || u16::from_be_bytes([bin[6], bin[7]]) != 2 {
        return Ok(None);
    }
    let hdr_size = be64(&bin, 8);
    if !(0x4000..=0x400000).contains(&hdr_size) || !hdr_size.is_power_of_two() || be64(&bin, 256) != offset {
        return Ok(None);
    }
    if offset + hdr_size > dev.size() {
        return Ok(None);
    }
    let mut json = vec![0u8; hdr_size as usize - 4096];
    dev.read_at(offset + 4096, &mut json)?;
    let alg = c_string(&bin[72..104]);
    let stored = bin[448..512].to_vec();
    let mut zeroed = bin.clone();
    zeroed[448..512].fill(0);
    let sum = match checksum(&alg, &[&zeroed, &json]) {
        Ok(s) => s,
        Err(_) => return Ok(None),
    };
    if stored[..sum.len()] != sum[..] {
        log::warn!("LUKS2 header at {offset}: checksum mismatch");
        return Ok(None);
    }
    Ok(Some(Copy {
        seqid: be64(&bin, 16),
        bin,
        json,
    }))
}

pub(super) fn read(dev: &dyn BlockDevice, _first: &[u8]) -> Result<Header> {
    let primary = read_copy(dev, 0, super::MAGIC)?;
    let mut secondary = None;
    if let Some(p) = &primary {
        secondary = read_copy(dev, be64(&p.bin, 8), MAGIC_2ND)?;
    }
    if secondary.is_none() {
        for off in SECONDARY_OFFSETS {
            if let Some(c) = read_copy(dev, off, MAGIC_2ND)? {
                secondary = Some(c);
                break;
            }
        }
    }
    let best = match (primary, secondary) {
        (Some(p), Some(s)) => {
            if s.seqid > p.seqid {
                s
            } else {
                p
            }
        }
        (Some(p), None) => p,
        (None, Some(s)) => s,
        (None, None) => return Err(bad("no valid header copy")),
    };
    let end = best.json.iter().position(|&b| b == 0).unwrap_or(best.json.len());
    let v: Value = serde_json::from_slice(&best.json[..end]).map_err(|e| bad(format!("JSON: {e}")))?;
    parse_json(&v, &best.bin)
}

fn obj<'a>(v: &'a Value, key: &str) -> Result<&'a Map<String, Value>> {
    v.get(key)
        .and_then(Value::as_object)
        .ok_or_else(|| bad(format!("missing object {key:?}")))
}

fn text<'a>(v: &'a Value, key: &str) -> Result<&'a str> {
    v.get(key)
        .and_then(Value::as_str)
        .ok_or_else(|| bad(format!("missing string {key:?}")))
}

/// A 64-bit number stored as a JSON string, or a plain JSON number.
fn number(v: &Value, key: &str) -> Result<u64> {
    match v.get(key) {
        Some(Value::String(s)) => s.parse().map_err(|_| bad(format!("{key:?} is not a number"))),
        Some(Value::Number(n)) => n.as_u64().ok_or_else(|| bad(format!("{key:?} out of range"))),
        _ => Err(bad(format!("missing number {key:?}"))),
    }
}

fn number32(v: &Value, key: &str) -> Result<u32> {
    u32::try_from(number(v, key)?).map_err(|_| bad(format!("{key:?} out of range")))
}

fn b64(v: &Value, key: &str) -> Result<Vec<u8>> {
    base64::decode_std(text(v, key)?).ok_or_else(|| bad(format!("{key:?} is not base64")))
}

fn ids(v: &Value, key: &str) -> Result<Vec<u32>> {
    v.get(key)
        .and_then(Value::as_array)
        .ok_or_else(|| bad(format!("missing list {key:?}")))?
        .iter()
        .map(|x| {
            x.as_str()
                .and_then(|s| s.parse().ok())
                .ok_or_else(|| bad(format!("bad id in {key:?}")))
        })
        .collect()
}

fn parse_json(v: &Value, bin: &[u8]) -> Result<Header> {
    if let Some(req) = v.get("config").and_then(|c| c.get("requirements")) {
        let mandatory = req.get("mandatory").and_then(Value::as_array);
        if mandatory.is_some_and(|m| !m.is_empty()) {
            return Err(Error::unsupported(format!(
                "LUKS2 requirements {}",
                req.get("mandatory").unwrap()
            )));
        }
    }
    // the data segment
    let segments = obj(v, "segments")?;
    let crypt: Vec<(&String, &Value)> = segments.iter().collect();
    if crypt.len() != 1 {
        return Err(Error::unsupported(
            "LUKS2 with several segments (re-encryption in progress?)",
        ));
    }
    let (seg_id, seg) = crypt[0];
    let seg_id: u32 = seg_id.parse().map_err(|_| bad("segment id"))?;
    if text(seg, "type")? != "crypt" {
        return Err(Error::unsupported(format!("LUKS2 segment type {}", text(seg, "type")?)));
    }
    if seg.get("integrity").is_some() {
        return Err(Error::unsupported("LUKS2 with authenticated encryption (dm-integrity)"));
    }
    let cipher = text(seg, "encryption")?.to_string();
    let data_offset = number(seg, "offset")?;
    let data_size = match seg.get("size") {
        Some(Value::String(s)) if s == "dynamic" => None,
        _ => Some(number(seg, "size")?),
    };
    let iv_tweak = number(seg, "iv_tweak")?;
    let sector_size = number32(seg, "sector_size")?;

    // digests of the segment's volume key
    let mut digests = Vec::new();
    for (_, d) in obj(v, "digests")? {
        if text(d, "type")? != "pbkdf2" {
            log::info!("LUKS2: skipping digest of type {}", text(d, "type")?);
            continue;
        }
        if !ids(d, "segments")?.contains(&seg_id) {
            continue;
        }
        digests.push(KeyDigest {
            hash: Hash::parse(text(d, "hash")?)?,
            iterations: number32(d, "iterations")?,
            salt: b64(d, "salt")?,
            digest: b64(d, "digest")?,
            keyslots: ids(d, "keyslots")?,
        });
    }
    if digests.is_empty() {
        return Err(bad("no digest for the data segment"));
    }

    let mut keyslots = Vec::new();
    let mut key_size = 0;
    for (id, k) in obj(v, "keyslots")? {
        let id: u32 = id.parse().map_err(|_| bad("key slot id"))?;
        if !digests.iter().any(|d| d.keyslots.contains(&id)) {
            continue;
        }
        if text(k, "type")? != "luks2" {
            log::info!("LUKS2: skipping key slot {id} of type {}", text(k, "type")?);
            continue;
        }
        let af = k.get("af").ok_or_else(|| bad("key slot without af"))?;
        let area = k.get("area").ok_or_else(|| bad("key slot without area"))?;
        let kdf = k.get("kdf").ok_or_else(|| bad("key slot without kdf"))?;
        if text(af, "type")? != "luks1" || text(area, "type")? != "raw" {
            return Err(Error::unsupported(format!("LUKS2 key slot {id} layout")));
        }
        let kdf = match text(kdf, "type")? {
            "pbkdf2" => Kdf::Pbkdf2 {
                hash: Hash::parse(text(kdf, "hash")?)?,
                iterations: number32(kdf, "iterations")?,
                salt: b64(kdf, "salt")?,
            },
            t @ ("argon2i" | "argon2id") => Kdf::Argon2 {
                id: t == "argon2id",
                time: number32(kdf, "time")?,
                memory_kib: number32(kdf, "memory")?,
                lanes: number32(kdf, "cpus")?,
                salt: b64(kdf, "salt")?,
            },
            t => return Err(Error::unsupported(format!("LUKS2 key derivation {t}"))),
        };
        let ks = Keyslot {
            id,
            kdf,
            priority: match k.get("priority") {
                Some(_) => number32(k, "priority")?,
                None => 1,
            },
            area_offset: number(area, "offset")?,
            area_cipher: text(area, "encryption")?.to_string(),
            area_key_size: number(area, "key_size")? as usize,
            key_size: number(k, "key_size")? as usize,
            stripes: number32(af, "stripes")?,
            af_hash: Hash::parse(text(af, "hash")?)?,
        };
        if key_size == 0 {
            key_size = ks.key_size;
        } else if ks.key_size != key_size {
            return Err(bad("key slots with different key sizes"));
        }
        keyslots.push(ks);
    }
    if keyslots.is_empty() {
        return Err(bad("no usable key slot"));
    }
    Ok(Header {
        version: 2,
        uuid: c_string(&bin[168..208]),
        label: c_string(&bin[24..72]),
        cipher,
        key_size,
        data_offset,
        data_size,
        sector_size,
        iv_tweak,
        keyslots,
        digests,
    })
}
