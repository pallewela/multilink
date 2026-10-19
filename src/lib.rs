//! multilink: balance traffic across several internet links through a TUN device.
//!
//! Used two ways: by the `multilink` command-line tool (src/main.rs), and embedded in other
//! programs such as the macOS menu bar app, through [`cli_main`] or the C functions in
//! [`ffi`] (header: macos/multilink.h).

/// println! for the running balancer's log: prefixed with the local date and time.
macro_rules! log {
    ($($arg:tt)*) => { println!("{} {}", chrono::Local::now().format("%Y-%m-%d %H:%M:%S"), format_args!($($arg)*)) };
}

/// eprintln! with the same timestamp prefix as log!.
macro_rules! elog {
    ($($arg:tt)*) => { eprintln!("{} {}", chrono::Local::now().format("%Y-%m-%d %H:%M:%S"), format_args!($($arg)*)) };
}

mod balance;
mod config;
mod dial;
pub mod ffi;
mod health;
mod links;
mod stats;
mod status;
mod tunnel;

use clap::{Parser, Subcommand};
use std::sync::{Arc, Mutex};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

#[derive(Parser)]
#[command(version, about = "Balance traffic across multiple internet links")]
struct Cli {
    /// Use only these interfaces (repeatable). Default: auto-detect.
    #[arg(long = "link", global = true)]
    links: Vec<String>,
    /// TOML config file (see multilink.example.toml). Flags override it.
    #[arg(long, global = true, value_name = "FILE")]
    config: Option<String>,
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// List the links that would be used.
    Links,
    /// Fetch the public IP through each link, to prove per-link routing works.
    Probe,
    /// Capture all IPv4 traffic and balance it across links. Needs root/admin.
    Run {
        /// Balancing policy [default: sticky]
        #[arg(long, value_enum)]
        policy: Option<balance::Policy>,
        /// Stop by itself after this many seconds (for unattended tests).
        #[arg(long = "for", value_name = "SECS")]
        stop_after: Option<u64>,
        /// Health-check target, TCP-connected through each link every 2s [default: 1.1.1.1:443]
        #[arg(long)]
        probe: Option<std::net::SocketAddr>,
        /// Status page address, or "off" [default: 127.0.0.1:7878]
        #[arg(long, value_name = "ADDR|off")]
        status: Option<String>,
    },
}

/// Run the multilink command line with `args` (the first is the program name, as in
/// `std::env::args`). Returns the process exit code: 0 ok, 1 runtime failure, 2 bad input.
pub fn cli_main<I, T>(args: I) -> i32
where
    I: IntoIterator<Item = T>,
    T: Into<std::ffi::OsString> + Clone,
{
    let cli = match Cli::try_parse_from(args) {
        Ok(cli) => cli,
        Err(e) => {
            let _ = e.print(); // --help and --version land here too
            return if e.use_stderr() { 2 } else { 0 };
        }
    };
    match tokio::runtime::Runtime::new() {
        Ok(rt) => rt.block_on(run(cli)),
        Err(e) => {
            eprintln!("can't start the async runtime: {e}");
            1
        }
    }
}

/// Check a config file without running anything. Err holds a one-line reason, without the
/// file name.
pub fn check_config(path: &str) -> Result<(), String> {
    let text = std::fs::read_to_string(path).map_err(|e| e.to_string())?;
    config::parse(&text).map(|_| ()).map_err(|e| last_line(&e))
}

async fn run(cli: Cli) -> i32 {
    let cfg = match &cli.config {
        Some(path) => match config::load(path) {
            Ok(c) => c,
            Err(e) => {
                eprintln!("config error: {e}");
                return 2;
            }
        },
        None => config::Config::default(),
    };
    let only = if cli.links.is_empty() { cfg.links.clone() } else { cli.links.clone() };
    let links = links::discover(&only, &cfg.exclude);
    if links.is_empty() {
        eprintln!("no usable links found");
        return 1;
    }
    match cli.cmd {
        Cmd::Links => {
            for l in &links {
                let gw = l.gateway.map_or("-".into(), |g| g.to_string());
                println!("{:<10} idx={:<3} ip={:<15} gw={gw}", l.name, l.index, l.ipv4);
            }
        }
        Cmd::Probe => {
            let dst = tokio::net::lookup_host("api.ipify.org:80")
                .await
                .ok()
                .and_then(|mut a| a.find(|a| a.is_ipv4()))
                .expect("resolve api.ipify.org");
            for l in &links {
                match public_ip(dst, l).await {
                    Ok(ip) => println!("{:<10} public ip {ip}", l.name),
                    Err(e) => println!("{:<10} FAILED: {e}", l.name),
                }
            }
        }
        Cmd::Run { policy, stop_after, probe, status } => {
            let fixed = match Fixed::new(&cfg, only, probe, status.as_deref()) {
                Ok(f) => f,
                Err(e) => {
                    elog!("{e}");
                    return 2;
                }
            };
            let policy = policy.or(cfg.policy).unwrap_or_default();
            log_weights_and_pins(&cfg);
            let balancer = Arc::new(balance::Balancer::new(policy, cfg.weights, cfg.pin));
            let stop_after = stop_after.map(std::time::Duration::from_secs);
            return relay(links, balancer, fixed, true, Arc::default(), stop_after).await;
        }
    }
    0
}

