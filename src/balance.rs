use crate::config::Pin;
use crate::links::Link;
use ipnet::IpNet;
use std::collections::HashMap;
use std::hash::{DefaultHasher, Hash, Hasher};
use std::net::SocketAddr;
use std::sync::RwLock;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

#[derive(Clone, Copy, Debug, Default, PartialEq, clap::ValueEnum, serde::Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Policy {
    /// Same destination IP always uses the same link (keeps sites' IP-bound sessions alive).
    #[default]
    Sticky,
    /// Each new connection goes to the next link in turn.
    RoundRobin,
    /// Everything goes to the link with the lowest probe latency; the rest are standby.
    LeastLatency,
}

/// Everything that decides a pick. Replaced as a whole while running (see `Balancer::set`);
/// flows already relaying keep their link.
struct Rules {
    policy: Policy,
    weights: HashMap<String, f64>,
    /// Most specific range first.
    pins: Vec<(IpNet, String)>,
}

pub struct Balancer {
    rules: RwLock<Rules>,
    next: AtomicUsize,
}

impl Balancer {
    pub fn new(policy: Policy, weights: HashMap<String, f64>, pins: Vec<Pin>) -> Self {
        Self { rules: RwLock::new(Rules::new(policy, weights, pins)), next: AtomicUsize::new(0) }
    }

    /// Switch the rules for new flows.
    pub fn set(&self, policy: Policy, weights: HashMap<String, f64>, pins: Vec<Pin>) {
        *self.rules.write().unwrap() = Rules::new(policy, weights, pins);
    }

    /// The policy's command-line name, e.g. "least-latency".
    pub fn policy_name(&self) -> &'static str {
        match self.rules.read().unwrap().policy {
            Policy::Sticky => "sticky",
            Policy::RoundRobin => "round-robin",
            Policy::LeastLatency => "least-latency",
        }
    }

    /// Pick a link for a new flow to `dst` from the currently healthy `links`.
    /// Precedence: a pin whose link is healthy, then DNS to the fastest link, then the policy.
    pub fn pick<'a>(&self, dst: SocketAddr, links: &'a [Link]) -> Option<&'a Link> {
        self.rules.read().unwrap().pick(dst, links, &self.next)
    }
}

impl Rules {
    fn new(policy: Policy, weights: HashMap<String, f64>, pins: Vec<Pin>) -> Self {
        let mut pins: Vec<_> = pins.into_iter().map(|p| (p.dst, p.link)).collect();
        pins.sort_by_key(|(net, _)| std::cmp::Reverse(net.prefix_len()));
        Self { policy, weights, pins }
    }

