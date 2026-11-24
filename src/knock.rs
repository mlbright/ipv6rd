//! Building and sending SPA packets (ADR-0006, ADR-0008).
//!
//! Each access entry is its own SPA packet: the message format carries a single
//! protocol/port. Every packet goes to the one pinned target address, from a
//! socket bound to the egress address it authorizes, so the packet's source
//! always equals its allow IP — which fwknop-rs's `require_source_address`
//! checks.

use std::io;
use std::net::{IpAddr, Ipv6Addr, SocketAddrV6, UdpSocket};
use std::time::Duration;

use fkospa::{AccessMessage, AccessSpec, KeySet, Request, SpaData, SpaMessage};

use crate::keys::DIGEST_TYPE;
use crate::log_info;

/// Sends the configured access request to the SPA target.
#[derive(Debug)]
pub struct Knocker {
    /// The pinned target address and the fwknopd UDP port.
    pub target: SocketAddrV6,
    /// One SPA packet per entry.
    pub access: Vec<AccessSpec>,
    pub keys: KeySet,
}

impl Knocker {
    /// One knock attempt for `allow`: every access entry in order. The first
    /// failure aborts; the caller retries the whole set.
    pub fn knock(&self, allow: Ipv6Addr, timeout: Duration) -> Result<(), String> {
        for spec in &self.access {
            let data = build_packet(&self.keys, allow, *spec)
                .map_err(|e| format!("access {spec}: building SPA packet: {e}"))?;
            send_packet(allow, self.target, data.as_str().as_bytes(), timeout)
                .map_err(|e| format!("access {spec}: {e}"))?;
            log_info!("sending access", "entry" => spec, "addr" => allow);
        }
        Ok(())
    }
}

/// Builds and encrypts one SPA access packet authorizing `allow` for `spec`,
/// with the fixed scheme from [`crate::keys`]. The username is the login name
/// of the user the daemon runs as, as in the Go version.
pub fn build_packet(
    keys: &KeySet,
    allow: Ipv6Addr,
    spec: AccessSpec,
) -> Result<SpaData, fkospa::Error> {
    let mut msg = SpaMessage::new(Request::access(AccessMessage::new(IpAddr::V6(allow), spec)))?;
    msg.digest_type = DIGEST_TYPE;
    msg.encrypt(keys)
}

