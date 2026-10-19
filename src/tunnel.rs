use crate::health::{self, Health};
use crate::stats::{self, Counted, Stats};
use crate::status::{self, LinkView, Shared};
use crate::{balance::Balancer, dial, links::Link};
use ipstack::{IpStack, IpStackConfig, IpStackStream, IpStackTcpStream, IpStackUdpStream};
use route_manager::{Route, RouteManager};
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::pin::Pin;
use std::sync::{Arc, RwLock};
use std::task::{Context, Poll, ready};
use std::{io, time::Duration};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt, ReadBuf};
use tokio::time::timeout;

const MTU: u16 = 1500;
const TUN_ADDR: Ipv4Addr = Ipv4Addr::new(10, 255, 255, 1);
const CONNECT_TIMEOUT: Duration = Duration::from_secs(5);

pub type Links = Arc<RwLock<Vec<Link>>>;

pub struct Opts {
    pub only: Vec<String>,
    pub exclude: Vec<String>,
    pub probe: SocketAddr,
    pub stop_after: Option<Duration>,
    /// Where to serve the status page; None = off.
    pub status: Option<SocketAddr>,
    /// Stop on Ctrl-C, SIGTERM, SIGHUP or console close. Off when embedded: the host process
    /// owns its signals and stops us through `stop`.
    pub signals: bool,
    pub stop: Arc<tokio::sync::Notify>,
}

/// Bring up the TUN, steal the default route with two /1 routes, relay every flow
/// out of a healthy, balanced link until stopped.
pub async fn run(links: Vec<Link>, balancer: Arc<Balancer>, opts: Opts) -> io::Result<()> {
    raise_fd_limit();
    let dev = tun_rs::DeviceBuilder::new().ipv4(TUN_ADDR, 30, None).mtu(MTU).build_async()?;
    let idx = dev.if_index()?;
    let mut cfg = IpStackConfig::default();
    cfg.mtu(MTU).map_err(io::Error::other)?;
    cfg.udp_timeout(Duration::from_secs(30));
    let mut stack = IpStack::new(cfg, Tun(dev));
    // Declared after `stack` so it drops first: routes go before the TUN closes.
    let mut routes = RouteGuard::new()?;
    routes.ensure_scoped(&links);
    routes.capture(idx)?;
    log!("tun up (index {idx}), routes installed; Ctrl-C to stop");
    let stats = Arc::new(Stats::default());
    let shared = Arc::new(Shared {
        stats: stats.clone(),
        links: RwLock::new(Vec::new()),
        balancer: balancer.clone(),
        started: std::time::Instant::now(),
    });
    let links: Links = Arc::new(RwLock::new(links));
    // A status page that can't start (port taken) is worth a warning, not stopping the tunnel.
    let status_page = async {
        if let Some(addr) = opts.status
            && let Err(e) = status::serve(addr, shared.clone()).await
        {
            elog!("status page on {addr} unavailable: {e}");
        }
        std::future::pending::<()>().await
    };

    let accept = async {
        loop {
            let (l, b, s) = (links.clone(), balancer.clone(), stats.clone());
            match stack.accept().await.map_err(io::Error::other)? {
                IpStackStream::Tcp(t) => tokio::spawn(relay_tcp(t, l, b, s)),
                IpStackStream::Udp(u) => tokio::spawn(relay_udp(u, l, b, s)),
                // ponytail: ICMP and other protocols dropped, so ping fails through the tunnel. Add a relay per protocol when needed.
                _ => continue,
            };
        }
    };
    let r = tokio::select! {
        r = accept => r,
        _ = monitor(&links, &opts, &mut routes, &shared) => Ok(()),
        _ = status_page => Ok(()),
        _ = async { if opts.signals { shutdown_signal().await } else { std::future::pending().await } } => Ok(()),
        _ = opts.stop.notified() => Ok(()),
        _ = async { match opts.stop_after { Some(d) => tokio::time::sleep(d).await, None => std::future::pending().await } } => Ok(()),
    };
    log!("stopping");
    r
    // routes drops here (also on error or panic) and removes the routes.
}

const PROBE_EVERY: Duration = Duration::from_secs(2);

