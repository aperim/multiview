//! Resolving the **concrete** local address an inbound datagram arrived on, so
//! str0m is fed a destination it gathered as a local candidate (box-validation
//! defect #3) — **feature `native`**.
//!
//! The single shared socket binds the unspecified dual-stack address `[::]`
//! (ADR-0042/§4). The kernel never tells `recv_from` which concrete local IP a
//! datagram landed on, so the driver historically fed str0m the bind addr
//! (`[::]:PORT`). But str0m matches each inbound STUN binding-request's
//! `destination` against its gathered **local candidates** — the concrete
//! `advertised_addresses` (host candidates) and relay candidates, *never* the
//! unspecified bind. An unspecified destination matches nothing, so str0m logs
//! "Discarding STUN request on unknown interface" and ICE never completes
//! (str0m 0.16.2 `ice/agent.rs` `local_candidates … v.addr() == req.destination`).
//!
//! The fix is in two parts:
//!
//! * [`recv_from_with_local`] — a `recvmsg(2)` read that asks the kernel for the
//!   datagram's **destination IP** via `IPV6_PKTINFO` / `IP_PKTINFO` (the
//!   canonical way to recover the concrete local address on a wildcard-bound
//!   socket). It is the only `unsafe` here (cmsg parsing); it is allocation-light
//!   and never panics.
//! * [`resolve_local_destination`] — a **pure**, fully unit-tested mapping from
//!   the concrete arrival address onto a gathered candidate: an exact match is
//!   returned verbatim; otherwise the candidate of the same IP family is chosen
//!   (NAT 1:1 / Docker — PKTINFO reports the private interface IP while str0m
//!   only knows the public advertised candidate); as a last resort the first
//!   concrete candidate is used. The unspecified bind addr is *never* returned.

use std::net::SocketAddr;

/// Map the concrete address an inbound datagram arrived on (`arrival`, as the
/// kernel reported it via PKTINFO) onto the local candidate str0m gathered, so
/// the STUN `destination` matches a known candidate (defect #3).
///
/// Resolution order:
/// 1. **Exact match** — `arrival` is itself a gathered candidate (the common,
///    direct, multi-homed case): return it unchanged.
/// 2. **Same-family match** — no exact match (NAT 1:1 / Docker reports the
///    private interface IP while the gathered candidate is the public advertised
///    address): return the first gathered candidate of the same IP family.
/// 3. **First concrete candidate** — neither matched: return the first gathered
///    candidate (str0m will pair it with the remote; any valid pair connects).
///
/// `arrival` is returned **only** when it is itself a non-unspecified gathered
/// candidate; the unspecified bind addr is never returned (str0m discards it).
/// When `candidates` is empty there is nothing to map to, so `arrival` is
/// returned as-is (the caller gathered no candidate — a misconfiguration the
/// negotiation path already rejects).
#[must_use]
pub fn resolve_local_destination(arrival: SocketAddr, candidates: &[SocketAddr]) -> SocketAddr {
    // 1. Exact match: arrival is a gathered candidate.
    if candidates.contains(&arrival) {
        return arrival;
    }
    // 2. Same-family match: pick the gathered candidate of the same IP family.
    if let Some(same_family) = candidates.iter().find(|c| c.is_ipv4() == arrival.is_ipv4()) {
        return *same_family;
    }
    // 3. Fall back to the first gathered candidate; if none, leave arrival as-is.
    candidates.first().copied().unwrap_or(arrival)
}

#[cfg(unix)]
pub(crate) use unix::{enable_pktinfo, recv_from_with_local};

// The `native`+`unix` PKTINFO seam: the crate's only `unsafe`. The workspace
// `forbid(unsafe_code)` is relaxed to `deny` in this crate's `[lints]` (mirroring
// `multiview-ntpsys`); every `unsafe` block below carries a `// SAFETY:` comment.
// The module-level allow keeps the FFI isolated here — the rest of the crate
// (and the default build) stays unsafe-free.
#[cfg(unix)]
#[allow(
    unsafe_code,
    reason = "the recvmsg(2) cmsg-parsing FFI for IPV6_PKTINFO/IP_PKTINFO; each \
              unsafe block carries a // SAFETY: justification (defect #3)"
)]
mod unix {
    use std::io;
    use std::mem::MaybeUninit;
    use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
    use std::os::fd::AsRawFd;

    use socket2::{MaybeUninitSlice, MsgHdrMut, SockAddr, SockAddrStorage, Socket};

