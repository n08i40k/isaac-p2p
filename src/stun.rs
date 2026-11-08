//! Minimal STUN (RFC 5389) binding requests: the transport socket asks a STUN server which public address and
//! port its datagrams come from.

use std::net::{Ipv4Addr, SocketAddrV4, ToSocketAddrs};

const BINDING_REQUEST: u16 = 0x0001;
const BINDING_SUCCESS: u16 = 0x0101;
const MAGIC_COOKIE: u32 = 0x2112_A442;
const MAPPED_ADDRESS: u16 = 0x0001;
const XOR_MAPPED_ADDRESS: u16 = 0x0020;

/// Two operators, so the two answers come from different IPs (needed to tell a symmetric NAT).
pub const DEFAULT_SERVERS: &str = "stun.l.google.com:19302,stun.cloudflare.com:3478";

pub fn request(transaction: &[u8; 12]) -> [u8; 20] {
    let mut out = [0u8; 20];
    out[..2].copy_from_slice(&BINDING_REQUEST.to_be_bytes());
    out[4..8].copy_from_slice(&MAGIC_COOKIE.to_be_bytes());
    out[8..].copy_from_slice(transaction);
    out
}

/// Whether a datagram looks like a STUN binding response (and not transport traffic).
pub fn is_response(data: &[u8]) -> bool {
    data.len() >= 20
        && u16::from_be_bytes([data[0], data[1]]) == BINDING_SUCCESS
        && u32::from_be_bytes(data[4..8].try_into().unwrap()) == MAGIC_COOKIE
}

/// Transaction ID and mapped address of a binding response.
pub fn parse_response(data: &[u8]) -> Option<([u8; 12], SocketAddrV4)> {
    if !is_response(data) {
        return None;
    }
    let transaction: [u8; 12] = data[8..20].try_into().unwrap();
    let length = (u16::from_be_bytes([data[2], data[3]]) as usize).min(data.len() - 20);
    let attributes = &data[20..20 + length];
    let mut plain = None;
    let mut at = 0;
    while at + 4 <= attributes.len() {
        let kind = u16::from_be_bytes([attributes[at], attributes[at + 1]]);
        let size = u16::from_be_bytes([attributes[at + 2], attributes[at + 3]]) as usize;
        let value = attributes.get(at + 4..at + 4 + size)?;
        if (kind == XOR_MAPPED_ADDRESS || kind == MAPPED_ADDRESS) && size >= 8 && value[1] == 1 {
            let mut port = u16::from_be_bytes([value[2], value[3]]);
            let mut ip = u32::from_be_bytes(value[4..8].try_into().unwrap());
            if kind == XOR_MAPPED_ADDRESS {
                port ^= (MAGIC_COOKIE >> 16) as u16;
                ip ^= MAGIC_COOKIE;
                return Some((transaction, SocketAddrV4::new(Ipv4Addr::from(ip), port)));
            }
            plain = Some(SocketAddrV4::new(Ipv4Addr::from(ip), port));
        }
        at += 4 + size.div_ceil(4) * 4;
    }
    plain.map(|addr| (transaction, addr))
}

/// Resolves "host:port" entries (comma separated, "stun:" prefixes allowed) to distinct IPv4 addresses.
pub fn resolve_servers(list: &str) -> Vec<SocketAddrV4> {
    let mut out: Vec<SocketAddrV4> = Vec::new();
    let resolved = list
        .split(',')
        .map(|entry| entry.trim().trim_start_matches("stun:"))
        .filter(|entry| !entry.is_empty())
        .filter_map(|entry| entry.to_socket_addrs().ok()?.find_map(|a| match a {
            std::net::SocketAddr::V4(a) => Some(a),
            std::net::SocketAddr::V6(_) => None,
        }));
    for addr in resolved {
        if !out.iter().any(|a| a.ip() == addr.ip()) {
            out.push(addr);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_xor_mapped_address() {
        let transaction = [9u8; 12];
        let mut response = request(&transaction);
        response[..2].copy_from_slice(&BINDING_SUCCESS.to_be_bytes());
        let mut data = response.to_vec();
        data[2..4].copy_from_slice(&12u16.to_be_bytes());
        let port = 27777u16 ^ 0x2112;
        let ip = u32::from(Ipv4Addr::new(94, 180, 38, 120)) ^ MAGIC_COOKIE;
        data.extend_from_slice(&XOR_MAPPED_ADDRESS.to_be_bytes());
        data.extend_from_slice(&8u16.to_be_bytes());
        data.extend_from_slice(&[0, 1]);
        data.extend_from_slice(&port.to_be_bytes());
        data.extend_from_slice(&ip.to_be_bytes());
        let (id, addr) = parse_response(&data).unwrap();
        assert_eq!(id, transaction);
        assert_eq!(addr, "94.180.38.120:27777".parse().unwrap());
        assert!(!is_response(&[0x49, 1, 0, 0]));
    }
}
