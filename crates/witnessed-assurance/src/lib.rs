pub mod dsse;
pub mod engine;
pub mod git;
pub mod github;
pub mod intoto;
pub mod policy;
pub mod report;
pub mod terraform;

pub const INPUT_SCHEMA: &str = "witnessed.change-input/v0.1";
pub const REPORT_SCHEMA: &str = "witnessed.change-verification/v0.1";
pub const EXIT_ERROR: i32 = 4;

pub fn hex(bytes: &[u8]) -> String {
    const DIGITS: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        out.push(DIGITS[usize::from(b >> 4)] as char);
        out.push(DIGITS[usize::from(b & 15)] as char);
    }
    out
}