    /// Ask the kernel to deliver the datagram's destination address as ancillary
    /// data on every `recvmsg`: `IPV6_RECVPKTINFO` (which on a dual-stack socket
    /// also reports IPv4-mapped destinations) plus `IP_PKTINFO` for safety. Must
    /// be called once on the bound socket before the recv loop.
    ///
    /// # Errors
    ///
    /// The underlying `setsockopt` error.
    pub(crate) fn enable_pktinfo(socket: &Socket) -> io::Result<()> {
        let fd = socket.as_raw_fd();
        let on: libc::c_int = 1;
        let optlen = socklen(std::mem::size_of_val(&on))?;
        // SAFETY: `fd` is a valid open socket for the call's duration (borrowed
        // from `socket`); `&on` points to a live `c_int` of `optlen` bytes; the
        // option level/name are valid IPv6 socket options. No memory is retained
        // past the call.
        let rc6 = unsafe {
            libc::setsockopt(
                fd,
                libc::IPPROTO_IPV6,
                libc::IPV6_RECVPKTINFO,
                std::ptr::addr_of!(on).cast(),
                optlen,
            )
        };
        if rc6 != 0 {
            return Err(io::Error::last_os_error());
        }
        // IP_PKTINFO is best-effort on a dual-stack v6 socket (the v6 option
        // already covers v4-mapped); ignore an error so a kernel that rejects it
        // on a v6 socket does not fail the bind.
        // SAFETY: identical contract to the IPv6 call above.
        let _ = unsafe {
            libc::setsockopt(
                fd,
                libc::IPPROTO_IP,
                libc::IP_PKTINFO,
                std::ptr::addr_of!(on).cast(),
                optlen,
            )
        };
        Ok(())
    }

