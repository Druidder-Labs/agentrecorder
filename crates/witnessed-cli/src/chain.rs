use sha2::{Digest, Sha256};
use witnessed_store::Event;

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

pub fn token_digest(token: &str) -> String {
    hex(&Sha256::digest(token.as_bytes()))
}

pub fn genesis(token_digest: Option<&str>) -> Hash {
    token_digest.and_then(unhex).unwrap_or(GENESIS)
}

pub fn file_digest(bytes: &[u8]) -> String {
    hex(&Sha256::digest(bytes))
}

pub fn hex(bytes: &[u8]) -> String {
    const DIGITS: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(64);
    for b in bytes {
        out.push(DIGITS[usize::from(b >> 4)] as char);
        out.push(DIGITS[usize::from(b & 15)] as char);
    }
    out
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

    #[test]
    fn a_token_moves_the_genesis_and_every_link_after_it() {
        let d = token_digest("invite-42");
        assert_eq!(genesis(None), GENESIS);
        assert_eq!(genesis(Some(&d)), unhex(&d).unwrap());
        assert_eq!(genesis(Some("not hex")), GENESIS);
        let a = leaf("a");
        assert_ne!(link(&genesis(Some(&d)), &a), link(&GENESIS, &a));
    }
}
