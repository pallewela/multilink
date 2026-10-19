use serde::Serialize;
use std::collections::BTreeMap;
use std::pin::Pin;
use std::sync::atomic::{AtomicI64, AtomicU64, Ordering::Relaxed};
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll, ready};
use std::{io, time::Duration};
use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};

#[derive(Default)]
struct Counters {
    active: AtomicI64,
    flows: AtomicU64,
    up: AtomicU64,
    down: AtomicU64,
}

/// Live per-link traffic counters, shared by every relay.
#[derive(Default)]
pub struct Stats(Mutex<BTreeMap<String, Arc<Counters>>>);

#[derive(Clone, Debug, Default, PartialEq, Serialize)]
pub struct Totals {
    pub active: i64,
    pub flows: u64,
    pub bytes_up: u64,
    pub bytes_down: u64,
}

impl Stats {
    /// Count a new flow on `link`. The guard counts its bytes and ends the flow when dropped.
    pub fn open(&self, link: &str) -> Flow {
        let c = self.0.lock().unwrap().entry(link.into()).or_default().clone();
        c.active.fetch_add(1, Relaxed);
        c.flows.fetch_add(1, Relaxed);
        Flow(c)
    }

    pub fn snapshot(&self) -> BTreeMap<String, Totals> {
        let m = self.0.lock().unwrap();
        m.iter()
            .map(|(name, c)| {
                let t = Totals {
                    active: c.active.load(Relaxed),
                    flows: c.flows.load(Relaxed),
                    bytes_up: c.up.load(Relaxed),
                    bytes_down: c.down.load(Relaxed),
                };
                (name.clone(), t)
            })
            .collect()
    }
}

/// One relayed flow on one link.
pub struct Flow(Arc<Counters>);

impl Flow {
    pub fn up(&self, n: usize) {
        self.0.up.fetch_add(n as u64, Relaxed);
    }
    pub fn down(&self, n: usize) {
        self.0.down.fetch_add(n as u64, Relaxed);
    }
}

impl Drop for Flow {
    fn drop(&mut self) {
        self.0.active.fetch_sub(1, Relaxed);
    }
}

/// The upstream socket of a flow: bytes written to it are "up", bytes read from it "down".
pub struct Counted<S>(pub S, pub Flow);

impl<S: AsyncRead + Unpin> AsyncRead for Counted<S> {
    fn poll_read(mut self: Pin<&mut Self>, cx: &mut Context<'_>, buf: &mut ReadBuf<'_>) -> Poll<io::Result<()>> {
        let before = buf.filled().len();
        ready!(Pin::new(&mut self.0).poll_read(cx, buf))?;
        self.1.down(buf.filled().len() - before);
        Poll::Ready(Ok(()))
    }
}

impl<S: AsyncWrite + Unpin> AsyncWrite for Counted<S> {
    fn poll_write(mut self: Pin<&mut Self>, cx: &mut Context<'_>, buf: &[u8]) -> Poll<io::Result<usize>> {
        let n = ready!(Pin::new(&mut self.0).poll_write(cx, buf))?;
        self.1.up(n);
        Poll::Ready(Ok(n))
    }
    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.0).poll_flush(cx)
    }
    fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.0).poll_shutdown(cx)
    }
}

/// Terminal stats line: per link, active/total flows, rates since `prev`, and total bytes.
pub fn line(now: &BTreeMap<String, Totals>, prev: &BTreeMap<String, Totals>, elapsed: Duration) -> String {
    let secs = elapsed.as_secs_f64().max(0.001);
    now.iter()
        .map(|(name, t)| {
            let p = prev.get(name).cloned().unwrap_or_default();
            let down = (t.bytes_down - p.bytes_down) as f64 / secs;
            let up = (t.bytes_up - p.bytes_up) as f64 / secs;
            format!(
                "{name}: {} active, {} flows, down {}/s up {}/s, {} total",
                t.active,
                t.flows,
                human(down),
                human(up),
                human((t.bytes_down + t.bytes_up) as f64)
            )
        })
        .collect::<Vec<_>>()
        .join(" | ")
}

fn human(b: f64) -> String {
    match b {
        b if b >= 1e9 => format!("{:.1} GB", b / 1e9),
        b if b >= 1e6 => format!("{:.1} MB", b / 1e6),
        b if b >= 1e3 => format!("{:.0} kB", b / 1e3),
        b => format!("{b:.0} B"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    #[tokio::test]
    async fn counts_bytes_live_and_ends_flow_on_drop() {
        let stats = Stats::default();
        let (a, mut b) = tokio::io::duplex(64);
        let mut up = Counted(a, stats.open("en0"));
        up.write_all(b"hello").await.unwrap();
        b.write_all(b"hi").await.unwrap();
        let mut buf = [0u8; 2];
        up.read_exact(&mut buf).await.unwrap();
        // counted while the flow is still open
        let t = &stats.snapshot()["en0"];
        assert_eq!((t.active, t.flows, t.bytes_up, t.bytes_down), (1, 1, 5, 2));
        drop(up);
        let _second = stats.open("en0");
        let t = &stats.snapshot()["en0"];
        assert_eq!((t.active, t.flows, t.bytes_up), (1, 2, 5));
    }

    #[test]
    fn line_shows_rates_since_previous_snapshot() {
        let t = |up, down| Totals { active: 2, flows: 9, bytes_up: up, bytes_down: down };
        let prev = BTreeMap::from([("en0".to_string(), t(0, 1_000_000))]);
        let now = BTreeMap::from([("en0".to_string(), t(4_000, 7_000_000)), ("en9".to_string(), t(10, 20))]);
        assert_eq!(
            line(&now, &prev, Duration::from_secs(2)),
            "en0: 2 active, 9 flows, down 3.0 MB/s up 2 kB/s, 7.0 MB total | en9: 2 active, 9 flows, down 10 B/s up 5 B/s, 30 B total"
        );
    }
}
