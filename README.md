# multilink

Balance your machine's internet traffic across several links at once (Wi-Fi, Ethernet, USB
tether, ...). Multilink captures IPv4 traffic through a TUN device and sends each new TCP
connection out over one of the links, with health checks and failover.

**Status: alpha.** Expect rough edges. Only the macOS release is published for now.

## What it does and does not do

- Balances per connection. Many parallel flows approach the sum of your links; a single
  stream is capped by the link it lands on. Single-stream bonding would need a relay server.
- Transparent: no per-app configuration. Needs root/admin (TUN device and route changes).
- TCP and UDP (UDP is needed for DNS). IPv4 only. ICMP and other protocols are dropped.

## Install (macOS 13+, Apple silicon)

1. Download `MultilinkBar-<version>-macos.zip` from the
   [Releases](https://github.com/pallewela/multilink/releases) page and verify it against
   the `.sha256` file.
2. Unzip and move `MultilinkBar.app` to `/Applications`. The app runs a root helper from
   its own bundle, so install it where only admins can modify it.
3. The alpha is ad-hoc signed, not notarized. Remove the quarantine flag, then open it:

   ```sh
   xattr -dr com.apple.quarantine /Applications/MultilinkBar.app
   open /Applications/MultilinkBar.app
   ```

4. The first Start registers the helper; allow it once in System Settings > Login Items.

The app lives in the menu bar. Its status page is at <http://127.0.0.1:7878>.

## Command line

```sh
multilink links                 # list the links that would be used
multilink probe                 # public IP through each link
sudo multilink run              # capture and balance traffic
sudo multilink run --config multilink.toml --policy least-latency
```

Policies: `sticky` (default; a destination keeps its link), `round-robin`, `least-latency`.
See [`multilink.example.toml`](multilink.example.toml) for weights, per-range pins, and the
other settings.

## Build from source

Needs Rust (stable).

```sh
cargo build --release            # CLI: target/release/multilink
sh scripts/build-macos-bar.sh    # macOS app: target/MultilinkBar.app (needs Xcode CLT)
sh scripts/build-linux.sh        # Linux bundle
sh scripts/build-windows.sh      # Windows (see also build-windows.ps1)
```

Linux and Windows builds are tested in CI but not yet released.

## License

[MIT](LICENSE)
