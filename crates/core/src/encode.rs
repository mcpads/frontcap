//! Standard base64 (RFC 4648), for images and bodies carried in JSON.
const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

pub fn base64(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let b = [chunk[0], *chunk.get(1).unwrap_or(&0), *chunk.get(2).unwrap_or(&0)];
        let n = (u32::from(b[0]) << 16) | (u32::from(b[1]) << 8) | u32::from(b[2]);
        out.push(ALPHABET[(n >> 18 & 63) as usize] as char);
        out.push(ALPHABET[(n >> 12 & 63) as usize] as char);
        out.push(if chunk.len() > 1 { ALPHABET[(n >> 6 & 63) as usize] as char } else { '=' });
        out.push(if chunk.len() > 2 { ALPHABET[(n & 63) as usize] as char } else { '=' });
    }
    out
}

/// Decodes standard base64 (padding optional); `None` on any other character or a dangling character.
pub fn unbase64(text: &str) -> Option<Vec<u8>> {
    const INVALID: u8 = 0xff;
    const TABLE: [u8; 256] = {
        let mut t = [INVALID; 256];
        let mut i = 0;
        while i < 64 {
            t[ALPHABET[i] as usize] = i as u8;
            i += 1;
        }
        t
    };
    let body = text.trim_end_matches('=');
    if body.len() % 4 == 1 || body.contains('=') {
        return None;
    }
    let mut out = Vec::with_capacity(body.len() / 4 * 3 + 2);
    let (mut acc, mut bits) = (0u32, 0u32);
    for c in body.bytes() {
        let v = TABLE[c as usize];
        if v == INVALID {
            return None;
        }
        acc = (acc << 6) | u32::from(v);
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push((acc >> bits) as u8);
            acc &= (1 << bits) - 1;
        }
    }
    Some(out)
}

#[cfg(test)]
#[path = "encode_tests.rs"]
mod tests;
