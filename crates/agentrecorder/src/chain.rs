use agentrecorder_store::Event;
use sha2::{Digest, Sha256};

pub const ALGORITHM: &str = "sha256-chain-v1";
pub const GENESIS: [u8; 32] = [0; 32];

pub type Hash = [u8; 32];

pub fn event_line(e: &Event) -> String {
    e.to_json().to_string()
}

pub fn leaf(line: &str) -> Hash {
    Sha256::digest(line.as_bytes()).into()
}

pub fn link(prev: &Hash, leaf: &Hash) -> Hash {
    let mut h = Sha256::new();
    h.update(prev);
    h.update(leaf);
    h.finalize().into()
}

pub fn file_digest(bytes: &[u8]) -> String {
    hex(&Sha256::digest(bytes))
}

pub fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

pub fn unhex(s: &str) -> Option<Hash> {
    if s.len() != 64 || !s.bytes().all(|b| b.is_ascii_hexdigit()) {
        return None;
    }
    let mut out = [0u8; 32];
    for (i, byte) in out.iter_mut().enumerate() {
        *byte = u8::from_str_radix(&s[i * 2..i * 2 + 2], 16).ok()?;
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn links_depend_on_order_and_hex_round_trips() {
        let (a, b) = (leaf("a"), leaf("b"));
        let ab = link(&link(&GENESIS, &a), &b);
        let ba = link(&link(&GENESIS, &b), &a);
        assert_ne!(ab, ba);
        assert_eq!(unhex(&hex(&ab)), Some(ab));
        assert_eq!(hex(&leaf("")), "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855");
        assert_eq!(unhex("zz"), None);
    }
}