    /// Convert a `usize` byte length to the platform `socklen_t`, erroring rather
    /// than truncating (`as_conversions` is denied; a length this small never
    /// overflows, but the conversion stays checked).
    fn socklen(len: usize) -> io::Result<libc::socklen_t> {
        libc::socklen_t::try_from(len)
            .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "socklen overflow"))
    }

    /// Receive one datagram, returning `(len, source, local_destination)` where
    /// `local_destination` is the **concrete** local address the datagram landed
    /// on (recovered from the `IPV6_PKTINFO` / `IP_PKTINFO` cmsg), carrying the
    /// socket's bound port. When the kernel reports no PKTINFO cmsg (it should,
    /// after [`enable_pktinfo`]), the destination falls back to the bound
    /// `local_addr` so the caller's resolver still maps it onto a candidate.
    ///
    /// Non-blocking: a `WouldBlock` surfaces verbatim so the driver parks on its
    /// timer rather than busy-spinning.
    ///
    /// # Errors
    ///
    /// The underlying `recvmsg(2)` error (including `WouldBlock`).
    pub(crate) fn recv_from_with_local(
        socket: &Socket,
        buf: &mut [u8],
        local_addr: SocketAddr,
    ) -> io::Result<(usize, SocketAddr, SocketAddr)> {
        // A control buffer large enough for one PKTINFO cmsg (v6 is the larger:
        // cmsghdr + in6_pktinfo). 128 bytes covers both families with headroom.
        let mut control = [MaybeUninit::<u8>::uninit(); 128];
        // SAFETY: a zeroed sockaddr storage with its full length is a valid empty
        // `SockAddr` for the kernel to fill with the source address; `recvmsg`
        // writes the concrete `msg_name` (length is clamped by the kernel).
        let mut src_addr = unsafe {
            SockAddr::new(
                SockAddrStorage::zeroed(),
                socklen(std::mem::size_of::<libc::sockaddr_storage>())?,
            )
        };
        // SAFETY: `buf` is initialised (`&mut [u8]`); the slice covers `buf`'s
        // bytes as `MaybeUninit<u8>` with the same length and provenance, and
        // `recvmsg` only ever writes initialised bytes into it (the socket2
        // contract for `recv_vectored`/`recvmsg`).
        let mut iov = [unsafe {
            MaybeUninitSlice::new(std::slice::from_raw_parts_mut(
                buf.as_mut_ptr().cast::<MaybeUninit<u8>>(),
                buf.len(),
            ))
        }];

        let (len, control_len) = {
            let mut msg = MsgHdrMut::new()
                .with_addr(&mut src_addr)
                .with_buffers(&mut iov)
                .with_control(&mut control);
            let len = socket.recvmsg(&mut msg, 0)?;
            (len, msg.control_len())
        };
        // `msg`'s mutable borrows of `src_addr` / `control` have ended; read them.

        let source = src_addr
            .as_socket()
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "recvmsg: no source addr"))?;
        let local = parse_pktinfo(&control, control_len, local_addr).unwrap_or(local_addr);
        Ok((len, source, local))
    }

    /// Parse the `IPV6_PKTINFO` / `IP_PKTINFO` destination IP out of a cmsg
    /// control buffer, returning a `SocketAddr` on the bound `port`. `None` when
    /// no PKTINFO cmsg is present.
    fn parse_pktinfo(
        control: &[MaybeUninit<u8>],
        control_len: usize,
        local_addr: SocketAddr,
    ) -> Option<SocketAddr> {
        let port = local_addr.port();
        // SAFETY: `control[..control_len]` was written by the kernel as a sequence
        // of cmsghdr-aligned ancillary-data blocks; we only read within
        // `control_len` and never dereference past a parsed length.
        let base = control.as_ptr().cast::<u8>();
        let mut offset = 0usize;
        let hdr_size = std::mem::size_of::<libc::cmsghdr>();
        while offset + hdr_size <= control_len {
            // SAFETY: `base + offset` is within the control buffer and aligned for
            // a cmsghdr (the kernel writes cmsghdr-aligned blocks); we read one.
            let cmsg =
                unsafe { std::ptr::read_unaligned(base.add(offset).cast::<libc::cmsghdr>()) };
            // `cmsg_len` is `usize` on Linux but `socklen_t` on some BSDs; convert
            // tolerantly (a value that cannot fit `usize` is malformed — stop).
            #[allow(
                clippy::useless_conversion,
                reason = "cmsg_len is usize on Linux but socklen_t elsewhere; the \
                          conversion is a no-op here but keeps the parser portable"
            )]
            let cmsg_len = usize::try_from(cmsg.cmsg_len).unwrap_or(0);
            if cmsg_len < hdr_size || offset + cmsg_len > control_len {
                break;
            }
            let data_offset = offset + cmsg_align(hdr_size);
            if cmsg.cmsg_level == libc::IPPROTO_IPV6 && cmsg.cmsg_type == libc::IPV6_PKTINFO {
                let need = std::mem::size_of::<libc::in6_pktinfo>();
                if data_offset + need <= control_len {
                    // SAFETY: the kernel placed an `in6_pktinfo` here for an
                    // IPV6_PKTINFO cmsg; we read it unaligned within bounds.
                    let pi = unsafe {
                        std::ptr::read_unaligned(base.add(data_offset).cast::<libc::in6_pktinfo>())
                    };
                    return Some(v6_dest(pi.ipi6_addr.s6_addr, port));
                }
            } else if cmsg.cmsg_level == libc::IPPROTO_IP && cmsg.cmsg_type == libc::IP_PKTINFO {
                let need = std::mem::size_of::<libc::in_pktinfo>();
                if data_offset + need <= control_len {
                    // SAFETY: the kernel placed an `in_pktinfo` here for an
                    // IP_PKTINFO cmsg; we read it unaligned within bounds.
                    let pi = unsafe {
                        std::ptr::read_unaligned(base.add(data_offset).cast::<libc::in_pktinfo>())
                    };
                    // `s_addr` is in network byte order; `Ipv4Addr::from(u32)`
                    // takes host-order, so flip from big-endian.
                    let host_order = u32::from_be(pi.ipi_addr.s_addr);
                    return Some(SocketAddr::new(
                        IpAddr::V4(Ipv4Addr::from(host_order)),
                        port,
                    ));
                }
            }
            offset += cmsg_align(cmsg_len);
        }
        None
    }

    /// Align a cmsg length up to the platform's cmsg alignment (`CMSG_ALIGN`).
    const fn cmsg_align(len: usize) -> usize {
        let align = std::mem::align_of::<libc::cmsghdr>();
        (len + align - 1) & !(align - 1)
    }

    /// Build a `SocketAddr` from a 16-byte IPv6 destination, un-mapping an
    /// IPv4-mapped `::ffff:a.b.c.d` to a real IPv4 addr so it matches a gathered
    /// IPv4 candidate (dual-stack reports v4 peers as v4-mapped).
    fn v6_dest(addr: [u8; 16], port: u16) -> SocketAddr {
        let v6 = Ipv6Addr::from(addr);
        if let Some(v4) = v6.to_ipv4_mapped() {
            SocketAddr::new(IpAddr::V4(v4), port)
        } else {
            SocketAddr::new(IpAddr::V6(v6), port)
        }
    }
}