/// Every PROBE_EVERY: re-discover links, give new ones a scoped route, probe them all and
/// publish the healthy set to the relays and the status page. Stats line every third round.
async fn monitor(links: &Links, opts: &Opts, routes: &mut RouteGuard, shared: &Shared) {
    let mut health = Health::default();
    let mut tick = tokio::time::interval(PROBE_EVERY);
    let (mut prev, mut prev_at) = (shared.stats.snapshot(), std::time::Instant::now());
    for n in 0u64.. {
        tick.tick().await;
        let found = crate::links::discover(&opts.only, &opts.exclude);
        routes.ensure_scoped(&found);
        let probes = health::probe_all(&found, opts.probe).await;
        let (up, events) = health.update(&probes);
        for e in events {
            log!("{e}");
        }
        let mut views: Vec<_> = probes
            .iter()
            .map(|(l, rtt)| LinkView {
                name: l.name.clone(),
                ipv4: l.ipv4.to_string(),
                gateway: l.gateway.map(|g| g.to_string()),
                up: up.iter().any(|u| u.name == l.name),
                rtt_ms: rtt.map(|d| d.as_secs_f64() * 1e3),
            })
            .collect();
        views.sort_by(|a, b| a.name.cmp(&b.name));
        *shared.links.write().unwrap() = views;
        // Probe target unreachable everywhere (blocked, or down itself): don't black-hole
        // all traffic on its say-so; keep using every link and let per-flow failover sort it out.
        *links.write().unwrap() = if up.is_empty() { found } else { up };
        if n % 3 == 2 {
            let now = shared.stats.snapshot();
            log!("{}", stats::line(&now, &prev, prev_at.elapsed()));
            (prev, prev_at) = (now, std::time::Instant::now());
        }
    }
}

/// Ctrl-C, SIGTERM, SIGHUP (terminal closed), or Windows console close.
async fn shutdown_signal() {
    #[cfg(unix)]
    {
        use tokio::signal::unix::{SignalKind, signal};
        let (Ok(mut term), Ok(mut hup)) = (signal(SignalKind::terminate()), signal(SignalKind::hangup())) else {
            return tokio::signal::ctrl_c().await.unwrap_or(());
        };
        tokio::select! {
            _ = tokio::signal::ctrl_c() => {}
            _ = term.recv() => {}
            _ = hup.recv() => {}
        }
    }
    #[cfg(windows)]
    {
        let Ok(mut close) = tokio::signal::windows::ctrl_close() else {
            return tokio::signal::ctrl_c().await.unwrap_or(());
        };
        tokio::select! {
            _ = tokio::signal::ctrl_c() => {}
            _ = close.recv() => {}
        }
    }
}

/// Links to try for `dst`: the balancer's pick first, then the rest as failover.
fn ordered(links: &Links, b: &Balancer, dst: SocketAddr) -> Vec<Link> {
    let mut all = links.read().unwrap().clone();
    if let Some(first) = b.pick(dst, &all).map(|l| l.name.clone()) {
        all.sort_by_key(|l| l.name != first);
    }
    all
}

async fn relay_tcp(mut tcp: IpStackTcpStream, links: Links, b: Arc<Balancer>, stats: Arc<Stats>) {
    let dst = tcp.peer_addr();
    for link in ordered(&links, &b, dst) {
        let err = match timeout(CONNECT_TIMEOUT, dial::connect(dst, &link)).await {
            Ok(Ok(up)) => {
                let mut up = Counted(up, stats.open(&link.name));
                let _ = tokio::io::copy_bidirectional(&mut tcp, &mut up).await;
                return;
            }
            Ok(Err(e)) => e.to_string(),
            Err(_) => "timeout".into(),
        };
        elog!("tcp {dst} via {}: {err}", link.name);
    }
}

async fn relay_udp(mut udp: IpStackUdpStream, links: Links, b: Arc<Balancer>, stats: Arc<Stats>) {
    let dst = udp.peer_addr();
    let mut picked = None;
    for link in ordered(&links, &b, dst) {
        match dial::udp(dst, &link).await {
            Ok(s) => {
                picked = Some((link, s));
                break;
            }
            Err(e) => elog!("udp {dst} via {}: {e}", link.name),
        }
    }
    let Some((link, sock)) = picked else { return };
    let flow = stats.open(&link.name);
    let (mut up, mut down) = (vec![0u8; 65535], vec![0u8; 65535]);
    loop {
        tokio::select! {
            r = udp.read(&mut up) => match r {
                Ok(k) if k > 0 && sock.send(&up[..k]).await.is_ok() => flow.up(k),
                _ => break, // EOF = ipstack's idle timeout
            },
            r = sock.recv(&mut down) => match r {
                Ok(k) if udp.write_all(&down[..k]).await.is_ok() => {
                    flow.down(k);
                    // DNS is one query, one answer: free the socket now instead of after the idle timeout.
                    if dst.port() == 53 { break }
                }
                _ => break,
            },
        }
    }
}

/// Owns every route we add; deletes them on drop.
/// The /1 routes vanish with the TUN even on a crash. The scoped defaults (macOS) do not; see PLAN.md.
struct RouteGuard {
    rm: RouteManager,
    capture: Vec<Route>,
    /// (interface index, gateway) of scoped defaults we added.
    #[cfg_attr(not(target_os = "macos"), allow(dead_code))]
    scoped: Vec<(u32, IpAddr)>,
}

impl RouteGuard {
    fn new() -> io::Result<Self> {
        Ok(RouteGuard { rm: RouteManager::new()?, capture: Vec::new(), scoped: Vec::new() })
    }