/// Settings that only take effect when the balancer starts. Policy, weights and pins are
/// not here: [`Running::reload`] applies those live.
#[derive(Clone, Debug, PartialEq)]
struct Fixed {
    only: Vec<String>,
    exclude: Vec<String>,
    probe: std::net::SocketAddr,
    status: Option<std::net::SocketAddr>,
}

impl Fixed {
    /// `only`, `probe` and `status` come from the command line or the embedder when set.
    fn new(
        cfg: &config::Config,
        only: Vec<String>,
        probe: Option<std::net::SocketAddr>,
        status: Option<&str>,
    ) -> Result<Self, String> {
        let status = match status.or(cfg.status.as_deref()).unwrap_or("127.0.0.1:7878") {
            "off" => None,
            s => Some(s.parse().map_err(|_| format!("bad status address {s:?}: want IP:PORT or off"))?),
        };
        let probe = probe.or(cfg.probe).unwrap_or(([1, 1, 1, 1], 443).into());
        Ok(Fixed { only, exclude: cfg.exclude.clone(), probe, status })
    }

    /// Config names of the settings that differ from `other`.
    fn changed(&self, other: &Fixed) -> Vec<&'static str> {
        [
            ("links", self.only != other.only),
            ("exclude", self.exclude != other.exclude),
            ("probe", self.probe != other.probe),
            ("status", self.status != other.status),
        ]
        .into_iter()
        .filter_map(|(name, differs)| differs.then_some(name))
        .collect()
    }
}

/// Run the tunnel until stopped; returns the exit code.
async fn relay(
    links: Vec<links::Link>,
    balancer: Arc<balance::Balancer>,
    fixed: Fixed,
    signals: bool,
    stop: Arc<tokio::sync::Notify>,
    stop_after: Option<std::time::Duration>,
) -> i32 {
    let names: Vec<_> = links.iter().map(|l| l.name.as_str()).collect();
    log!("links: {} | policy: {} | probe: {}", names.join(", "), balancer.policy_name(), fixed.probe);
    let Fixed { only, exclude, probe, status } = fixed;
    let opts = tunnel::Opts { only, exclude, probe, stop_after, status, signals, stop };
    if let Err(e) = tunnel::run(links, balancer, opts).await {
        elog!("error: {e}");
        if e.kind() == std::io::ErrorKind::AlreadyExists {
            elog!("the capture routes already exist: is another multilink running?");
        } else {
            elog!("needs root (sudo) or, on Windows, an Administrator prompt with wintun.dll next to the exe");
        }
        return 1;
    }
    0
}

/// A balancer running on its own thread inside this process. Made by [`start`].
pub struct Running {
    balancer: Arc<balance::Balancer>,
    fixed: Fixed,
    status_override: Option<String>,
    stop: Arc<tokio::sync::Notify>,
    thread: Mutex<Option<std::thread::JoinHandle<i32>>>,
    code: Mutex<Option<i32>>,
}

/// Start balancing with the settings in `config_toml` (the config file's text, so the caller
/// decides which file is trusted). `status` overrides the config's status address. Errors
/// found before starting come back here; later ones end the run and show in [`Running::wait`].
pub fn start(config_toml: &str, status: Option<&str>) -> Result<Running, String> {
    let cfg = config::parse(config_toml).map_err(|e| last_line(&e))?;
    let fixed = Fixed::new(&cfg, cfg.links.clone(), None, status)?;
    let links = links::discover(&fixed.only, &fixed.exclude);
    if links.is_empty() {
        return Err("no usable links found".into());
    }
    let rt = tokio::runtime::Runtime::new().map_err(|e| format!("can't start the async runtime: {e}"))?;
    log_weights_and_pins(&cfg);
    let balancer = Arc::new(balance::Balancer::new(cfg.policy.unwrap_or_default(), cfg.weights, cfg.pin));
    let stop = Arc::new(tokio::sync::Notify::new());
    let (b, s) = (balancer.clone(), stop.clone());
    let opts = fixed.clone();
    let thread = std::thread::Builder::new()
        .name("multilink".into())
        .spawn(move || {
            std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                rt.block_on(relay(links, b, opts, false, s, None))
            }))
            .unwrap_or(101)
        })
        .map_err(|e| format!("can't start the balancer thread: {e}"))?;
    Ok(Running {
        balancer,
        fixed,
        status_override: status.map(str::to_string),
        stop,
        thread: Mutex::new(Some(thread)),
        code: Mutex::new(None),
    })
}

