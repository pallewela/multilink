use std::net::{Ipv4Addr, Ipv6Addr};
use std::time::Duration;

/// One internet link: a physical interface we can bind outbound sockets to.
#[derive(Clone, Debug)]
pub struct Link {
    pub name: String,
    pub index: u32,
    pub ipv4: Ipv4Addr,
    pub gateway: Option<Ipv4Addr>,
    pub gateway6: Option<Ipv6Addr>,
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
            if only.is_empty() { i.gateway.is_some() && !exclude.contains(&i.name) } else { only.contains(&i.name) }
        })
        .filter_map(|i| {
            let gw = i.gateway.as_ref();
            Some(Link {
                ipv4: i.ipv4.first()?.addr(),
                gateway: gw.and_then(|g| g.ipv4.first().copied()),
                gateway6: gw.and_then(|g| g.ipv6.first().copied()),
                name: i.name,
                index: i.index,
                rtt: None,
            })
        })
        .collect()
}
