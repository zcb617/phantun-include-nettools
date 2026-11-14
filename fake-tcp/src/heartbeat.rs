use crate::packet;
use bytes::Bytes;
use pnet::packet::{Packet, tcp};
use std::net::SocketAddr;

/// 心跳编号的高位标识，保证发送窗口不小于 32768。
pub(crate) const MIN_TICKET: u16 = 0x8000;

/// 构造携带编号且没有业务数据的心跳报文。
pub(crate) fn build(
    local: SocketAddr,
    remote: SocketAddr,
    seq: u32,
    ack: u32,
    ticket: u16,
) -> Bytes {
    // 用窗口保存编号，保持业务载荷为空。
    assert!(ticket >= MIN_TICKET);
    packet::build_tcp_packet_with_window(
        local,
        remote,
        seq,
        ack,
        tcp::TcpFlags::ACK | tcp::TcpFlags::PSH,
        None,
        ticket,
    )
}

/// 识别专用心跳报文并返回编号。
pub(crate) fn decode(packet: &tcp::TcpPacket<'_>) -> Option<u16> {
    // 同时检查标志、空载荷和编号标识。
    if packet.get_flags() == (tcp::TcpFlags::ACK | tcp::TcpFlags::PSH)
        && packet.payload().is_empty()
        && packet.get_window() >= MIN_TICKET
    {
        Some(packet.get_window())
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::packet::{build_tcp_packet, build_tcp_packet_with_window, parse_ip_packet};
    use pnet::packet::{ipv4, ipv6};

    /// 验证 IPv4 和 IPv6 心跳编号及 TCP 校验和。
    #[test]
    fn heartbeat_addresses_and_checksum() {
        // 分别构造两种地址族的报文，校验编号及伪首部校验和。
        for (local, remote) in [
            ("192.0.2.1:1000", "192.0.2.2:2000"),
            ("[2001:db8::1]:1000", "[2001:db8::2]:2000"),
        ] {
            let local: SocketAddr = local.parse().unwrap();
            let remote: SocketAddr = remote.parse().unwrap();
            let raw = build(local, remote, 12, 34, 0x8123);
            let (_, tcp) = parse_ip_packet(&raw).unwrap();
            assert_eq!(decode(&tcp), Some(0x8123));
            match (local, remote) {
                (SocketAddr::V4(a), SocketAddr::V4(b)) => {
                    let ip = ipv4::Ipv4Packet::new(&raw).unwrap();
                    assert_eq!(ipv4::checksum(&ip), ip.get_checksum());
                    assert_eq!(tcp::ipv4_checksum(&tcp, a.ip(), b.ip()), tcp.get_checksum());
                }
                (SocketAddr::V6(a), SocketAddr::V6(b)) => {
                    assert!(ipv6::Ipv6Packet::new(&raw).is_some());
                    assert_eq!(tcp::ipv6_checksum(&tcp, a.ip(), b.ip()), tcp.get_checksum());
                }
                _ => unreachable!(),
            }
        }
    }

    /// 验证普通数据、旧保活及非法心跳不会被识别。
    #[test]
    fn rejects_non_heartbeat_packets() {
        // 覆盖 ACK 保活、普通数据、非空 PSH 及低窗口。
        let local = "192.0.2.1:1000".parse().unwrap();
        let remote = "192.0.2.2:2000".parse().unwrap();
        let flags = tcp::TcpFlags::ACK | tcp::TcpFlags::PSH;
        for raw in [
            build_tcp_packet(local, remote, 1, 1, tcp::TcpFlags::ACK, None),
            build_tcp_packet(local, remote, 1, 1, tcp::TcpFlags::ACK, Some(&[1])),
            build_tcp_packet(local, remote, 1, 1, flags, Some(&[1])),
            build_tcp_packet_with_window(local, remote, 1, 1, flags, None, 0x7fff),
        ] {
            assert_eq!(decode(&parse_ip_packet(&raw).unwrap().1), None);
        }
    }
}
