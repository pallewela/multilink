use crate::links::Link;
use socket2::{Domain, Protocol, Socket, Type};
use std::{io, net::SocketAddr};
use tokio::net::{TcpSocket, TcpStream};

/// TCP connect to `dst` forced out of `link`'s interface, bypassing the routing table's choice.
pub async fn connect(dst: SocketAddr, link: &Link) -> io::Result<TcpStream> {
    let sock = Socket::new(Domain::for_address(dst), Type::STREAM, Some(Protocol::TCP))?;
    bind_to(&sock, link, dst.is_ipv4())?;
    sock.set_nonblocking(true)?;
    TcpSocket::from_std_stream(sock.into()).connect(dst).await
}

#[cfg(target_vendor = "apple")]
fn bind_to(sock: &Socket, link: &Link, v4: bool) -> io::Result<()> {
    let idx = std::num::NonZeroU32::new(link.index);
    if v4 { sock.bind_device_by_index_v4(idx) } else { sock.bind_device_by_index_v6(idx) }
}

#[cfg(target_os = "linux")]
fn bind_to(sock: &Socket, link: &Link, _v4: bool) -> io::Result<()> {
    sock.bind_device(Some(link.name.as_bytes()))
}

#[cfg(windows)]
fn bind_to(sock: &Socket, link: &Link, v4: bool) -> io::Result<()> {
    use std::os::windows::io::AsRawSocket;
    use windows_sys::Win32::Networking::WinSock::*;
    // IP_UNICAST_IF takes the index in network byte order; IPV6_UNICAST_IF in host order.
    let (level, opt, val) = if v4 {
        (IPPROTO_IP, IP_UNICAST_IF, link.index.to_be())
    } else {
        (IPPROTO_IPV6, IPV6_UNICAST_IF, link.index)
    };
    let rc = unsafe {
        setsockopt(sock.as_raw_socket() as SOCKET, level, opt, &val as *const u32 as *const u8, 4)
    };
    if rc == 0 { Ok(()) } else { Err(io::Error::last_os_error()) }
}

/// UDP socket "connected" to `dst`, forced out of `link`'s interface.
pub async fn udp(dst: SocketAddr, link: &Link) -> io::Result<tokio::net::UdpSocket> {
    let sock = Socket::new(Domain::for_address(dst), Type::DGRAM, Some(Protocol::UDP))?;
    bind_to(&sock, link, dst.is_ipv4())?;
    sock.set_nonblocking(true)?;
    let s = tokio::net::UdpSocket::from_std(sock.into())?;
    s.connect(dst).await?;
    Ok(s)
}
