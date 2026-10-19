// Multilink: a macOS menu bar app showing multilink's state.
// Polls multilink's status JSON every second; the icon's shape gives the overall state
// and the menu lists each link.
//
// Control goes through a privileged daemon, never HTTP: the same executable run by launchd as
// root (`Multilink daemon`, registered once with SMAppService and approved in System
// Settings). The app talks to it over XPC, and each side only accepts a peer whose code has
// the same cdhash as its own, so only this exact build can drive the root process. The daemon
// runs the multilink library (macos/multilink.h) in-process; the GUI never runs as root.
// The daemon stops the balancer when the app's connection goes away (quit, crash, force quit).
// Build with scripts/build-macos-app.sh.
//
// Settings (defaults write com.multilink.bar <key> <value>):
//   statusURL     default http://127.0.0.1:7878
//   configPath    multilink TOML config, default ~/Library/Application Support/Multilink/multilink.toml
//   startOnLaunch YES = start the balancer when the app launches (if none is running)
// The Settings submenu edits startOnLaunch, the policy line of the config, or opens the config.
// Config changes apply while running; settings that need a restart offer one.
// `Multilink --print` fetches once, prints the menu text and exits.
// `Multilink --selftest` runs the built-in checks and exits.
import AppKit
import ServiceManagement

struct Link: Decodable {
    let name: String
    let ipv4: String
    let gateway: String?
    let up: Bool
    let rtt_ms: Double?
    let present: Bool
    let active: Int
    let flows: Int
    let bytes_up: UInt64
    let bytes_down: UInt64
}

struct Status: Decodable {
    let policy: String
    let uptime_s: Int
    let links: [Link]
}

typealias Rates = [String: (down: Double, up: Double)]

enum Overall {
    case stopped, ok, degraded, down

    /// SF Symbol and words: the shape and the tooltip carry the state, not colour.
    var symbol: (name: String, label: String) {
        switch self {
        case .stopped: ("network.slash", "multilink is not running")
        case .ok: ("network", "All links up")
        case .degraded: ("exclamationmark.triangle", "Some links down")
        case .down: ("wifi.exclamationmark", "All links down")
        }
    }
}

func overall(_ s: Status?) -> Overall {
    guard let s else { return .stopped }
    let present = s.links.filter(\.present)
    if present.allSatisfy({ !$0.up }) { return .down }
    return present.allSatisfy(\.up) ? .ok : .degraded
}

/// Bytes per second per link between two samples. Counters going backwards = restart: no rate.
func rates(prev: Status?, prevAt: Date?, now: Status, at: Date) -> Rates {
    guard let prev, let prevAt else { return [:] }
    let dt = max(at.timeIntervalSince(prevAt), 0.001)
    var r = Rates()
    for l in now.links {
        guard let p = prev.links.first(where: { $0.name == l.name }),
              l.bytes_down >= p.bytes_down, l.bytes_up >= p.bytes_up else { continue }
        r[l.name] = (Double(l.bytes_down - p.bytes_down) / dt, Double(l.bytes_up - p.bytes_up) / dt)
    }
    return r
}

func human(_ b: Double) -> String {
    switch b {
    case 1e9...: String(format: "%.1f GB", b / 1e9)
    case 1e6...: String(format: "%.1f MB", b / 1e6)
    case 1e3...: String(format: "%.0f kB", b / 1e3)
    default: String(format: "%.0f B", b)
    }
}

func uptime(_ s: Int) -> String {
    let (h, m) = (s / 3600, s % 3600 / 60)
    return h > 0 ? "\(h)h \(m)m" : m > 0 ? "\(m)m \(s % 60)s" : "\(s)s"
}

/// One menu row per entry: (state word for the link or nil for plain rows, text).
func menuRows(_ s: Status?, _ r: Rates) -> [(state: String?, text: String)] {
    guard let s else { return [(nil, "Balancer is not running")] }
    var rows: [(String?, String)] = [(nil, "Policy \(s.policy) · up \(uptime(s.uptime_s))")]
    if s.links.isEmpty { rows.append((nil, "No links detected")) }
    for l in s.links {
        let state = !l.present ? "Gone" : l.up ? "Up" : "Down"
        let rtt = l.rtt_ms.map { String(format: " · %.0f ms", $0) } ?? ""
        let rate = r[l.name] ?? (0, 0)
        rows.append((state, "\(l.name)  \(state)\(rtt)"))
        rows.append((nil, "    ↓ \(human(rate.down))/s   ↑ \(human(rate.up))/s   · \(l.active) active"))
    }
    return rows
}

