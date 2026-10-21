use std::io;
use std::os::unix::net::UnixStream;

#[cfg(target_os = "linux")]
pub fn peer_uid(stream: &UnixStream) -> io::Result<u32> {
    use nix::sys::socket::{getsockopt, sockopt::PeerCredentials};
    Ok(getsockopt(stream, PeerCredentials)?.uid())
}

#[cfg(not(target_os = "linux"))]
pub fn peer_uid(stream: &UnixStream) -> io::Result<u32> {
    Ok(nix::unistd::getpeereid(stream)?.0.as_raw())
}