impl Running {
    /// Apply `config_toml` to new flows: policy, weights and pins change now. Returns the
    /// names of settings that changed but only apply after a restart (links, exclude, probe,
    /// status). An invalid config changes nothing.
    pub fn reload(&self, config_toml: &str) -> Result<Vec<&'static str>, String> {
        let cfg = config::parse(config_toml).map_err(|e| last_line(&e))?;
        let fixed = Fixed::new(&cfg, cfg.links.clone(), None, self.status_override.as_deref())?;
        let policy = cfg.policy.unwrap_or_default();
        self.balancer.set(policy, cfg.weights, cfg.pin);
        let restart = self.fixed.changed(&fixed);
        log!("settings reloaded: policy {}", self.balancer.policy_name());
        if !restart.is_empty() {
            log!("changed, apply after a restart: {}", restart.join(", "));
        }
        Ok(restart)
    }

    /// Ask the balancer to stop. Returns at once; [`Running::wait`] returns once it has
    /// removed its routes.
    pub fn stop(&self) {
        self.stop.notify_one();
    }

    /// Block until the balancer has stopped; returns its exit code (0 ok, 1 runtime failure,
    /// 101 panic). Safe to call again or from several threads: later calls return the same code.
    pub fn wait(&self) -> i32 {
        let mut thread = self.thread.lock().unwrap();
        if let Some(t) = thread.take() {
            *self.code.lock().unwrap() = Some(t.join().unwrap_or(101));
        }
        self.code.lock().unwrap().unwrap_or(101)
    }
}

fn log_weights_and_pins(cfg: &config::Config) {
    if !cfg.weights.is_empty() || !cfg.pin.is_empty() {
        log!("weights: {:?} | pins: {}", cfg.weights, cfg.pin.len());
    }
}

/// TOML errors are multi-line pictures; their last line says what is wrong.
fn last_line(e: &str) -> String {
    e.lines().rev().map(str::trim).find(|l| !l.is_empty()).unwrap_or("invalid config").to_string()
}

async fn public_ip(dst: std::net::SocketAddr, link: &links::Link) -> std::io::Result<String> {
    let mut s = tokio::time::timeout(std::time::Duration::from_secs(5), dial::connect(dst, link)).await??;
    s.write_all(b"GET / HTTP/1.0\r\nHost: api.ipify.org\r\n\r\n").await?;
    let mut body = String::new();
    s.read_to_string(&mut body).await?;
    Ok(body.rsplit("\r\n\r\n").next().unwrap_or_default().trim().to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A Running whose thread has already finished: reload and wait work without a TUN.
    fn running(toml: &str, status: Option<&str>) -> Running {
        let cfg = config::parse(toml).unwrap();
        Running {
            fixed: Fixed::new(&cfg, cfg.links.clone(), None, status).unwrap(),
            balancer: Arc::new(balance::Balancer::new(cfg.policy.unwrap_or_default(), cfg.weights, cfg.pin)),
            status_override: status.map(str::to_string),
            stop: Arc::default(),
            thread: Mutex::new(Some(std::thread::spawn(|| 0))),
            code: Mutex::new(None),
        }
    }

    #[test]
    fn reload_applies_policy_live_and_lists_restart_only_changes() {
        let r = running("policy = 'sticky'\nexclude = ['bridge0']", None);
        assert_eq!(r.reload("policy = 'round-robin'\nexclude = ['bridge0']"), Ok(vec![]));
        assert_eq!(r.balancer.policy_name(), "round-robin");
        let restart = r.reload("policy = 'least-latency'\nprobe = '9.9.9.9:443'\nlinks = ['en0']").unwrap();
        assert_eq!(restart, ["links", "exclude", "probe"]);
        assert_eq!(r.balancer.policy_name(), "least-latency");
        // an invalid config changes nothing
        assert!(r.reload("policy = 'fastest'").unwrap_err().contains("unknown variant"));
        assert_eq!(r.balancer.policy_name(), "least-latency");
        // no policy line = the default
        r.reload("exclude = ['bridge0']").unwrap();
        assert_eq!(r.balancer.policy_name(), "sticky");
        assert_eq!((r.wait(), r.wait()), (0, 0));
    }

    #[test]
    fn status_override_hides_the_configs_status_address() {
        let r = running("", Some("127.0.0.1:7878"));
        assert_eq!(r.reload("status = '127.0.0.1:9999'"), Ok(vec![]));
        let r = running("", None);
        assert_eq!(r.reload("status = 'off'"), Ok(vec!["status"]));
        assert!(r.reload("status = 'nowhere'").unwrap_err().contains("bad status address"));
    }
}