@MainActor
final class Bar: NSObject, NSApplicationDelegate, NSMenuDelegate {
    let url: URL
    let item = NSStatusBar.system.statusItem(withLength: NSStatusItem.squareLength)
    let menu = NSMenu()
    var status: Status?
    var sampledAt: Date?
    var currentRates = Rates()
    var menuOpen = false
    /// The daemon runs a balancer we asked for (set when asking, cleared when it stops).
    var ours = false
    var lastError: String?
    var firstTick = true
    /// When the pending start was asked for; cleared once it settles.
    var startedAt: Date?
    /// When the config was last sent to the running balancer.
    var appliedAt: Date?
    /// Settings changed while running that only a restart applies (from the daemon's reload).
    var needsRestart: String?
    lazy var daemon = DaemonClient { [weak self] code in self?.balancerStopped(code) }
    /// One persistent Settings item and submenu, re-added (never recreated) on rebuild.
    let settings = NSMenu()
    lazy var settingsItem: NSMenuItem = {
        let mi = NSMenuItem(title: "Settings", action: nil, keyEquivalent: "")
        mi.submenu = settings
        return mi
    }()
    var settingsOpen = false
    /// Link rows in the open menu, updated in place, and the layout they were built for.
    var rowItems: [NSMenuItem] = []
    var layout = ""

    init(url: URL) {
        self.url = url
    }

    func applicationDidFinishLaunching(_ note: Notification) {
        menu.delegate = self
        menu.autoenablesItems = false
        settings.delegate = self
        settings.autoenablesItems = false
        item.menu = menu
        show(.stopped)
        // .common mode: keep updating while the menu is open.
        let timer = Timer(timeInterval: 1, repeats: true) { [weak self] _ in
            MainActor.assumeIsolated { self?.tick() }
        }
        RunLoop.main.add(timer, forMode: .common)
        tick()
    }

    func tick() {
        Task {
            let new = await fetch(url)
            let now = Date()
            currentRates = new.map { rates(prev: status, prevAt: sampledAt, now: $0, at: now) } ?? [:]
            (status, sampledAt) = (new, now)
            show(overall(new))
            if firstTick, new == nil, UserDefaults.standard.bool(forKey: "startOnLaunch") { startBalancer() }
            firstTick = false
            if ours, let appliedAt, modified(Paths.config) > appliedAt { applyConfig() }
            refresh()
        }
    }

    func show(_ o: Overall) {
        let (name, label) = o.symbol
        let image = NSImage(systemSymbolName: name, accessibilityDescription: label)
        image?.isTemplate = true
        item.button?.image = image
        item.button?.toolTip = "multilink: \(label)"
    }

    func menuWillOpen(_ m: NSMenu) {
        if m === settings {
            settingsOpen = true
            rebuildSettings()
        } else {
            menuOpen = true
            rebuild()
        }
    }

    func menuDidClose(_ m: NSMenu) {
        if m === settings { settingsOpen = false } else { menuOpen = false }
    }

    /// Keep the open menu current without replacing its items: replacing them every second
    /// made a hovered or open Settings submenu flash shut. Rates and latency are updated in
    /// place; items are only rebuilt when the set of items must change, and never while the
    /// Settings submenu is open.
    func refresh() {
        guard menuOpen else { return }
        let rows = menuRows(status, currentRates)
        if layoutKey(rows) == layout {
            for (mi, row) in zip(rowItems, rows) { style(mi, row) }
        } else if !settingsOpen {
            rebuild()
        }
    }

    /// What decides which items the menu has. Row text (rates, latency) is not part of it.
    func layoutKey(_ rows: [(state: String?, text: String)]) -> String {
        let states = rows.map { $0.state ?? "-" }.joined(separator: ",")
        return "\(states)|\(ours)|\(status != nil)|\(restartOffered)|\(lastError ?? "")"
    }

    var restartOffered: Bool { ours && status != nil && needsRestart != nil }

    func style(_ mi: NSMenuItem, _ row: (state: String?, text: String)) {
        guard let state = row.state else {
            mi.title = row.text
            return
        }
        // A coloured dot beside the state word; the word itself carries the meaning.
        let dot: NSColor = state == "Up" ? .systemGreen : state == "Down" ? .systemRed : .secondaryLabelColor
        let title = NSMutableAttributedString(string: "● ", attributes: [.foregroundColor: dot])
        title.append(NSAttributedString(string: row.text, attributes: [.font: NSFont.menuFont(ofSize: 0)]))
        mi.attributedTitle = title
    }

