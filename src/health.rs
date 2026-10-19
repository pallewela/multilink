use crate::{dial, links::Link};
use std::collections::HashMap;
use std::net::SocketAddr;
use std::time::{Duration, Instant};
use tokio::time::timeout;

/// Consecutive failed probes before a link leaves rotation. One success brings it back.
const FAILS_TO_DOWN: u32 = 2;
const PROBE_TIMEOUT: Duration = Duration::from_secs(2);

/// TCP-connect to `target` out of every link in parallel. `Some(rtt)` = reachable.
pub async fn probe_all(links: &[Link], target: SocketAddr) -> Vec<(Link, Option<Duration>)> {
    let mut set = tokio::task::JoinSet::new();
    for l in links.iter().cloned() {
        set.spawn(async move {
            let t = Instant::now();
            let ok = matches!(timeout(PROBE_TIMEOUT, dial::connect(target, &l)).await, Ok(Ok(_)));
            (l, ok.then(|| t.elapsed()))
        });
    }
    set.join_all().await
}

/// Consecutive probe failures per link name.
#[derive(Default)]
pub struct Health(HashMap<String, u32>);

impl Health {
    /// Record one probe round. Returns the links that are up, and state-change messages.
    /// A link seen for the first time counts as down until it passes a probe.
    pub fn update(&mut self, results: &[(Link, Option<Duration>)]) -> (Vec<Link>, Vec<String>) {
        let mut events = Vec::new();
        // Interface vanished (unplugged, hotspot off): no probe can fail, so report it here.
        self.0.retain(|name, fails| {
            let present = results.iter().any(|(l, _)| l.name == *name);
            if !present && *fails < FAILS_TO_DOWN {
                events.push(format!("link {name} down (interface gone)"));
            }
            present
        });
        let mut up = Vec::new();
        for (link, rtt) in results {
            let fails = self.0.entry(link.name.clone()).or_insert(FAILS_TO_DOWN);
            let was_up = *fails < FAILS_TO_DOWN;
            *fails = match rtt {
                Some(_) => 0,
                None => fails.saturating_add(1),
            };
            let is_up = *fails < FAILS_TO_DOWN;
            match (was_up, is_up, rtt) {
                (false, true, Some(rtt)) => events.push(format!("link {} up ({} ms)", link.name, rtt.as_millis())),
                (true, false, _) => events.push(format!("link {} down", link.name)),
                _ => {}
            }
            if is_up {
                up.push(Link { rtt: *rtt, ..link.clone() });
            }
        }
        up.sort_by(|a, b| a.name.cmp(&b.name));
        events.sort();
        (up, events)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn link(name: &str) -> Link {
        Link { name: name.into(), index: 0, ipv4: [0, 0, 0, 0].into(), gateway: None, gateway6: None, rtt: None }
    }

    /// Up link names, then events.
    fn round(h: &mut Health, r: &[(&str, bool)]) -> (Vec<String>, Vec<String>) {
        let r: Vec<_> = r.iter().map(|(n, ok)| (link(n), ok.then_some(Duration::from_millis(5)))).collect();
        let (up, events) = h.update(&r);
        (up.into_iter().map(|l| l.name).collect(), events)
    }

    #[test]
    fn down_after_two_failures_up_after_one_success() {
        let mut h = Health::default();
        // new links start down: a failing newcomer never joins
        assert_eq!(round(&mut h, &[("a", true), ("b", false)]), (vec!["a".into()], vec!["link a up (5 ms)".into()]));
        // one blip tolerated
        assert_eq!(
            round(&mut h, &[("a", false), ("b", true)]),
            (vec!["a".into(), "b".into()], vec!["link b up (5 ms)".into()])
        );
        // second failure: down
        assert_eq!(round(&mut h, &[("a", false), ("b", true)]), (vec!["b".into()], vec!["link a down".into()]));
        // one success: back
        assert_eq!(round(&mut h, &[("a", true), ("b", true)]).1, ["link a up (5 ms)"]);
        // an up link whose interface vanishes is reported down
        assert_eq!(round(&mut h, &[("b", true)]), (vec!["b".into()], vec!["link a down (interface gone)".into()]));
        // it starts down when it reappears, and a link already down vanishes silently
        assert_eq!(round(&mut h, &[("a", false), ("b", true)]), (vec!["b".into()], vec![]));
        assert_eq!(round(&mut h, &[("b", true)]).1, Vec::<String>::new());
    }
}
