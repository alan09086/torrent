//! DSCP (Differentiated Services Code Point) socket marking utilities.
//!
//! Provides functions to apply DSCP/ToS markings to TCP and UDP sockets.
//! DSCP values occupy the top 6 bits of the IPv4 TOS / IPv6 Traffic Class byte.
//! Failures are logged but never propagated — DSCP is a QoS hint, not a hard requirement.

use std::os::fd::AsRawFd;

/// Apply DSCP/ToS marking to any socket with a file descriptor.
///
/// DSCP value of 0 is a no-op (best-effort, no marking needed).
/// Failures are logged but not propagated — DSCP is a QoS hint.
pub(crate) fn apply_dscp<S: AsRawFd>(socket: &S, dscp: u8, is_ipv6: bool) {
    if dscp == 0 {
        return;
    }
    let tos = (dscp << 2) as libc::c_int; // DSCP occupies top 6 bits of TOS byte
    let fd = socket.as_raw_fd();
    let result = unsafe {
        if is_ipv6 {
            libc::setsockopt(
                fd,
                libc::IPPROTO_IPV6,
                libc::IPV6_TCLASS,
                &tos as *const _ as *const libc::c_void,
                std::mem::size_of::<libc::c_int>() as libc::socklen_t,
            )
        } else {
            libc::setsockopt(
                fd,
                libc::IPPROTO_IP,
                libc::IP_TOS,
                &tos as *const _ as *const libc::c_void,
                std::mem::size_of::<libc::c_int>() as libc::socklen_t,
            )
        }
    };
    if result != 0 {
        tracing::debug!(
            dscp,
            %is_ipv6,
            "failed to set DSCP: {}",
            std::io::Error::last_os_error()
        );
    }
}

/// Apply DSCP to a tokio TcpListener (for accepted connections to inherit).
pub(crate) fn apply_dscp_listener(listener: &tokio::net::TcpListener, dscp: u8) {
    if dscp == 0 {
        return;
    }
    let addr = listener.local_addr().ok();
    let is_ipv6 = addr.is_some_and(|a| a.is_ipv6());
    apply_dscp(listener, dscp, is_ipv6);
}

/// Apply DSCP to a tokio TcpStream.
pub(crate) fn apply_dscp_tcp(stream: &tokio::net::TcpStream, dscp: u8) {
    if dscp == 0 {
        return;
    }
    let addr = stream.peer_addr().ok();
    let is_ipv6 = addr.is_some_and(|a| a.is_ipv6());
    apply_dscp(stream, dscp, is_ipv6);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dscp_zero_is_noop() {
        // Just verify no panic — a no-op for value 0
        let sock = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        apply_dscp(&sock, 0, false);
    }

    #[test]
    fn dscp_ipv4_succeeds() {
        let sock = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        apply_dscp(&sock, 0x04, false); // AF11
        // Verify with getsockopt
        let fd = sock.as_raw_fd();
        let mut tos: libc::c_int = 0;
        let mut len = std::mem::size_of::<libc::c_int>() as libc::socklen_t;
        let r = unsafe {
            libc::getsockopt(
                fd,
                libc::IPPROTO_IP,
                libc::IP_TOS,
                &mut tos as *mut _ as *mut libc::c_void,
                &mut len,
            )
        };
        assert_eq!(r, 0);
        assert_eq!(tos, 0x10); // AF11: 0x04 << 2 = 0x10
    }

    #[test]
    fn dscp_ipv6_succeeds() {
        let sock = std::net::TcpListener::bind("[::1]:0").unwrap();
        apply_dscp(&sock, 0x04, true); // AF11
        let fd = sock.as_raw_fd();
        let mut tclass: libc::c_int = 0;
        let mut len = std::mem::size_of::<libc::c_int>() as libc::socklen_t;
        let r = unsafe {
            libc::getsockopt(
                fd,
                libc::IPPROTO_IPV6,
                libc::IPV6_TCLASS,
                &mut tclass as *mut _ as *mut libc::c_void,
                &mut len,
            )
        };
        assert_eq!(r, 0);
        assert_eq!(tclass, 0x10);
    }

    #[test]
    fn dscp_ef_value() {
        let sock = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        apply_dscp(&sock, 0x2E, false); // EF = 46
        let fd = sock.as_raw_fd();
        let mut tos: libc::c_int = 0;
        let mut len = std::mem::size_of::<libc::c_int>() as libc::socklen_t;
        let r = unsafe {
            libc::getsockopt(
                fd,
                libc::IPPROTO_IP,
                libc::IP_TOS,
                &mut tos as *mut _ as *mut libc::c_void,
                &mut len,
            )
        };
        assert_eq!(r, 0);
        assert_eq!(tos, 0xB8); // 0x2E << 2 = 0xB8
    }
}