    func rebuild() {
        menu.removeAllItems()
        let rows = menuRows(status, currentRates)
        layout = layoutKey(rows)
        rowItems = rows.map { row in
            let mi = NSMenuItem(title: row.text, action: #selector(openPage), keyEquivalent: "")
            mi.target = self
            style(mi, row)
            menu.addItem(mi)
            return mi
        }
        menu.addItem(.separator())
        switch (ours, status != nil) {
        case (true, false): add("Starting…", nil)
        case (true, true): add("Stop Balancer", #selector(stopBalancer))
        case (false, false): add("Start Balancer", #selector(startBalancer))
        case (false, true): add("Started outside Multilink", nil)
        }
        if restartOffered, let needsRestart { add("Restart Balancer to Apply \(needsRestart)", #selector(restartBalancer)) }
        if let lastError { add(lastError, nil) }
        menu.addItem(settingsItem)
        add("Open Log", #selector(openLog))
        menu.addItem(.separator())
        let open = NSMenuItem(title: "Open Status Page", action: #selector(openPage), keyEquivalent: "o")
        open.target = self
        open.isEnabled = status != nil
        menu.addItem(open)
        let quit = NSMenuItem(title: "Quit Multilink", action: #selector(quit), keyEquivalent: "q")
        quit.target = self
        menu.addItem(quit)
    }

    func add(_ title: String, _ action: Selector?) {
        let mi = NSMenuItem(title: title, action: action, keyEquivalent: "")
        mi.target = self
        mi.isEnabled = action != nil
        menu.addItem(mi)
    }

    func changed() {
        refresh()
    }

    func rebuildSettings() {
        settings.removeAllItems()
        let current = configPolicy((try? String(contentsOf: Paths.config, encoding: .utf8)) ?? "")
        settings.addItem(withTitle: "Policy", action: nil, keyEquivalent: "").isEnabled = false
        for (key, label) in policies {
            let mi = NSMenuItem(title: label, action: #selector(choosePolicy(_:)), keyEquivalent: "")
            mi.target = self
            mi.representedObject = key
            mi.state = key == current ? .on : .off
            mi.indentationLevel = 1
            settings.addItem(mi)
        }
        settings.addItem(.separator())
        let launch = NSMenuItem(title: "Start Balancer on Launch", action: #selector(toggleStartOnLaunch), keyEquivalent: "")
        launch.target = self
        launch.state = UserDefaults.standard.bool(forKey: "startOnLaunch") ? .on : .off
        settings.addItem(launch)
        let edit = NSMenuItem(title: "Edit Config File…", action: #selector(editConfig), keyEquivalent: ",")
        edit.target = self
        settings.addItem(edit)
    }

    @objc func choosePolicy(_ sender: NSMenuItem) {
        guard let key = sender.representedObject as? String else { return }
        do {
            try ensureConfig()
            let text = try String(contentsOf: Paths.config, encoding: .utf8)
            try withPolicy(text, key).write(to: Paths.config, atomically: true, encoding: .utf8)
        } catch {
            lastError = "Couldn't save settings: \(error.localizedDescription)"
        }
        if ours { applyConfig() }
        changed()
    }

    @objc func toggleStartOnLaunch() {
        let d = UserDefaults.standard
        d.set(!d.bool(forKey: "startOnLaunch"), forKey: "startOnLaunch")
    }

    @objc func editConfig() {
        do {
            try ensureConfig()
        } catch {
            lastError = "Couldn't create config: \(error.localizedDescription)"
            return
        }
        // .toml often has no default app; fall back to TextEdit.
        if NSWorkspace.shared.urlForApplication(toOpen: Paths.config) != nil {
            NSWorkspace.shared.open(Paths.config)
        } else {
            NSWorkspace.shared.open([Paths.config], withApplicationAt: URL(fileURLWithPath: "/System/Applications/TextEdit.app"),
                                    configuration: NSWorkspace.OpenConfiguration())
        }
    }

    @objc func restartBalancer() {
        daemon.stop { [weak self] code in
            self?.balancerStopped(code)
            if code == 0 { self?.startBalancer() }
        }
    }

    /// The config's text, read as the user (the daemon never opens user files), and checked
    /// by the built-in library so mistakes show here instead of as a failed start.
    func configText() -> Result<String, Problem> {
        guard FileManager.default.fileExists(atPath: Paths.config.path) else { return .success("") }
        if let problem = checkConfig(Paths.config.path) { return .failure(Problem("Config: " + problem)) }
        do {
            return .success(try String(contentsOf: Paths.config, encoding: .utf8))
        } catch {
            return .failure(Problem("Config: \(error.localizedDescription)"))
        }
    }

    @objc func startBalancer() {
        guard !ours else { return }
        let config: String
        switch configText() {
        case .success(let text): config = text
        case .failure(let p):
            log(p.message)
            lastError = p.message
            return changed()
        }
        if let problem = daemonReady() {
            log(problem)
            lastError = problem
            return changed()
        }
        let at = Date()
        (ours, lastError, needsRestart, appliedAt, startedAt) = (true, nil, nil, at, at)
        log("starting the balancer with \(Paths.config.path)")
        daemon.start(config: config, status: "\(url.host ?? "127.0.0.1"):\(url.port ?? 7878)") { [weak self] error in
            guard let self, let error else { return }
            log("couldn't start: \(error)")
            (ours, lastError, startedAt) = (false, "Couldn't start: \(error)", nil)
            changed()
        }
        // A daemon launchd can't spawn never answers: give up rather than show Starting… forever.
        DispatchQueue.main.asyncAfter(deadline: .now() + 20) { [weak self] in
            MainActor.assumeIsolated { self?.startTimedOut(at) }
        }
        changed()
    }

    func startTimedOut(_ at: Date) {
        guard startedAt == at, status == nil else { return }
        log("the Multilink service didn't answer within 20s")
        (ours, lastError, startedAt) = (false, "The Multilink service didn't answer (see log)", nil)
        daemon.disconnect()
        changed()
    }

    /// Register the daemon on first use. nil when it can be reached, else what the user must do.
    func daemonReady() -> String? {
        let service = SMAppService.daemon(plistName: daemonPlist)
        log("Multilink service status: \(service.status.rawValue)")
        if service.status == .enabled { return nil }
        if service.status != .requiresApproval {
            do {
                try service.register()
                return nil
            } catch where service.status != .requiresApproval
                && (error as NSError).code != Int(kSMErrorLaunchDeniedByUser) {
                return "Couldn't install the Multilink service: \(error.localizedDescription)"
            } catch {}
        }
        SMAppService.openSystemSettingsLoginItems()
        return "Allow Multilink in System Settings › Login Items, then Start again"
    }

    /// Send the config to the running balancer: policy, weights and pins apply at once.
    func applyConfig() {
        appliedAt = Date()
        switch configText() {
        case .success(let text):
            daemon.reload(config: text) { [weak self] code, message in
                guard let self else { return }
                switch code {
                case 0: (lastError, needsRestart) = (nil, nil)
                case 1: (lastError, needsRestart) = (nil, message)
                default: lastError = "Config not applied: \(message ?? "unknown error")"
                }
                changed()
            }
        case .failure(let p):
            lastError = p.message
        }
        changed()
    }

    func balancerStopped(_ code: Int) {
        log("the balancer stopped (code \(code))")
        (ours, needsRestart, startedAt) = (false, nil, nil)
        if code != 0 { lastError = "Balancer stopped with an error (see log)" }
        changed()
    }

    @objc func stopBalancer() {
        daemon.stop { [weak self] code in self?.balancerStopped(code) }
    }

    @objc func openLog() {
        for f in [Paths.appLog, Paths.log] where FileManager.default.fileExists(atPath: f.path) {
            NSWorkspace.shared.open(f)
        }
    }

    @objc func openPage() {
        NSWorkspace.shared.open(url)
    }

    @objc func quit() {
        NSApp.terminate(nil)
    }
}

struct Problem: Error {
    let message: String
    init(_ message: String) { self.message = message }
}

/// launchd label and XPC service name of the daemon; the plist is in Contents/Library/LaunchDaemons.
// Not com.multilink.bar.daemon: macOS kept that label's background item from MultilinkBar
// builds (old executable path), so it could never spawn the renamed app.
let daemonLabel = "com.multilink.helper"
let daemonPlist = daemonLabel + ".plist"

/// What the app may ask the daemon. Arguments are plain data that the daemon checks itself.
@objc protocol DaemonCalls {
    /// reply(nil) once balancing has started, else why not.
    func start(config: String, status: String, reply: @escaping (String?) -> Void)
    /// reply(code, message) as multilink_reload: 0 applied; 1 applied, message lists the
    /// settings that need a restart; -1 not applied, message says why.
    func reload(config: String, reply: @escaping (Int, String?) -> Void)
    /// reply(exit code) once the balancer has stopped and removed its routes (0 if none ran).
    func stop(reply: @escaping (Int) -> Void)
}

/// What the daemon tells the app: the balancer stopped without being asked to.
@objc protocol AppCalls {
    func stopped(code: Int)
}

/// A code requirement only this executable's code meets: its cdhash. nil when unsigned.
/// App and daemon are one executable, so each side requires exactly its own build.
func sameCodeRequirement() -> String? {
    var code: SecCode?
    var staticCode: SecStaticCode?
    var info: CFDictionary?
    guard SecCodeCopySelf([], &code) == errSecSuccess, let code,
          SecCodeCopyStaticCode(code, [], &staticCode) == errSecSuccess, let staticCode,
          SecCodeCopySigningInformation(staticCode, [], &info) == errSecSuccess,
          let cdhash = (info as? [String: Any])?[kSecCodeInfoUnique as String] as? Data
    else { return nil }
    return "cdhash H\"" + cdhash.map { String(format: "%02x", $0) }.joined() + "\""
}

/// The app's connection to the daemon. Replies arrive on the main actor. If the daemon can't
/// be reached, a call fails the way that is safe for it (start and reload report an error;
/// stop reports stopped, since the daemon stops the balancer when our connection goes).
@MainActor
final class DaemonClient: NSObject, AppCalls {
    private var connection: NSXPCConnection?
    private let onStopped: (Int) -> Void

    init(onStopped: @escaping (Int) -> Void) {
        self.onStopped = onStopped
    }

    nonisolated func stopped(code: Int) {
        Task { @MainActor in self.onStopped(code) }
    }

    func start(config: String, status: String, reply: @escaping (String?) -> Void) {
        call({ reply($0) }) { $0.start(config: config, status: status) { e in Task { @MainActor in reply(e) } } }
    }

    func reload(config: String, reply: @escaping (Int, String?) -> Void) {
        call({ reply(-1, $0) }) { $0.reload(config: config) { c, m in Task { @MainActor in reply(c, m) } } }
    }

    func stop(reply: @escaping (Int) -> Void) {
        call({ _ in reply(0) }) { $0.stop { c in Task { @MainActor in reply(c) } } }
    }

    /// Drop the connection (say, to a daemon that never answered); the next call makes a new one.
    func disconnect() {
        connection?.invalidate()
    }

    private func call(_ failed: @escaping @MainActor (String) -> Void, _ body: (DaemonCalls) -> Void) {
        guard let requirement = sameCodeRequirement() else {
            return failed("Multilink is not code signed")
        }
        let c = connection ?? connect(requirement)
        let proxy = c.remoteObjectProxyWithErrorHandler { error in
            Task { @MainActor in failed("can't reach the Multilink service (\(error.localizedDescription))") }
        }
        guard let daemon = proxy as? DaemonCalls else { return failed("bad XPC interface") }
        body(daemon)
    }

    private func connect(_ requirement: String) -> NSXPCConnection {
        let c = NSXPCConnection(machServiceName: daemonLabel, options: .privileged)
        c.remoteObjectInterface = NSXPCInterface(with: DaemonCalls.self)
        c.exportedInterface = NSXPCInterface(with: AppCalls.self)
        c.exportedObject = self
        c.setCodeSigningRequirement(requirement)
        // Daemon gone (crash, kill, other build): it took the balancer with it.
        c.invalidationHandler = { [weak self] in
            Task { @MainActor in
                self?.connection = nil
                self?.onStopped(0)
            }
        }
        c.interruptionHandler = { [weak c] in c?.invalidate() }
        c.resume()
        connection = c
        return c
    }
}

/// The root side, run by launchd. At most one balancer, owned by the connection that started
/// it: when that connection goes away (app quit or crashed) the balancer stops. Exits when
/// idle so launchd starts the current build next time.
final class Daemon: NSObject, NSXPCListenerDelegate, DaemonCalls, @unchecked Sendable {
    let requirement: String
    /// Every field below is only touched on this queue.
    let queue = DispatchQueue(label: daemonLabel)
    var running: OpaquePointer?
    var owner: NSXPCConnection?
    var stopReplies: [(Int) -> Void] = []
    var connections = 0
    var terminating = false

    init(requirement: String) {
        self.requirement = requirement
    }

    func listener(_ listener: NSXPCListener, shouldAcceptNewConnection c: NSXPCConnection) -> Bool {
        c.setCodeSigningRequirement(requirement)
        c.exportedInterface = NSXPCInterface(with: DaemonCalls.self)
        c.exportedObject = self
        c.remoteObjectInterface = NSXPCInterface(with: AppCalls.self)
        let id = ObjectIdentifier(c)
        c.invalidationHandler = { [weak self] in self?.queue.async { self?.gone(id) } }
        queue.sync { connections += 1 }
        c.resume()
        return true
    }

    func start(config: String, status: String, reply: @escaping (String?) -> Void) {
        let caller = NSXPCConnection.current()
        queue.async { [self] in
            guard running == nil else { return reply("the balancer is already running") }
            guard caller != nil, !terminating else { return reply("the service is shutting down") }
            var error: UnsafeMutablePointer<CChar>?
            guard let r = multilink_start(config, status, &error) else { return reply(take(error) ?? "unknown error") }
            (running, owner) = (r, caller)
            reply(nil)
            Thread.detachNewThread { [self] in
                let code = Int(multilink_wait(r))
                queue.async { self.finished(r, code) }
            }
        }
    }

    func reload(config: String, reply: @escaping (Int, String?) -> Void) {
        queue.async { [self] in
            guard let r = running else { return reply(-1, "the balancer is not running") }
            var message: UnsafeMutablePointer<CChar>?
            let code = multilink_reload(r, config, &message)
            reply(Int(code), take(message))
        }
    }

    func stop(reply: @escaping (Int) -> Void) {
        queue.async { [self] in
            guard let r = running else { return reply(0) }
            stopReplies.append(reply)
            multilink_stop(r)
        }
    }

    /// SIGTERM from launchd (unregister, shutdown): stop cleanly, then exit.
    func terminate() {
        queue.async { [self] in
            terminating = true
            if let r = running { multilink_stop(r) } else { exit(0) }
        }
    }

    private func finished(_ r: OpaquePointer, _ code: Int) {
        multilink_release(r)
        running = nil
        let replies = stopReplies
        stopReplies = []
        // Asked to stop: the reply says so. Otherwise tell the owner it stopped by itself.
        if replies.isEmpty { (owner?.remoteObjectProxy as? AppCalls)?.stopped(code: code) }
        replies.forEach { $0(code) }
        owner = nil
        exitIfIdle()
    }

    private func gone(_ id: ObjectIdentifier) {
        connections -= 1
        if let owner, ObjectIdentifier(owner) == id, let r = running {
            log("the app's connection closed: stopping the balancer")
            multilink_stop(r)
        }
        exitIfIdle()
    }

    private func exitIfIdle() {
        if running == nil, connections == 0 || terminating { exit(0) }
    }
}

/// A string returned by the library, freed.
func take(_ s: UnsafeMutablePointer<CChar>?) -> String? {
    guard let s else { return nil }
    defer { multilink_free_string(s) }
    return String(cString: s)
}

/// A log line, timestamped like the library's (the daemon's to /var/log, the app's to ~/Library/Logs).
func log(_ text: String) {
    let f = DateFormatter()
    f.dateFormat = "yyyy-MM-dd HH:mm:ss"
    print("\(f.string(from: Date())) \(text)")
    fflush(stdout)
}

/// Run as the daemon (root, started by launchd through the app's SMAppService registration).
func runDaemon() -> Never {
    guard getuid() == 0 else {
        FileHandle.standardError.write(Data("daemon mode is started by launchd as root\n".utf8))
        exit(1)
    }
    // Only root can write /var/log, so nothing can plant a symlink there; O_NOFOLLOW anyway.
    let fd = open(Paths.log.path, O_WRONLY | O_APPEND | O_CREAT | O_NOFOLLOW, 0o644)
    if fd >= 0 {
        dup2(fd, STDOUT_FILENO)
        dup2(fd, STDERR_FILENO)
        close(fd)
    }
    guard let requirement = sameCodeRequirement() else {
        log("Multilink is not code signed: refusing every connection")
        exit(1)
    }
    let daemon = Daemon(requirement: requirement)
    signal(SIGTERM, SIG_IGN)
    let term = DispatchSource.makeSignalSource(signal: SIGTERM, queue: daemon.queue)
    term.setEventHandler { daemon.terminate() }
    term.resume()
    let listener = NSXPCListener(machServiceName: daemonLabel)
    listener.delegate = daemon
    listener.resume()
    withExtendedLifetime((term, listener)) { dispatchMain() }
}

enum Paths {
    static let home = FileManager.default.homeDirectoryForCurrentUser
    static let support = home.appending(path: "Library/Application Support/Multilink")
    /// Written by the daemon as root.
    static let log = URL(fileURLWithPath: "/var/log/multilink.log")
    /// Written by the app: what happens before the daemon answers.
    static let appLog = home.appending(path: "Library/Logs/Multilink.log")
    static var config: URL {
        UserDefaults.standard.string(forKey: "configPath").map { URL(fileURLWithPath: $0) }
            ?? support.appending(path: "multilink.toml")
    }
}

let policies = [("sticky", "Sticky"), ("round-robin", "Round Robin"), ("least-latency", "Least Latency")]
let policyLine = try! NSRegularExpression(pattern: #"^[ \t]*policy[ \t]*=[ \t]*"([a-z-]*)"[^\n]*$"#, options: .anchorsMatchLines)

/// The policy a config sets; multilink's default when it sets none.
func configPolicy(_ text: String) -> String {
    let ns = text as NSString
    guard let m = policyLine.firstMatch(in: text, range: NSRange(location: 0, length: ns.length)) else { return "sticky" }
    return ns.substring(with: m.range(at: 1))
}

/// `text` with its policy line set to `policy` (added at the top if it has none). Other lines are kept.
func withPolicy(_ text: String, _ policy: String) -> String {
    let ns = text as NSString
    let line = "policy = \"\(policy)\""
    guard let m = policyLine.firstMatch(in: text, range: NSRange(location: 0, length: ns.length)) else { return line + "\n" + text }
    return ns.replacingCharacters(in: m.range, with: line)
}

func modified(_ url: URL) -> Date {
    ((try? FileManager.default.attributesOfItem(atPath: url.path))?[.modificationDate] as? Date) ?? .distantPast
}

func ensureConfig() throws {
    guard !FileManager.default.fileExists(atPath: Paths.config.path) else { return }
    try FileManager.default.createDirectory(at: Paths.config.deletingLastPathComponent(), withIntermediateDirectories: true)
    try configTemplate.write(to: Paths.config, atomically: true, encoding: .utf8)
}

let configTemplate = """
# multilink settings used by the Multilink menu bar app. Saving applies policy, weights and
# pins to new connections at once; the app offers a restart for the others.
# Every option is described in multilink.example.toml in the multilink repository.

# sticky: a destination keeps its link (logins tied to an IP survive)
# round-robin: each new connection takes the next link
# least-latency: everything uses the fastest healthy link
policy = "sticky"

# Health check: TCP connect through each link every 2 seconds.
# probe = "1.1.1.1:443"

# Use exactly these interfaces instead of auto-detecting:
# links = ["en0", "en9"]

# Never use these when auto-detecting:
# exclude = ["bridge0"]

# Sticky only: relative share of destinations per link (unlisted links weigh 1):
# [weights]
# en0 = 3

# Prefer a link for an address range while that link is healthy:
# [[pin]]
# dst = "17.0.0.0/8"
# link = "en0"

"""

/// The library's config check: nil when valid, else a one-line reason.
func checkConfig(_ path: String) -> String? {
    take(multilink_check_config(path))
}

func fetch(_ base: URL) async -> Status? {
    var req = URLRequest(url: base.appendingPathComponent("api/status"))
    req.timeoutInterval = 0.8
    req.cachePolicy = .reloadIgnoringLocalCacheData
    guard let (data, _) = try? await URLSession.shared.data(for: req) else { return nil }
    return try? JSONDecoder().decode(Status.self, from: data)
}

let url = URL(string: UserDefaults.standard.string(forKey: "statusURL") ?? "http://127.0.0.1:7878")!

// Run by launchd as root (Contents/Library/LaunchDaemons): no AppKit at all.
if CommandLine.arguments.count > 1, CommandLine.arguments[1] == "daemon" {
    runDaemon()
}

if CommandLine.arguments.contains("--selftest") {
    // XPC peers are pinned to this build's cdhash.
    let requirement = sameCodeRequirement()
    precondition(requirement?.hasPrefix("cdhash H\"") == true, "no cdhash: \(String(describing: requirement))")
    var parsed: SecRequirement?
    precondition(SecRequirementCreateWithString(requirement! as CFString, [], &parsed) == errSecSuccess, "bad requirement")
    var me: SecCode?
    SecCodeCopySelf([], &me)
    precondition(SecCodeCheckValidity(me!, [], parsed) == errSecSuccess, "this build fails its own requirement")
    // Policy edits touch only the policy line.
    let cfg = "# top\npolicy = \"sticky\"  # note\nprobe = \"1.1.1.1:443\"\n[weights]\nen0 = 3\n"
    precondition(configPolicy(cfg) == "sticky")
    let edited = withPolicy(cfg, "least-latency")
    precondition(edited == cfg.replacingOccurrences(of: "policy = \"sticky\"  # note", with: "policy = \"least-latency\""), edited)
    precondition(configPolicy(edited) == "least-latency")
    precondition(configPolicy("# policy = \"round-robin\"\n") == "sticky", "commented line counts")
    precondition(withPolicy("probe = \"x\"\n", "round-robin") == "policy = \"round-robin\"\nprobe = \"x\"\n")
    precondition(configPolicy(configTemplate) == "sticky")
    // The embedded library checks configs.
    let tmp = FileManager.default.temporaryDirectory.appending(path: "multilink-selftest.toml")
    try! "polcy = \"x\"\n".write(to: tmp, atomically: true, encoding: .utf8)
    precondition(checkConfig(tmp.path)?.contains("unknown field `polcy`") == true, "\(String(describing: checkConfig(tmp.path)))")
    try! configTemplate.write(to: tmp, atomically: true, encoding: .utf8)
    precondition(checkConfig(tmp.path) == nil, "template rejected: \(checkConfig(tmp.path) ?? "")")
    try? FileManager.default.removeItem(at: tmp)
    // The open menu must keep its items across refreshes (replacing them made Settings flash).
    MainActor.assumeIsolated {
        _ = NSApplication.shared
        func status(_ down: UInt64, _ names: [String]) -> Status {
            let links = names.map { #"{"name":"\#($0)","ipv4":"10.0.0.1","gateway":null,"up":true,"rtt_ms":5,"present":true,"active":1,"flows":2,"bytes_up":0,"bytes_down":\#(down)}"# }
            return try! JSONDecoder().decode(Status.self, from: Data(#"{"policy":"sticky","uptime_s":1,"links":[\#(links.joined(separator: ","))]}"#.utf8))
        }
        let bar = Bar(url: url)
        bar.status = status(0, ["en0", "en9"])
        bar.menuWillOpen(bar.menu)
        let before = bar.menu.items
        bar.currentRates = ["en0": (5e6, 1e3)]
        bar.refresh()
        precondition(bar.menu.items.count == before.count && zip(bar.menu.items, before).allSatisfy { $0 === $1 }, "items replaced on refresh")
        precondition(bar.menu.items.contains { $0.title.contains("5.0 MB/s") }, "rates not updated in place")
        precondition(bar.menu.items.contains { $0 === bar.settingsItem })
        // A new link changes the layout: rebuilt, but not while Settings is open.
        bar.settingsOpen = true
        bar.status = status(0, ["en0", "en9", "en5"])
        bar.refresh()
        precondition(zip(bar.menu.items, before).allSatisfy { $0 === $1 }, "rebuilt while Settings open")
        bar.settingsOpen = false
        bar.refresh()
        precondition(bar.menu.items.contains { $0.title.contains("en5") }, "layout change not applied")
        precondition(bar.menu.items.contains { $0 === bar.settingsItem }, "Settings item recreated")
        // Restart is offered only for settings the daemon said need one.
        let restart = { bar.menu.items.first { $0.title.hasPrefix("Restart") }?.title }
        bar.ours = true
        bar.refresh()
        precondition(restart() == nil && bar.menu.items.contains { $0.title == "Stop Balancer" }, "restart offered with nothing to restart")
        bar.needsRestart = "probe, status"
        bar.refresh()
        precondition(restart() == "Restart Balancer to Apply probe, status", "restart not offered: \(String(describing: restart()))")
        bar.balancerStopped(0)
        bar.status = nil
        bar.refresh()
        precondition(restart() == nil && bar.menu.items.contains { $0.title == "Start Balancer" }, "still running after stop")
    }
    print("selftest ok")
    exit(0)
}

if CommandLine.arguments.contains("--print") {
    // Two samples a second apart, so rates are real.
    let done = DispatchSemaphore(value: 0)
    Task {
        let a = await fetch(url)
        let t = Date()
        try? await Task.sleep(for: .seconds(1))
        let b = await fetch(url)
        let r = b.map { rates(prev: a, prevAt: t, now: $0, at: Date()) } ?? [:]
        print("icon: \(overall(b).symbol.name)")
        for row in menuRows(b, r) { print(row.text) }
        done.signal()
    }
    done.wait()
    exit(0)
}

// Root is the daemon's job. A GUI running as root (say, through an old sudoers rule) would
// hand root to everything it opens.
guard getuid() != 0 else {
    FileHandle.standardError.write(Data("Multilink must not run as root\n".utf8))
    exit(1)
}

// Builds named MultilinkBar kept their files here; never overwrites an existing folder.
try? FileManager.default.moveItem(at: Paths.home.appending(path: "Library/Application Support/MultilinkBar"), to: Paths.support)

// The app's own log, for what happens before the daemon answers.
let appLog = open(Paths.appLog.path, O_WRONLY | O_APPEND | O_CREAT | O_NOFOLLOW, 0o644)
if appLog >= 0 {
    dup2(appLog, STDOUT_FILENO)
    dup2(appLog, STDERR_FILENO)
    close(appLog)
}
log("Multilink \(Bundle.main.infoDictionary?["CFBundleVersion"] as? String ?? "?") started")

// Top-level code runs on the main thread.
MainActor.assumeIsolated {
    let app = NSApplication.shared
    let bar = Bar(url: url)
    app.delegate = bar
    app.setActivationPolicy(.accessory)
    app.run()
}
