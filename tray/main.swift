// micro-tray: menu bar switch for who owns the Codex Micro pad.
//
// microd (the device daemon) always runs — reading pad input is shared and
// conflict-free. The only contention is LED writes, so "ownership" simply
// means whether herdr-bridge is active:
//   - herdr owns the pad  -> herdr-bridge launchd agent loaded
//   - Codex owns the pad  -> herdr-bridge unloaded (ChatGPT app drives LEDs)
//
// Modes: Auto (default; Codex owns the pad while the ChatGPT app is running,
// herdr otherwise), or forced herdr / Codex. The icon shows the current owner.

import AppKit

let bridgeLabel = "com.twick.herdr-bridge"
let bridgePlist = NSString(string: "~/Library/LaunchAgents/com.twick.herdr-bridge.plist")
    .expandingTildeInPath
let microdSocket = NSString(string: "~/.cache/microd/microd.sock").expandingTildeInPath
let uid = getuid()

enum Mode: String {
    case auto, herdr, codex
}

@discardableResult
func sh(_ args: [String]) -> Int32 {
    let p = Process()
    p.executableURL = URL(fileURLWithPath: args[0])
    p.arguments = Array(args.dropFirst())
    p.standardOutput = FileHandle.nullDevice
    p.standardError = FileHandle.nullDevice
    do { try p.run() } catch { return -1 }
    p.waitUntilExit()
    return p.terminationStatus
}

func bridgeLoaded() -> Bool {
    sh(["/bin/launchctl", "print", "gui/\(uid)/\(bridgeLabel)"]) == 0
}

func chatGPTRunning() -> Bool {
    NSWorkspace.shared.runningApplications.contains {
        $0.bundleIdentifier == "com.openai.chat" || $0.localizedName == "ChatGPT"
    }
}

func startBridge() {
    sh(["/bin/launchctl", "bootstrap", "gui/\(uid)", bridgePlist])
}

func stopBridge() {
    sh(["/bin/launchctl", "bootout", "gui/\(uid)/\(bridgeLabel)"])
    // Leave the Agent Keys dark for the next owner.
    sh(["/bin/sh", "-c",
        "printf '{\"cmd\":\"clear\"}\\n' | /usr/bin/nc -U \(microdSocket) -w 1"])
}

final class AppDelegate: NSObject, NSApplicationDelegate, NSMenuDelegate {
    var statusItem: NSStatusItem!
    var lastDesired: Bool?
    var mode: Mode = Mode(rawValue: UserDefaults.standard.string(forKey: "mode") ?? "auto") ?? .auto

    let autoItem = NSMenuItem(title: "Auto (Codex while ChatGPT is open)",
                              action: #selector(setAuto), keyEquivalent: "")
    let herdrItem = NSMenuItem(title: "herdr", action: #selector(setHerdr), keyEquivalent: "")
    let codexItem = NSMenuItem(title: "Codex", action: #selector(setCodex), keyEquivalent: "")
    let statusLine = NSMenuItem(title: "Pad: …", action: nil, keyEquivalent: "")

    func applicationDidFinishLaunching(_ note: Notification) {
        statusItem = NSStatusBar.system.statusItem(withLength: NSStatusItem.variableLength)

        let menu = NSMenu()
        menu.delegate = self
        statusLine.isEnabled = false
        menu.addItem(statusLine)
        menu.addItem(NSMenuItem.separator())
        for item in [autoItem, herdrItem, codexItem] {
            item.target = self
            menu.addItem(item)
        }
        menu.addItem(NSMenuItem.separator())
        let quit = NSMenuItem(title: "Quit micro-tray", action: #selector(quit), keyEquivalent: "q")
        quit.target = self
        menu.addItem(quit)
        statusItem.menu = menu

        // Event-driven: reconcile only when an app launches/quits (to catch
        // ChatGPT) — no polling, no idle CPU.
        let center = NSWorkspace.shared.notificationCenter
        for name in [NSWorkspace.didLaunchApplicationNotification,
                     NSWorkspace.didTerminateApplicationNotification] {
            center.addObserver(forName: name, object: nil, queue: .main) { [weak self] _ in
                self?.apply()
            }
        }
        apply(force: true)
    }

    /// Reconcile the desired owner with reality and refresh the icon. Only
    /// touches launchctl when ownership actually changes (commands are
    /// idempotent: bootstrap/bootout of an already-correct state is a no-op
    /// error we ignore).
    func apply(force: Bool = false) {
        let desiredHerdr: Bool
        switch mode {
        case .auto: desiredHerdr = !chatGPTRunning()
        case .herdr: desiredHerdr = true
        case .codex: desiredHerdr = false
        }
        if force || desiredHerdr != lastDesired {
            if desiredHerdr {
                startBridge()
            } else {
                stopBridge()
            }
            lastDesired = desiredHerdr
        }
        let owner = desiredHerdr ? "H" : "C"
        statusItem.button?.title = "⌨\(owner)"
        statusLine.title = desiredHerdr ? "Pad: herdr" : "Pad: Codex"
    }

    func menuWillOpen(_ menu: NSMenu) {
        autoItem.state = mode == .auto ? .on : .off
        herdrItem.state = mode == .herdr ? .on : .off
        codexItem.state = mode == .codex ? .on : .off
        apply()
    }

    func setMode(_ m: Mode) {
        mode = m
        UserDefaults.standard.set(m.rawValue, forKey: "mode")
        apply(force: true)
    }

    @objc func setAuto() { setMode(.auto) }
    @objc func setHerdr() { setMode(.herdr) }
    @objc func setCodex() { setMode(.codex) }
    @objc func quit() { NSApp.terminate(nil) }
}

let app = NSApplication.shared
app.setActivationPolicy(.accessory)
let delegate = AppDelegate()
app.delegate = delegate
app.run()
