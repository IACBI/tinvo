//! Base64 (RFC 4648, the standard alphabet, padded), for binary data kept in
//! text files: a plugin's state in a project.

const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

/// `data` as base64.
pub fn encode(data: &[u8]) -> String {
    let mut out = String::with_capacity(data.len().div_ceil(3) * 4);
    for chunk in data.chunks(3) {
        let b = [chunk[0], chunk.get(1).copied().unwrap_or(0), chunk.get(2).copied().unwrap_or(0)];
        let n = u32::from(b[0]) << 16 | u32::from(b[1]) << 8 | u32::from(b[2]);
        for i in 0..4 {
            if i <= chunk.len() {
                out.push(char::from(ALPHABET[(n >> (18 - 6 * i)) as usize & 63]));
            } else {
                out.push('=');
            }
        }
    }
    out
}

/// The bytes `text` encodes, if it is base64 as [`encode`] writes it: in
/// whole groups of four, padded only at the end, with no bits left over.
pub fn decode(text: &str) -> Option<Vec<u8>> {
    let t = text.as_bytes();
    if t.len() % 4 != 0 {
        return None;
    }
    let value = |c: u8| -> Option<u32> {
        Some(u32::from(match c {
            b'A'..=b'Z' => c - b'A',
            b'a'..=b'z' => c - b'a' + 26,
            b'0'..=b'9' => c - b'0' + 52,
            b'+' => 62,
            b'/' => 63,
            _ => return None,
        }))
    };
    let mut out = Vec::with_capacity(t.len() / 4 * 3);
    for (k, group) in t.chunks(4).enumerate() {
        let last = k + 1 == t.len() / 4;
        let pad = group.iter().rev().take_while(|&&c| c == b'=').count();
        if pad > 2 || (pad > 0 && !last) {
            return None;
        }
        let mut n = 0;
        for &c in &group[..4 - pad] {
            n = n << 6 | value(c)?;
        }
        n <<= 6 * pad;
        let bytes = [(n >> 16) as u8, (n >> 8) as u8, n as u8];
        let keep = 3 - pad;
        // The bits past the last byte must be zero, so a text has one
        // reading.
        if bytes[keep..].iter().any(|&b| b != 0) {
            return None;
        }
        out.extend_from_slice(&bytes[..keep]);
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rfc_4648_vectors_round_trip() {
        for (plain, coded) in [
            ("", ""),
            ("f", "Zg=="),
            ("fo", "Zm8="),
            ("foo", "Zm9v"),
            ("foob", "Zm9vYg=="),
            ("fooba", "Zm9vYmE="),
            ("foobar", "Zm9vYmFy"),
        ] {
            assert_eq!(encode(plain.as_bytes()), coded);
            assert_eq!(decode(coded).as_deref(), Some(plain.as_bytes()));
        }
        let all: Vec<u8> = (0..=255).cycle().take(1000).collect();
        assert_eq!(decode(&encode(&all)), Some(all));
        assert_eq!(encode(&[0xfb, 0xff]), "+/8=");
    }

    #[test]
    fn anything_else_is_refused() {
        for bad in ["Zg", "Zg=", "Z===", "Zg==Zg==", "Zm9v Yg==", "Zm9v\n", "Zh==", "Zm9=", "Zm-v", "===="] {
            assert_eq!(decode(bad), None, "{bad:?}");
        }
    }
}
