use std::net::{IpAddr, Ipv4Addr};
use std::time::Duration;

/// One internet link: a physical interface we can bind outbound sockets to.
#[derive(Clone, Debug)]
pub struct Link {
    pub name: String,
    pub index: u32,
    pub ipv4: Ipv4Addr,
    pub gateway: Option<IpAddr>,
    /// Latest health-probe round trip; None until probed.
    pub rtt: Option<Duration>,
}

/// Auto-detect links: up, running, not loopback/tun, has IPv4 and a gateway, not in `exclude`.
/// `only` non-empty = manual mode: use exactly these interface names (gateway not required).
pub fn discover(only: &[String], exclude: &[String]) -> Vec<Link> {
    netdev::get_interfaces()
        .into_iter()
        .filter(|i| i.is_up() && i.is_running() && !i.is_loopback() && !i.is_tun())
        .filter(|i| {
            if only.is_empty() {
                i.gateway.is_some() && !exclude.contains(&i.name)
            } else {
                only.contains(&i.name)
            }
        })
        .filter_map(|i| {
            let gateway = i.gateway.as_ref().and_then(|g| {
                g.ipv4.first().map(|a| IpAddr::V4(*a)).or(g.ipv6.first().map(|a| IpAddr::V6(*a)))
            });
            Some(Link { ipv4: i.ipv4.first()?.addr(), name: i.name, index: i.index, gateway, rtt: None })
        })
        .collect()
}
