//! Codec2 voice codec for the browser — pure Rust, the same `codec2` crate and
//! wire layout `voicetastic-core` uses on desktop/Android, so encoded audio is
//! interoperable across clients.
//!
//! The codec works on 8 kHz mono i16; the browser captures/plays at the audio
//! context's rate, so we resample to/from 8 kHz here. `codec_param` → mode and
//! the i16 conversion mirror core's `src/codec/imp.rs` exactly.

use codec2::{Codec2, Codec2Mode};

/// Codec2 fixed sample rate.
const CODEC2_RATE: u32 = 8_000;

/// Map the on-wire `codec_param` byte to a Codec2 mode (mirrors core).
fn mode_from_param(b: u8) -> Result<Codec2Mode, String> {
    Ok(match b {
        0 => Codec2Mode::MODE_3200,
        1 => Codec2Mode::MODE_2400,
        2 => Codec2Mode::MODE_1600,
        3 => Codec2Mode::MODE_1400,
        4 => Codec2Mode::MODE_1300,
        5 => Codec2Mode::MODE_1200,
        _ => return Err(format!("unknown codec2 mode index {b}")),
    })
}

/// Encode mono f32 PCM at `in_rate` into concatenated Codec2 frames (the
/// `audio` payload `build_message` chunks). Trailing samples shorter than one
/// codec frame are dropped (< 40 ms).
pub fn encode(pcm: &[f32], in_rate: u32, codec_param: u8) -> Result<Vec<u8>, String> {
    let mut c2 = Codec2::new(mode_from_param(codec_param)?);
    let spf = c2.samples_per_frame();
    let bpf = c2.bits_per_frame().div_ceil(8);

    let mut pcm8k: Vec<f32> = Vec::with_capacity(pcm.len());
    Resampler::new(in_rate, CODEC2_RATE).push(pcm, &mut pcm8k);

    let mut payload = Vec::new();
    let mut i = 0;
    while i + spf <= pcm8k.len() {
        let frame_i16: Vec<i16> = pcm8k[i..i + spf]
            .iter()
            .map(|s| (s.clamp(-1.0, 1.0) * i16::MAX as f32) as i16)
            .collect();
        let mut packed = vec![0u8; bpf];
        c2.encode(&mut packed, &frame_i16);
        payload.extend_from_slice(&packed);
        i += spf;
    }
    Ok(payload)
}

/// Decode concatenated Codec2 frames to 8 kHz mono f32 PCM. The browser
/// resamples on playback (create the `AudioBuffer` at 8 kHz), so we don't
/// upsample here. Returns `(pcm, sample_rate_hz)`.
pub fn decode(payload: &[u8], codec_param: u8) -> Result<(Vec<f32>, u32), String> {
    let mut c2 = Codec2::new(mode_from_param(codec_param)?);
    let spf = c2.samples_per_frame();
    let bpf = c2.bits_per_frame().div_ceil(8);

    let mut pcm: Vec<f32> = Vec::new();
    let mut frame = vec![0i16; spf];
    let mut i = 0;
    while i + bpf <= payload.len() {
        c2.decode(&mut frame, &payload[i..i + bpf]);
        pcm.extend(frame.iter().map(|&s| s as f32 / i16::MAX as f32));
        i += bpf;
    }
    Ok((pcm, CODEC2_RATE))
}

/// Streaming linear resampler (copied verbatim from core's
/// `src/codec/resampler.rs` — pure, dependency-free).
struct Resampler {
    ratio: f64,
    cursor: f64,
    last: f32,
}

impl Resampler {
    fn new(src_hz: u32, dst_hz: u32) -> Self {
        Self {
            ratio: src_hz as f64 / dst_hz as f64,
            cursor: 0.0,
            last: 0.0,
        }
    }

    fn push(&mut self, input: &[f32], dst: &mut Vec<f32>) {
        if input.is_empty() {
            return;
        }
        let n = input.len() as f64;
        while self.cursor < n {
            let idx_floor = self.cursor.floor();
            let frac = (self.cursor - idx_floor) as f32;
            let i0 = idx_floor as isize;
            let s0 = if i0 < 0 { self.last } else { input[i0 as usize] };
            let s1 = if (i0 + 1) < 0 {
                self.last
            } else {
                input.get((i0 + 1) as usize).copied().unwrap_or(s0)
            };
            dst.push(s0 + (s1 - s0) * frac);
            self.cursor += self.ratio;
        }
        if let Some(&last) = input.last() {
            self.last = last;
        }
        self.cursor -= n;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip_is_plausible() {
        // 0.5 s of 8 kHz sine → encode → decode → similar length back.
        let pcm: Vec<f32> = (0..4000)
            .map(|i| (i as f32 * 0.1).sin() * 0.3)
            .collect();
        let bytes = encode(&pcm, 8_000, 5).expect("encode");
        assert!(!bytes.is_empty());
        let (out, rate) = decode(&bytes, 5).expect("decode");
        assert_eq!(rate, 8_000);
        assert!(!out.is_empty());
    }

    #[test]
    fn bad_mode_errors() {
        assert!(encode(&[0.0; 320], 8_000, 9).is_err());
    }
}
