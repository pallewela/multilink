# 🔀 multilink

**Use all your internet connections at once.**

You have Wi-Fi. You have Ethernet. Your phone has 5G and a USB cable. Your computer uses
exactly one of them and ignores the rest. Multilink puts every link to work: new connections
are spread across all of them, and when one drops, traffic moves to the others without you
noticing.

No proxy settings, no per-app setup, no server to rent. Start it, and everything on the
machine goes faster.

> 🧪 **Alpha.** It works on the author's desk; it may not work on yours yet. Bug reports are
> very welcome. The first release is macOS only.

## ✨ Why

- 🚀 **More bandwidth.** Downloads, updates, video calls and browser tabs open many connections.
  Spread over two links, they get roughly the bandwidth of both.
- 🛟 **Failover.** Each link is health-checked every two seconds. A dead link stops getting new
  connections; existing ones on healthy links are untouched.
- 📌 **Logins stay put.** The default `sticky` policy keeps each destination on the same link,
  so sites that tie your session to an IP address don't log you out.
- 🎛️ **You stay in control.** Weight links ("Wi-Fi gets three quarters"), pin address ranges to a
  link, or exclude interfaces entirely.
- 📊 **See what's happening.** A live status page at <http://127.0.0.1:7878> shows every link,
  whether it's healthy, and live download and upload rates.

## ⚙️ How it works

```
 apps ──► OS routing ──► TUN device ──► userspace TCP/UDP stack
                                              │
                                     balancer picks a link
                                     ┌────────┼────────┐
                                     ▼        ▼        ▼
                                   Wi-Fi  Ethernet  iPhone USB ──► internet
```

Multilink installs two routes (`0.0.0.0/1` and `128.0.0.0/1`) that send all IPv4 traffic into
a virtual network device. A userspace network stack turns packets back into connections, and
each new connection is dialed out through a socket bound to the chosen link. The routes are
removed when multilink stops.

IPv6 works the same way (`::/1` and `8000::/1`) while at least one link passes the IPv6 health
check, and IPv6 connections only use links that pass it. With no working IPv6 link, IPv6 is
left to the system. Set `probe6 = "off"` to never capture IPv6. On macOS, apps only look up
IPv6 addresses when the primary network (the top one in System Settings > Network) has IPv6. If
the primary network is IPv4 only, apps stay on IPv4 even though other links have IPv6.

⚠️ **What it isn't:** bonding. A single TCP stream, like one large file download over one
connection, is still capped by the link it lands on. Combining one stream across links needs a
relay server, and that's out of scope. ICMP (ping) is not relayed yet, and a link needs IPv4 to
be used at all.

## 🍎 Install on macOS

Requires macOS 13 or later on Apple silicon.

With [Homebrew](https://brew.sh):

```sh
brew install --cask pallewela/tap/multilink
```

Then skip to step 4. Or install by hand:

1. Download `Multilink-<version>-macos.dmg` from
   [Releases](https://github.com/pallewela/multilink/releases) and check it against the
   `.sha256` file:

   ```sh
   shasum -a 256 -c Multilink-*-macos.dmg.sha256
   ```

2. Open the DMG and drag **Multilink** into **Applications**. Install it there: the app
   runs a root helper from its own bundle, so it must live where only admins can change it.
3. The alpha isn't notarized yet, so macOS will refuse to open it. Clear the quarantine flag
   once:

   ```sh
   xattr -dr com.apple.quarantine /Applications/Multilink.app
   ```

4. Open Multilink. It lives in the menu bar. Choose **Start Balancer**, then allow the helper once in
   **System Settings > General > Login Items**.

## 💻 Command line

The same engine ships as a CLI (build it from source for now):

```sh
multilink links                       # which links would be used
multilink probe                       # your public IP through each link
sudo multilink run                    # balance everything
sudo multilink run --policy least-latency --config multilink.toml
```

| Policy          | Behaviour                                                        |
|-----------------|------------------------------------------------------------------|
| `sticky`        | Default. Each destination keeps its link; weights shape the split |
| `round-robin`   | Every new connection takes the next link                         |
| `least-latency` | Everything on the fastest healthy link; the rest are hot standby |

All settings are optional. See [`multilink.example.toml`](multilink.example.toml) for weights,
pins, the health-check target and more.

## 🛠️ Build from source

You need stable Rust, plus the Xcode command line tools for the macOS app.

```sh
cargo build --release            # CLI: target/release/multilink
sh scripts/build-macos-app.sh    # menu bar app: target/Multilink.app
sh scripts/build-linux.sh        # Linux bundle
sh scripts/build-windows.sh      # Windows (or scripts/build-windows.ps1)
```

Linux and Windows builds pass CI but aren't released yet.

## 📄 License

[MIT](LICENSE)
