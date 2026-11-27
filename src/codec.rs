//! Small, dependency-free pieces of the voice protocol: 8-bit companding, byte headers and WAV.
//!
//! The engine exposes no raw PCM playback and Opus needs a native C build, so voice frames are
//! companded 16 kHz mono samples: 320 bytes for a 20 ms frame (16 KB/s). Weft's typed `Vec<u8>`
//! channels carry the frames and the engine's QUIC transport handles ordering.

pub const RATE: u32 = 16_000;
pub const FRAME: usize = 320; // 20 ms at 16 kHz

/// Convert one f32 sample in -1..1 to an 8-bit companded byte: sign plus a square-root
/// magnitude. Square-root companding keeps small sounds detailed and is exactly the inverse of
/// `decode`, so voice comes back clean instead of as the harsh noise a mismatched G.711 table
/// produces.
pub fn encode(sample: f32) -> u8 {
    let s = sample.clamp(-1.0, 1.0);
    let magnitude = (s.abs().sqrt() * 127.0) as u8;
    if s < 0.0 {
        0x80 | magnitude
    } else {
        magnitude
    }
}

/// Convert a companded byte back to an f32 sample in -1..1.
pub fn decode(byte: u8) -> f32 {
    let magnitude = (byte & 0x7f) as f32 / 127.0;
    let value = magnitude * magnitude;
    if byte & 0x80 != 0 {
        -value
    } else {
        value
    }
}

pub fn encode_frame(samples: &[f32; FRAME]) -> Vec<u8> {
    samples.iter().map(|&s| encode(s)).collect()
}

pub fn decode_frame(bytes: &[u8], out: &mut [f32; FRAME]) {
    for (i, &b) in bytes.iter().take(FRAME).enumerate() {
        out[i] = decode(b);
    }
    for s in out.iter_mut().skip(bytes.len().min(FRAME)) {
        *s = 0.0;
    }
}

/// The header of a server-to-client audio packet, 40 bytes.
/// `seq[0..4] speaker[4..12] dim[12..16] group[16..24] x[24..28] y[28..32] z[32..36]
///  count[36..38] flags[38..40]` followed by `count` mu-law bytes.
pub const HEADER: usize = 40;
pub const FLAG_GROUP: u16 = 1;

pub fn put_u32(out: &mut Vec<u8>, v: u32) {
    out.extend_from_slice(&v.to_le_bytes());
}
pub fn put_u64(out: &mut Vec<u8>, v: u64) {
    out.extend_from_slice(&v.to_le_bytes());
}
pub fn put_f32(out: &mut Vec<u8>, v: f32) {
    out.extend_from_slice(&v.to_le_bytes());
}
pub fn put_u16(out: &mut Vec<u8>, v: u16) {
    out.extend_from_slice(&v.to_le_bytes());
}
pub fn get_u32(bytes: &[u8], at: usize) -> Option<u32> {
    bytes.get(at..at + 4).and_then(|b| b.try_into().ok()).map(u32::from_le_bytes)
}
pub fn get_u64(bytes: &[u8], at: usize) -> Option<u64> {
    bytes.get(at..at + 8).and_then(|b| b.try_into().ok()).map(u64::from_le_bytes)
}
pub fn get_f32(bytes: &[u8], at: usize) -> Option<f32> {
    bytes.get(at..at + 4).and_then(|b| b.try_into().ok()).map(f32::from_le_bytes)
}
pub fn get_u16(bytes: &[u8], at: usize) -> Option<u16> {
    bytes.get(at..at + 2).and_then(|b| b.try_into().ok()).map(u16::from_le_bytes)
}

/// Builds the packet sent by the server to listeners.
pub fn wrap(seq: u32, speaker: u64, dim: u32, group: u64, pos: [f32; 3], audio: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(HEADER + audio.len());
    put_u32(&mut out, seq);
    put_u64(&mut out, speaker);
    put_u32(&mut out, dim);
    put_u64(&mut out, group);
    for &v in &pos {
        put_f32(&mut out, v);
    }
    put_u16(&mut out, audio.len().min(FRAME) as u16);
    put_u16(&mut out, if group != 0 { FLAG_GROUP } else { 0 });
    out.extend_from_slice(audio);
    out
}

/// A WAV file needs only a 44 byte header; write it once recording ends.
pub fn write_wav(path: &str, samples: &[i16], rate: u32) -> std::io::Result<()> {
    let data_len = (samples.len() * 2) as u32;
    let mut bytes = Vec::with_capacity(44 + samples.len() * 2);
    bytes.extend_from_slice(b"RIFF");
    bytes.extend_from_slice(&(36u32.wrapping_add(data_len)).to_le_bytes());
    bytes.extend_from_slice(b"WAVEfmt ");
    bytes.extend_from_slice(&16u32.to_le_bytes()); // fmt chunk size
    bytes.extend_from_slice(&1u16.to_le_bytes()); // PCM
    bytes.extend_from_slice(&1u16.to_le_bytes()); // mono
    bytes.extend_from_slice(&rate.to_le_bytes());
    bytes.extend_from_slice(&(rate * 2).to_le_bytes()); // byte rate
    bytes.extend_from_slice(&2u16.to_le_bytes()); // block align
    bytes.extend_from_slice(&16u16.to_le_bytes()); // bits per sample
    bytes.extend_from_slice(b"data");
    bytes.extend_from_slice(&data_len.to_le_bytes());
    for &s in samples {
        bytes.extend_from_slice(&s.to_le_bytes());
    }
    std::fs::write(path, bytes)
}
