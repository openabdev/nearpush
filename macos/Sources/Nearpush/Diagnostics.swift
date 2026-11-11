import Foundation
import NearpushCore

struct Check: Identifiable, Equatable {
    let id: String
    let ok: Bool
    let title: String
    /// What to do when it fails.
    let hint: String?
}

enum Diagnostics {
    static let keychainService = "nearpush-kiro-api-key"

    /// `tailscale serve status --json`, parsed. Runs off the main actor.
    static func serveStatus() async -> ServeStatus {
        guard let bin = Tailscale.binary() else { return .unknown("tailscale CLI not found") }
        guard let out = await ProcessRunner.run(bin, ["serve", "status", "--json"], timeout: 5) else {
            return .unknown("cannot run \(bin)")
        }
        if out.timedOut { return .unknown("tailscale serve status timed out") }
        if out.status != 0 { return .unknown("tailscale serve status exited \(out.status)") }
        return ServeStatus.parse(out.stdout)
    }

    /// True when the Keychain item exists. Only the exit status is used: no `-w`,
    /// stdout and stderr go to /dev/null, so the key is never read or shown.
    static func keychainKeyPresent() async -> Bool {
        let out = await ProcessRunner.run("/usr/bin/security", ["find-generic-password", "-s", keychainService],
                                          timeout: 5, captureStdout: false)
        return out?.status == 0 && out?.timedOut == false
    }

    static func run(result: StatusResult?, tokenURL: URL) async -> (checks: [Check], serve: ServeStatus) {
        async let serve = serveStatus()
        async let key = keychainKeyPresent()
        var checks: [Check] = []

        switch result {
        case .ok(let s)?:
            checks.append(Check(id: "daemon", ok: true, title: "Daemon reachable (nearpushd \(s.version), pid \(s.pid))", hint: nil))
        case .unauthorized?:
            checks.append(Check(id: "daemon", ok: false, title: "Daemon rejects the token",
                                hint: "Another nearpushd with a different token owns 8787, or the token file changed. Restart the daemon."))
        case .external(let why)?:
            checks.append(Check(id: "daemon", ok: false, title: "External daemon on 8787: \(why)",
                                hint: "Stop it: launchctl bootout gui/$(id -u)/dev.nearpush.daemon"))
        case .unreachable?, nil:
            checks.append(Check(id: "daemon", ok: false, title: "Daemon not reachable on 127.0.0.1:8787",
                                hint: "Choose Restart daemon, then Open logs if it keeps failing."))
        case .noToken(let why)?:
            checks.append(Check(id: "daemon", ok: false, title: "No push token: \(why)",
                                hint: "nearpushd creates it on first start; choose Restart daemon."))
        }

        let tokenOK = FileManager.default.isReadableFile(atPath: tokenURL.path)
        checks.append(Check(id: "token", ok: tokenOK, title: tokenOK ? "Token file readable" : "Token file missing",
                            hint: tokenOK ? nil : "\(tokenURL.path) is created by nearpushd on first start."))

        let sv = await serve
        switch sv {
        case .mapped(let url):
            checks.append(Check(id: "serve", ok: true, title: "tailscale serve: \(url) → 8787", hint: nil))
        case .empty:
            checks.append(Check(id: "serve", ok: false, title: "tailscale serve: no config",
                                hint: "Run `tailscale serve --bg http://127.0.0.1:8787`, or choose Set up tailscale serve."))
        case .otherConfig:
            checks.append(Check(id: "serve", ok: false, title: "tailscale serve: nothing points at 127.0.0.1:8787",
                                hint: "A serve config exists for something else; add a handler yourself (the app never overwrites it)."))
        case .unknown(let why):
            checks.append(Check(id: "serve", ok: false, title: "tailscale serve: \(why)",
                                hint: "Install Tailscale and log in, then check again."))
        }

        let hasKey = await key
        checks.append(Check(id: "keychain", ok: hasKey,
                            title: hasKey ? "Keychain key present (\(keychainService))" : "Keychain key missing (\(keychainService))",
                            hint: hasKey ? nil : "The summarizer needs it: security add-generic-password -s \(keychainService) -a \"$USER\" -w"))
        if let cl = result?.status?.classifier {
            checks.append(classifierCheck(cl))
        }
        return (checks, sv)
    }

    /// Reads only `keyPresent` from the daemon's `/status`. No key value is read here.
    static func classifierCheck(_ cl: ClassifierStatus) -> Check {
        let provider = ClassifierProvider(rawValue: cl.provider)
        let name = provider?.title ?? cl.provider
        guard cl.enabled else {
            return Check(id: "classifier", ok: false, title: "Classifier: \(name) — disabled",
                         hint: "Set [classifier] enabled = true in config.toml and restart the daemon")
        }
        switch cl.keyPresent {
        case true?:
            return Check(id: "classifier", ok: true, title: "Classifier: \(name) — key present", hint: nil)
        case false?:
            return Check(id: "classifier", ok: false, title: "Classifier: \(name) — key missing (\(cl.keyService))",
                         hint: provider.map(ClassifierProvider.keychainHint))
        case nil:
            return Check(id: "classifier", ok: true, title: "Classifier: \(name) — not checked yet", hint: nil)
        }
    }
}
