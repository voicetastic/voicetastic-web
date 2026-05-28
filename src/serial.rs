//! Meshtastic USB-serial framing — the 4-byte start header + length-prefix
//! protocol used between the device firmware and us. Self-contained: no
//! protocol-decode logic lives here, only the framer/deframer the read
//! loop and writer call into.

/// Magic bytes that begin every serial-framed packet (see core's serial.rs).
pub const START1: u8 = 0x94;
pub const START2: u8 = 0xc3;
/// Maximum protobuf payload length accepted by the device.
pub const MAX_PAYLOAD: usize = 512;
/// Default baud for Meshtastic USB-serial.
pub const BAUD: u32 = 115_200;

/// Prepend the 4-byte Meshtastic serial header (`0x94 0xc3 len_hi len_lo`).
pub fn frame_serial(payload: &[u8]) -> Vec<u8> {
    let len = payload.len() as u16;
    let mut v = Vec::with_capacity(payload.len() + 4);
    v.extend_from_slice(&[START1, START2, (len >> 8) as u8, (len & 0xff) as u8]);
    v.extend_from_slice(payload);
    v
}

/// Extract one framed payload from the front of `buf`, scanning past console
/// noise. Returns `(payload, bytes_consumed)`, an empty payload as a resync
/// marker on a bad length, or `None` when more bytes are needed.
pub fn next_frame(buf: &[u8]) -> Option<(Vec<u8>, usize)> {
    let mut i = 0;
    while i + 1 < buf.len() {
        if buf[i] == START1 && buf[i + 1] == START2 {
            break;
        }
        i += 1;
    }
    if i + 1 >= buf.len() {
        return None;
    }
    if i + 4 > buf.len() {
        return None;
    }
    let len = ((buf[i + 2] as usize) << 8) | (buf[i + 3] as usize);
    if len == 0 || len > MAX_PAYLOAD {
        return Some((Vec::new(), i + 1));
    }
    let start = i + 4;
    if start + len > buf.len() {
        return None;
    }
    Some((buf[start..start + len].to_vec(), start + len))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frame_roundtrip() {
        let f = frame_serial(b"hi");
        assert_eq!(&f[..4], &[0x94, 0xc3, 0x00, 0x02]);
        let (p, n) = next_frame(&f).unwrap();
        assert_eq!(p, b"hi");
        assert_eq!(n, f.len());
    }

    #[test]
    fn skips_leading_noise() {
        let mut data = b"debug log\n".to_vec();
        data.extend(frame_serial(b"ok"));
        assert_eq!(next_frame(&data).unwrap().0, b"ok");
    }
}