    /// The two /1 routes into the TUN.
    fn capture(&mut self, tun_index: u32) -> io::Result<()> {
        for a in [0, 128] {
            let r = Route::new(IpAddr::V4(Ipv4Addr::new(a, 0, 0, 0)), 1).with_if_index(tun_index);
            self.rm.add(&r)?; // on error, drop removes whatever got added
            self.capture.push(r);
        }
        Ok(())
    }

    /// macOS: a socket bound to an interface (IP_BOUND_IF) whose lookup lands on another
    /// interface only falls back to routes scoped to its own interface. The primary link's
    /// default route is unscoped, so once the /1 routes exist it gets "network unreachable".
    /// Give every link a scoped default. Called every monitor round: links that appear later,
    /// or reconnect and lose their routes, get one again. Existing ones are left alone.
    #[cfg(target_os = "macos")]
    fn ensure_scoped(&mut self, links: &[Link]) {
        for l in links {
            // IPv4 routes only; netdev falls back to an IPv6 gateway when a link has no IPv4 default.
            let Some(gw @ IpAddr::V4(_)) = l.gateway else { continue };
            match self.rm.add(&scoped_default(l.index, gw)) {
                Ok(()) if !self.scoped.contains(&(l.index, gw)) => self.scoped.push((l.index, gw)),
                Ok(()) => {}
                Err(e) if e.kind() == io::ErrorKind::AlreadyExists => {}
                Err(e) => elog!("scoped default for {}: {e}", l.name),
            }
        }
    }

    #[cfg(not(target_os = "macos"))]
    fn ensure_scoped(&mut self, _links: &[Link]) {}
}

/// Interfaces holding an unscoped IPv4 default route.
#[cfg(target_os = "macos")]
fn primary_v4_indexes(rm: &mut RouteManager) -> Vec<u32> {
    rm.list()
        .unwrap_or_default()
        .iter()
        .filter(|r| r.destination().is_ipv4() && r.prefix() == 0 && !r.if_scope())
        .filter_map(|r| r.if_index())
        .collect()
}

#[cfg(target_os = "macos")]
fn scoped_default(index: u32, gw: IpAddr) -> Route {
    Route::new(IpAddr::V4(Ipv4Addr::UNSPECIFIED), 0).with_gateway(gw).with_if_index(index).with_if_scope(true)
}

impl Drop for RouteGuard {
    fn drop(&mut self) {
        for r in &self.capture {
            if let Err(e) = self.rm.delete(r) {
                elog!("route cleanup {r:?}: {e}");
            }
        }
        // A scoped default is redundant only on the primary interface (the one holding the
        // unscoped default). Every other interface needs one, and macOS may have adopted ours
        // when the primary changed mid-run (Wi-Fi off, then on): deleting it there would leave
        // that link without a default route. So only remove ours from the current primary.
        #[cfg(target_os = "macos")]
        let primary = primary_v4_indexes(&mut self.rm);
        #[cfg(target_os = "macos")]
        for &(index, gw) in &self.scoped {
            if primary.contains(&index) {
                let _ = self.rm.delete(&scoped_default(index, gw));
            }
        }
    }
}

/// tun-rs exposes poll_recv/poll_send; ipstack wants AsyncRead/AsyncWrite.
struct Tun(tun_rs::AsyncDevice);

impl AsyncRead for Tun {
    fn poll_read(self: Pin<&mut Self>, cx: &mut Context<'_>, buf: &mut ReadBuf<'_>) -> Poll<io::Result<()>> {
        let n = ready!(self.0.poll_recv(cx, buf.initialize_unfilled()))?;
        buf.advance(n);
        Poll::Ready(Ok(()))
    }
}

impl AsyncWrite for Tun {
    fn poll_write(self: Pin<&mut Self>, cx: &mut Context<'_>, buf: &[u8]) -> Poll<io::Result<usize>> {
        self.0.poll_send(cx, buf)
    }
    fn poll_flush(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<io::Result<()>> {
        Poll::Ready(Ok(()))
    }
    fn poll_shutdown(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<io::Result<()>> {
        Poll::Ready(Ok(()))
    }
}

/// Every relayed flow holds one socket. macOS starts processes at 256 open files, which a
/// browser exhausts in seconds; at that point Apple frameworks used by link discovery crash.
#[cfg(unix)]
fn raise_fd_limit() {
    let mut lim = libc::rlimit { rlim_cur: 0, rlim_max: 0 };
    if unsafe { libc::getrlimit(libc::RLIMIT_NOFILE, &mut lim) } != 0 {
        return;
    }
    if lim.rlim_cur >= 10240 {
        return;
    }
    // macOS rejects values above kern.maxfilesperproc, so step down until one sticks.
    for want in [lim.rlim_max.min(65536), 24576, 10240] {
        let new = libc::rlimit { rlim_cur: want, rlim_max: lim.rlim_max };
        if unsafe { libc::setrlimit(libc::RLIMIT_NOFILE, &new) } == 0 {
            log!("open file limit raised to {want}");
            return;
        }
    }
    elog!("could not raise open file limit above {}", lim.rlim_cur);
}

#[cfg(not(unix))]
fn raise_fd_limit() {}

