import Foundation

/// What `tailscale serve status --json` says about the daemon's port.
public enum ServeStatus: Equatable, Sendable {
    /// A web handler proxies to the daemon; the URL the iPhone opens.
    case mapped(String)
    /// Serve is configured, but nothing points at the daemon. Never touched.
    case otherConfig
    /// No serve config at all; the app may offer `tailscale serve --bg`.
    case empty
    /// tailscale missing or the command failed.
    case unknown(String)

    /// Parses the JSON. `port` is the daemon's loopback port.
    public static func parse(_ data: Data, port: Int = 8787) -> ServeStatus {
        let trimmed = String(decoding: data, as: UTF8.self).trimmingCharacters(in: .whitespacesAndNewlines)
        if trimmed.isEmpty || trimmed == "{}" || trimmed == "null" { return .empty }
        guard let obj = try? JSONSerialization.jsonObject(with: data) as? [String: Any] else {
            return .unknown("unreadable `tailscale serve status --json` output")
        }
        let targets: Set<String> = ["127.0.0.1:\(port)", "localhost:\(port)"]
        let web = obj["Web"] as? [String: Any] ?? [:]
        for hostPort in web.keys.sorted() {
            guard let handlers = (web[hostPort] as? [String: Any])?["Handlers"] as? [String: Any] else { continue }
            for path in handlers.keys.sorted() {
                guard var proxy = (handlers[path] as? [String: Any])?["Proxy"] as? String else { continue }
                for scheme in ["http://", "https+insecure://", "https://"] where proxy.hasPrefix(scheme) {
                    proxy.removeFirst(scheme.count)
                }
                while proxy.hasSuffix("/") { proxy.removeLast() }
                guard targets.contains(proxy) else { continue }
                var host = hostPort
                if host.hasSuffix(":443") { host.removeLast(4) }
                let p = path.hasPrefix("/") ? path : "/" + path
                return .mapped("https://\(host)\(p)")
            }
        }
        let tcp = obj["TCP"] as? [String: Any] ?? [:]
        let funnel = obj["AllowFunnel"] as? [String: Any] ?? [:]
        return web.isEmpty && tcp.isEmpty && funnel.isEmpty ? .empty : .otherConfig
    }
}
