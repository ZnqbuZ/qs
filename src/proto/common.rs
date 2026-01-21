use std::fmt;
use std::fmt::{Debug, Display};

include!(concat!(env!("OUT_DIR"), "/common.rs"));

impl From<std::net::Ipv4Addr> for Ipv4Addr {
    fn from(value: std::net::Ipv4Addr) -> Self {
        Self {
            addr: u32::from_be_bytes(value.octets()),
        }
    }
}

impl From<Ipv4Addr> for std::net::Ipv4Addr {
    fn from(value: Ipv4Addr) -> Self {
        std::net::Ipv4Addr::from(value.addr)
    }
}

impl Display for Ipv4Addr {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", std::net::Ipv4Addr::from(self.addr))
    }
}

impl Debug for Ipv4Addr {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        let std_ipv4_addr = std::net::Ipv4Addr::from(*self);
        write!(f, "{}", std_ipv4_addr)
    }
}

impl From<std::net::Ipv6Addr> for Ipv6Addr {
    fn from(value: std::net::Ipv6Addr) -> Self {
        let b = value.octets();
        Self {
            part1: u32::from_be_bytes([b[0], b[1], b[2], b[3]]),
            part2: u32::from_be_bytes([b[4], b[5], b[6], b[7]]),
            part3: u32::from_be_bytes([b[8], b[9], b[10], b[11]]),
            part4: u32::from_be_bytes([b[12], b[13], b[14], b[15]]),
        }
    }
}

impl From<Ipv6Addr> for std::net::Ipv6Addr {
    fn from(value: Ipv6Addr) -> Self {
        let part1 = value.part1.to_be_bytes();
        let part2 = value.part2.to_be_bytes();
        let part3 = value.part3.to_be_bytes();
        let part4 = value.part4.to_be_bytes();
        std::net::Ipv6Addr::from([
            part1[0], part1[1], part1[2], part1[3], part2[0], part2[1], part2[2], part2[3],
            part3[0], part3[1], part3[2], part3[3], part4[0], part4[1], part4[2], part4[3],
        ])
    }
}

impl Display for Ipv6Addr {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", std::net::Ipv6Addr::from(*self))
    }
}

impl Debug for Ipv6Addr {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        let std_ipv6_addr = std::net::Ipv6Addr::from(*self);
        write!(f, "{}", std_ipv6_addr)
    }
}

impl From<std::net::SocketAddr> for SocketAddr {
    fn from(value: std::net::SocketAddr) -> Self {
        match value {
            std::net::SocketAddr::V4(v4) => SocketAddr {
                ip: Some(socket_addr::Ip::Ipv4((*v4.ip()).into())),
                port: v4.port() as u32,
            },
            std::net::SocketAddr::V6(v6) => SocketAddr {
                ip: Some(socket_addr::Ip::Ipv6((*v6.ip()).into())),
                port: v6.port() as u32,
            },
        }
    }
}

impl From<SocketAddr> for std::net::SocketAddr {
    fn from(value: SocketAddr) -> Self {
        if value.ip.is_none() {
            return "0.0.0.0:0".parse().unwrap();
        }
        match value.ip.unwrap() {
            socket_addr::Ip::Ipv4(ip) => std::net::SocketAddr::V4(std::net::SocketAddrV4::new(
                std::net::Ipv4Addr::from(ip),
                value.port as u16,
            )),
            socket_addr::Ip::Ipv6(ip) => std::net::SocketAddr::V6(std::net::SocketAddrV6::new(
                std::net::Ipv6Addr::from(ip),
                value.port as u16,
                0,
                0,
            )),
        }
    }
}

impl Display for SocketAddr {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", std::net::SocketAddr::from(*self))
    }
}