/// Sends `data` to `to` from a UDP socket bound to `from`. Binding fails at
/// once (for example `EADDRNOTAVAIL`) if `from` is no longer assigned to this
/// host, instead of sending a packet the server would drop.
pub fn send_packet(
    from: Ipv6Addr,
    to: SocketAddrV6,
    data: &[u8],
    timeout: Duration,
) -> io::Result<()> {
    let sock = UdpSocket::bind(SocketAddrV6::new(from, 0, 0, 0))
        .map_err(|e| io::Error::new(e.kind(), format!("binding to [{from}]: {e}")))?;
    sock.set_write_timeout(Some(timeout))?;
    sock.connect(to)
        .map_err(|e| io::Error::new(e.kind(), format!("connecting to {to}: {e}")))?;
    let n = sock
        .send(data)
        .map_err(|e| io::Error::new(e.kind(), format!("sending to {to}: {e}")))?;
    if n != data.len() {
        return Err(io::Error::other(format!(
            "short send to {to}: {n} of {} bytes",
            data.len()
        )));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::keys::build_keyset;
    use fkospa::{EncryptionKey, EncryptionMode, HmacKey, HmacType, MessageType};
    use zeroize::Zeroizing;

    fn random_key(len: usize) -> Zeroizing<Vec<u8>> {
        let b64 = fkospa::generate_key(len).unwrap();
        Zeroizing::new(fkospa::decode_base64_key(&b64).unwrap())
    }

    fn keyset() -> KeySet {
        build_keyset(random_key(32), random_key(64)).unwrap()
    }

    fn me() -> String {
        nix::unistd::User::from_uid(nix::unistd::getuid())
            .unwrap()
            .map(|u| u.name)
            .unwrap_or_else(|| std::env::var("USER").unwrap())
    }

    fn spec(s: &str) -> AccessSpec {
        s.parse().unwrap()
    }

    #[test]
    fn packets_decrypt_with_the_server_codec() {
        let keys = keyset();
        let allow: Ipv6Addr = "2001:db8::5".parse().unwrap();
        let data = build_packet(&keys, allow, spec("tcp/443")).unwrap();

        let msg = SpaMessage::decrypt(&data, &keys).unwrap();
        let access = msg.request.access_message().expect("an access message");
        assert_eq!(access.allow_ip, IpAddr::V6(allow));
        assert_eq!(access.spec, spec("tcp/443"));
        assert_eq!(msg.message_type(), MessageType::Access);
        assert_eq!(msg.request.client_timeout(), None);
        assert_eq!(msg.digest_type, fkospa::DigestType::Sha256);
        assert_eq!(msg.username, me());
        assert_eq!(msg.server_auth, None);
    }

    #[test]
    fn packets_only_decrypt_under_cbc_and_hmac_sha256() {
        let keys = keyset();
        let data = build_packet(&keys, "2001:db8::5".parse().unwrap(), spec("tcp/22")).unwrap();
        let enc = keys.encryption_key.clone();
        let hmac = keys.hmac.clone().unwrap();

        // fkospa's legacy IV mode differs from CBC only by padding keys shorter
        // than 16 bytes; with the 32-byte keys the key rules require, the two
        // produce the same bytes, so the mode is pinned in `keys` instead.
        for weaker in [
            HmacType::Md5,
            HmacType::Sha1,
            HmacType::Sha384,
            HmacType::Sha512,
        ] {
            let ks = KeySet::new(
                enc.clone(),
                EncryptionMode::Cbc,
                Some(HmacKey::new(hmac.as_bytes(), weaker)),
            );
            assert!(SpaMessage::decrypt(&data, &ks).is_err(), "{weaker}");
        }
        let no_hmac = KeySet::new(enc, EncryptionMode::Cbc, None);
        assert!(
            SpaMessage::decrypt(&data, &no_hmac).is_err(),
            "without HMAC"
        );
        let wrong_key = KeySet::new(
            EncryptionKey::new(vec![9; 32]),
            EncryptionMode::Cbc,
            Some(hmac),
        );
        assert!(
            SpaMessage::decrypt(&data, &wrong_key).is_err(),
            "wrong encryption key"
        );
    }

    #[test]
    fn every_packet_is_fresh() {
        let keys = keyset();
        let allow: Ipv6Addr = "2001:db8::5".parse().unwrap();
        let a = build_packet(&keys, allow, spec("tcp/443")).unwrap();
        let b = build_packet(&keys, allow, spec("tcp/443")).unwrap();
        assert_ne!(
            a.as_str(),
            b.as_str(),
            "random value and salt must differ per packet"
        );
    }

    #[test]
    fn knock_sends_one_packet_per_entry_from_the_allow_ip() {
        let server = UdpSocket::bind("[::1]:0").unwrap();
        server
            .set_read_timeout(Some(Duration::from_secs(5)))
            .unwrap();
        let std::net::SocketAddr::V6(target) = server.local_addr().unwrap() else {
            unreachable!()
        };
        let knocker = Knocker {
            target,
            access: vec![spec("tcp/443"), spec("udp/53"), spec("tcp/22")],
            keys: keyset(),
        };

        knocker
            .knock(Ipv6Addr::LOCALHOST, Duration::from_secs(5))
            .unwrap();

        let mut buf = [0u8; 2048];
        for want in ["tcp/443", "udp/53", "tcp/22"] {
            let (n, src) = server.recv_from(&mut buf).unwrap();
            assert_eq!(src.ip(), IpAddr::V6(Ipv6Addr::LOCALHOST), "UDP source");
            let msg = SpaMessage::decrypt(&SpaData::from_bytes(&buf[..n]), &knocker.keys).unwrap();
            let access = msg.request.access_message().unwrap();
            assert_eq!(
                access.allow_ip,
                src.ip(),
                "allow IP must equal the packet source"
            );
            assert_eq!(access.spec, spec(want));
        }
        server
            .set_read_timeout(Some(Duration::from_millis(200)))
            .unwrap();
        assert!(server.recv_from(&mut buf).is_err(), "no extra packets");
    }

    #[test]
    fn knock_fails_fast_when_the_egress_address_is_gone() {
        let server = UdpSocket::bind("[::1]:0").unwrap();
        server
            .set_read_timeout(Some(Duration::from_millis(200)))
            .unwrap();
        let std::net::SocketAddr::V6(target) = server.local_addr().unwrap() else {
            unreachable!()
        };
        let knocker = Knocker {
            target,
            access: vec![spec("tcp/443")],
            keys: keyset(),
        };

        // 2001:db8::/32 is documentation space: never assigned to this host.
        let gone: Ipv6Addr = "2001:db8::dead".parse().unwrap();
        let err = knocker.knock(gone, Duration::from_secs(1)).unwrap_err();
        assert!(err.contains("binding to [2001:db8::dead]"), "{err}");
        let mut buf = [0u8; 2048];
        assert!(server.recv_from(&mut buf).is_err(), "nothing may be sent");
    }
}
