use crate::balance::Balancer;
use crate::stats::{Stats, Totals};
use serde::Serialize;
use std::net::SocketAddr;
use std::sync::{Arc, RwLock};
use std::time::{Duration, Instant};
use std::io;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

const PAGE: &str = include_str!("status.html");

/// What the status page shows; the monitor refreshes `links` every probe round.
/// Read-only: control goes through the embedding API ([`crate::Running`]), never over HTTP.
pub struct Shared {
    pub stats: Arc<Stats>,
    pub links: RwLock<Vec<LinkView>>,
    pub balancer: Arc<Balancer>,
    pub started: Instant,
}

#[derive(Clone, Debug, Serialize)]
pub struct LinkView {
    pub name: String,
    pub ipv4: String,
    pub gateway: Option<String>,
    pub up: bool,
    pub rtt_ms: Option<f64>,
}

#[derive(Serialize)]
struct Row {
    #[serde(flatten)]
    view: LinkView,
    #[serde(flatten)]
    totals: Totals,
    /// False once the interface has disappeared; its counters stay for the record.
    present: bool,
}

#[derive(Serialize)]
struct Status {
    policy: &'static str,
    uptime_s: u64,
    links: Vec<Row>,
}

fn status_json(s: &Shared) -> String {
    let mut totals = s.stats.snapshot();
    let mut rows: Vec<Row> = s
        .links
        .read()
        .unwrap()
        .iter()
        .map(|v| Row { totals: totals.remove(&v.name).unwrap_or_default(), view: v.clone(), present: true })
        .collect();
    rows.extend(totals.into_iter().map(|(name, totals)| Row {
        view: LinkView { name, ipv4: String::new(), gateway: None, up: false, rtt_ms: None },
        totals,
        present: false,
    }));
    let status = Status { policy: s.balancer.policy_name(), uptime_s: s.started.elapsed().as_secs(), links: rows };
    serde_json::to_string(&status).expect("status serializes")
}

/// Serve the status page and its JSON on `addr` until the process stops.
pub async fn serve(addr: SocketAddr, shared: Arc<Shared>) -> io::Result<()> {
    let listener = TcpListener::bind(addr).await?;
    log!("status page: http://{addr}/");
    loop {
        let (sock, _) = listener.accept().await?;
        let shared = shared.clone();
        tokio::spawn(async move {
            let _ = handle(sock, addr, &shared).await;
        });
    }
}

async fn handle(mut sock: TcpStream, addr: SocketAddr, shared: &Shared) -> io::Result<()> {
    let mut buf = vec![0u8; 8192];
    let mut n = 0;
    while !buf[..n].windows(4).any(|w| w == b"\r\n\r\n") {
        if n == buf.len() {
            return Ok(());
        }
        let k = tokio::time::timeout(Duration::from_secs(5), sock.read(&mut buf[n..])).await??;
        if k == 0 {
            return Ok(());
        }
        n += k;
    }
    let (code, ctype, body) = route(&String::from_utf8_lossy(&buf[..n]), addr, || status_json(shared));
    let head = format!(
        "HTTP/1.1 {code}\r\nContent-Type: {ctype}\r\nContent-Length: {}\r\nCache-Control: no-store\r\nConnection: close\r\n\r\n",
        body.len()
    );
    sock.write_all(head.as_bytes()).await?;
    sock.write_all(body.as_bytes()).await?;
    Ok(())
}

/// Pick the response (code, content type, body) for a request head. `json` builds the status
/// body only when needed.
fn route(head: &str, addr: SocketAddr, json: impl FnOnce() -> String) -> (&'static str, &'static str, String) {
    let mut first = head.lines().next().unwrap_or_default().split(' ');
    let (method, path) = (first.next().unwrap_or_default(), first.next().unwrap_or_default());
    if !host_ok(head, addr) {
        return ("403 Forbidden", "text/plain", "forbidden host\n".into());
    }
    match (method, path) {
        ("GET", "/") => ("200 OK", "text/html; charset=utf-8", PAGE.into()),
        ("GET", "/api/status") => ("200 OK", "application/json", json()),
        (_, "/" | "/api/status") => ("405 Method Not Allowed", "text/plain", "method not allowed\n".into()),
        _ => ("404 Not Found", "text/plain", "not found\n".into()),
    }
}

fn header<'a>(head: &'a str, name: &str) -> Option<&'a str> {
    head.lines().find_map(|l| l.split_once(':').filter(|(k, _)| k.trim().eq_ignore_ascii_case(name)).map(|(_, v)| v.trim()))
}

/// On a loopback address, only answer requests addressed to a loopback name. This stops a
/// web page from reading the status through DNS rebinding (attacker name -> 127.0.0.1).
fn host_ok(head: &str, addr: SocketAddr) -> bool {
    if !addr.ip().is_loopback() {
        return true;
    }
    let Some(host) = header(head, "host") else { return false };
    let name = match host.strip_prefix('[') {
        Some(v6) => v6.split(']').next().unwrap_or_default(),
        None => host.split(':').next().unwrap_or_default(),
    };
    matches!(name, "127.0.0.1" | "localhost" | "::1")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn req(method: &str, path: &str, host: &str) -> String {
        format!("{method} {path} HTTP/1.1\r\nHost: {host}\r\nAccept: */*\r\n\r\n")
    }

    #[test]
    fn routes_and_rejects_foreign_hosts() {
        let lo: SocketAddr = "127.0.0.1:7878".parse().unwrap();
        let code = |r: &str, a| route(r, a, || "{}".into()).0;
        assert_eq!(code(&req("GET", "/", "127.0.0.1:7878"), lo), "200 OK");
        assert_eq!(route(&req("GET", "/api/status", "localhost:7878"), lo, || "{\"x\":1}".into()).2, "{\"x\":1}");
        assert_eq!(code(&req("GET", "/", "[::1]:7878"), lo), "200 OK");
        assert_eq!(code(&req("GET", "/nope", "localhost"), lo), "404 Not Found");
        assert_eq!(code(&req("POST", "/", "localhost"), lo), "405 Method Not Allowed");
        // no control over HTTP
        assert_eq!(code(&req("POST", "/api/stop", "localhost"), lo), "404 Not Found");
        assert_eq!(code(&req("GET", "/api/status", "evil.example:7878"), lo), "403 Forbidden");
        assert_eq!(code("GET / HTTP/1.1\r\n\r\n", lo), "403 Forbidden"); // no Host
        // bound to a LAN address on purpose: any Host is fine
        let lan: SocketAddr = "192.168.1.13:7878".parse().unwrap();
        assert_eq!(code(&req("GET", "/", "my-mac.local:7878"), lan), "200 OK");
    }

    #[test]
    fn json_lists_current_links_then_vanished_ones() {
        let stats = Arc::new(Stats::default());
        let _a = stats.open("en0");
        let _b = stats.open("en9");
        let view = LinkView { name: "en0".into(), ipv4: "192.168.1.13".into(), gateway: None, up: true, rtt_ms: Some(5.0) };
        let shared = Shared {
            stats,
            links: RwLock::new(vec![view]),
            balancer: Arc::new(Balancer::new(Default::default(), Default::default(), vec![])),
            started: Instant::now(),
        };
        let v: serde_json::Value = serde_json::from_str(&status_json(&shared)).unwrap();
        assert_eq!(v["policy"], "sticky");
        assert_eq!(v["links"][0]["name"], "en0");
        assert_eq!(v["links"][0]["active"], 1);
        assert_eq!(v["links"][0]["present"], true);
        assert_eq!(v["links"][1]["name"], "en9");
        assert_eq!(v["links"][1]["present"], false);
    }
}