    fn pick<'a>(&self, dst: SocketAddr, links: &'a [Link], next: &AtomicUsize) -> Option<&'a Link> {
        if let Some(l) = self.pinned(dst, links) {
            return Some(l);
        }
        let fastest = || links.iter().min_by_key(|l| l.rtt.unwrap_or(Duration::MAX));
        // DNS has no session to keep and every page load waits on it.
        // ponytail: latest single probe RTT; smooth it (EWMA) if similar links make it flap.
        if dst.port() == 53 {
            return fastest();
        }
        match self.policy {
            // Weighted rendezvous hashing: stable per destination, shares follow the weights,
            // and when a link drops only the flows on that link move.
            Policy::Sticky => links.iter().max_by(|a, b| self.score(dst, a).total_cmp(&self.score(dst, b))),
            Policy::RoundRobin => {
                if links.is_empty() {
                    return None;
                }
                links.get(next.fetch_add(1, Ordering::Relaxed) % links.len())
            }
            Policy::LeastLatency => fastest(),
        }
    }

    fn pinned<'a>(&self, dst: SocketAddr, links: &'a [Link]) -> Option<&'a Link> {
        let (_, name) = self.pins.iter().find(|(net, _)| net.contains(&dst.ip()))?;
        links.iter().find(|l| l.name == *name)
    }

    fn score(&self, dst: SocketAddr, link: &Link) -> f64 {
        let mut h = DefaultHasher::new();
        (dst.ip(), &link.name).hash(&mut h);
        let u = ((h.finish() >> 11) as f64 + 0.5) / (1u64 << 53) as f64; // uniform in (0, 1)
        self.weights.get(&link.name).copied().unwrap_or(1.0) / -u.ln()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::IpAddr;

    fn link(name: &str, ms: Option<u64>) -> Link {
        Link {
            name: name.into(),
            index: 0,
            ipv4: [0, 0, 0, 0].into(),
            gateway: None,
            rtt: ms.map(Duration::from_millis),
        }
    }

    fn dst(i: u32, port: u16) -> SocketAddr {
        SocketAddr::new(IpAddr::V4(i.into()), port)
    }

    fn name(b: &Balancer, d: SocketAddr, links: &[Link]) -> String {
        b.pick(d, links).unwrap().name.clone()
    }

    fn plain(policy: Policy) -> Balancer {
        Balancer::new(policy, HashMap::new(), vec![])
    }

    #[test]
    fn sticky_is_stable_spreads_and_only_moves_dead_links_flows() {
        let all = [link("en0", None), link("en1", None), link("en2", None)];
        let b = plain(Policy::Sticky);
        let before: Vec<_> = (0..300).map(|i| name(&b, dst(i, 443), &all)).collect();
        assert!((0..300).all(|i| name(&b, dst(i, 443), &all) == before[i as usize]));
        for l in &all {
            assert!(before.iter().filter(|n| **n == l.name).count() > 60, "{} starved", l.name);
        }
        let alive = [link("en0", None), link("en2", None)];
        for i in 0..300 {
            if before[i as usize] != "en1" {
                assert_eq!(name(&b, dst(i, 443), &alive), before[i as usize]);
            }
        }
        assert!(b.pick(dst(1, 443), &[]).is_none());
    }

    #[test]
    fn sticky_weights_set_the_share() {
        let all = [link("a", None), link("b", None)];
        let b = Balancer::new(Policy::Sticky, HashMap::from([("a".into(), 3.0)]), vec![]);
        let a = (0..4000).filter(|i| name(&b, dst(*i, 443), &all) == "a").count();
        assert!((2800..3200).contains(&a), "a got {a} of 4000, want about 3000");
    }

    #[test]
    fn round_robin_cycles_least_latency_picks_fastest() {
        let all = [link("a", Some(50)), link("b", Some(5)), link("c", None)];
        let rr = plain(Policy::RoundRobin);
        let got: Vec<_> = (0..4).map(|_| name(&rr, dst(1, 443), &all)).collect();
        assert_eq!(got, ["a", "b", "c", "a"]);
        let ll = plain(Policy::LeastLatency);
        assert!((0..20).all(|i| name(&ll, dst(i, 443), &all) == "b"));
    }

    #[test]
    fn precedence_pin_then_dns_then_policy() {
        let all = [link("slow", Some(300)), link("fast", Some(5))];
        let pins = vec![
            Pin { dst: "10.0.0.0/8".parse().unwrap(), link: "fast".into() },
            Pin { dst: "10.1.0.0/16".parse().unwrap(), link: "slow".into() },
            Pin { dst: "11.0.0.0/8".parse().unwrap(), link: "gone".into() },
        ];
        let b = Balancer::new(Policy::RoundRobin, HashMap::new(), pins);
        let at = |s: &str| s.parse::<SocketAddr>().unwrap();
        assert_eq!(name(&b, at("10.1.2.3:443"), &all), "slow"); // most specific pin wins
        assert_eq!(name(&b, at("10.9.9.9:53"), &all), "fast"); // pin beats DNS rule
        assert_eq!(name(&b, at("1.1.1.2:53"), &all), "fast"); // DNS: fastest
        assert_eq!(name(&b, at("1.1.1.2:53"), &all), "fast"); // ...not rotated
        // pinned link not healthy: normal policy (round robin starts at the first link)
        assert_eq!(name(&b, at("11.0.0.1:443"), &all), "slow");
        assert_eq!(name(&b, at("11.0.0.1:443"), &all), "fast");
    }

    #[test]
    fn set_switches_rules_for_new_picks() {
        let all = [link("a", Some(50)), link("b", Some(5))];
        let b = plain(Policy::RoundRobin);
        b.set(Policy::LeastLatency, HashMap::new(), vec![]);
        assert_eq!(b.policy_name(), "least-latency");
        assert!((0..10).all(|i| name(&b, dst(i, 443), &all) == "b"));
        b.set(Policy::LeastLatency, HashMap::new(), vec![Pin { dst: "10.0.0.0/8".parse().unwrap(), link: "a".into() }]);
        assert_eq!(name(&b, dst(0x0a00_0001, 443), &all), "a");
    }
}
