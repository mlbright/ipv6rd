//! The egress address: the source the kernel selects for traffic to the SPA
//! target (ADR-0001). Connecting a UDP socket runs RFC 6724 source-address
//! selection without sending anything; its local address is the answer.

use std::io;
use std::net::{Ipv6Addr, SocketAddr, UdpSocket};

/// Returns the source address the kernel would use to reach `target`, or
/// `None` when there is no usable IPv6 source (e.g. no IPv6 connectivity).
pub fn egress_addr(target: Ipv6Addr) -> io::Result<Option<Ipv6Addr>> {
    let sock = UdpSocket::bind((Ipv6Addr::UNSPECIFIED, 0))?;
    sock.connect((target, 1))?;
    match sock.local_addr()? {
        SocketAddr::V6(a) if !a.ip().is_unspecified() => Ok(Some(*a.ip())),
        _ => Ok(None),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn loopback_selects_loopback() {
        assert_eq!(
            egress_addr(Ipv6Addr::LOCALHOST).unwrap(),
            Some(Ipv6Addr::LOCALHOST)
        );
    }
}
