import Foundation

/// Outcome of probing `GET /status`.
public enum StatusResult: Equatable, Sendable {
    case ok(DaemonStatus)
    /// Something answered HTTP on the port but not with our `/status` (e.g. the
    /// pre-menubar nearpushd answers 404).
    case external(String)
    /// The token was rejected: another daemon with a different token, or a stale file.
    case unauthorized
    /// Nothing listening (or no answer within the timeout).
    case unreachable(String)
    /// `~/.config/nearpush/token` is missing or empty.
    case noToken(String)

    public var status: DaemonStatus? {
        if case .ok(let s) = self { return s }
        return nil
    }
}

public enum ClientError: Error, LocalizedError {
    case noToken(String)
    case http(Int, String)

    public var errorDescription: String? {
        switch self {
        case .noToken(let m): return m
        case .http(let code, let m): return "nearpushd answered \(code): \(m)"
        }
    }
}

/// Talks to nearpushd on loopback with the push token.
public final class DaemonClient: @unchecked Sendable {
    public static let defaultBase = URL(string: "http://127.0.0.1:8787")!

    public static var defaultTokenURL: URL {
        FileManager.default.homeDirectoryForCurrentUser.appendingPathComponent(".config/nearpush/token")
    }

    public static let decoder: JSONDecoder = {
        let d = JSONDecoder()
        d.keyDecodingStrategy = .convertFromSnakeCase
        return d
    }()

    public let base: URL
    public let tokenURL: URL
    private let session: URLSession

    public init(base: URL = DaemonClient.defaultBase, tokenURL: URL = DaemonClient.defaultTokenURL) {
        self.base = base
        self.tokenURL = tokenURL
        let cfg = URLSessionConfiguration.ephemeral
        cfg.timeoutIntervalForRequest = 2
        cfg.timeoutIntervalForResource = 4
        cfg.connectionProxyDictionary = [:] // never through a proxy: the daemon rejects proxied requests
        cfg.requestCachePolicy = .reloadIgnoringLocalCacheData
        session = URLSession(configuration: cfg)
    }

    /// The trimmed token, or `nil` when the file is missing, unreadable or empty.
    public func readToken() -> String? {
        guard let data = try? Data(contentsOf: tokenURL),
              let s = String(data: data, encoding: .utf8)?.trimmingCharacters(in: .whitespacesAndNewlines),
              !s.isEmpty else { return nil }
        return s
    }

    /// Maps an HTTP answer from `/status` to a result. Pure, for tests.
    public static func classify(statusCode: Int, data: Data) -> StatusResult {
        switch statusCode {
        case 200:
            if let s = try? decoder.decode(DaemonStatus.self, from: data) { return .ok(s) }
            return .external("an unknown server answers /status")
        case 401:
            return .unauthorized
        case 404:
            return .external("an older nearpushd without /status")
        default:
            return .external("HTTP \(statusCode) from /status")
        }
    }

    public func status() async -> StatusResult {
        guard let token = readToken() else {
            // Still probe the port: any HTTP answer (401 from a current daemon,
            // 404 from an older one) means someone else already owns it.
            let req = URLRequest(url: base.appendingPathComponent("status"))
            if let (_, resp) = try? await session.data(for: req), resp is HTTPURLResponse {
                return .external("a daemon answers, but \(tokenURL.path) is missing or empty")
            }
            return .noToken("\(tokenURL.path) is missing or empty")
        }
        var req = URLRequest(url: base.appendingPathComponent("status"))
        req.setValue("Bearer \(token)", forHTTPHeaderField: "Authorization")
        do {
            let (data, resp) = try await session.data(for: req)
            let code = (resp as? HTTPURLResponse)?.statusCode ?? 0
            return DaemonClient.classify(statusCode: code, data: data)
        } catch {
            return .unreachable(error.localizedDescription)
        }
    }

    public func setPaused(_ paused: Bool) async throws -> ControlResponse {
        try await control(["paused": paused])
    }

    public func setStates(_ states: [String]) async throws -> ControlResponse {
        try await control(["states": states])
    }

    public func setClassifier(_ p: ClassifierProvider) async throws -> ControlResponse {
        try await control(["classifier": p.rawValue])
    }

    private func control(_ body: [String: Any]) async throws -> ControlResponse {
        guard let token = readToken() else { throw ClientError.noToken("\(tokenURL.path) is missing or empty") }
        var req = URLRequest(url: base.appendingPathComponent("control"))
        req.httpMethod = "POST"
        req.setValue("Bearer \(token)", forHTTPHeaderField: "Authorization")
        req.setValue("application/json", forHTTPHeaderField: "Content-Type")
        req.httpBody = try JSONSerialization.data(withJSONObject: body)
        let (data, resp) = try await session.data(for: req)
        let code = (resp as? HTTPURLResponse)?.statusCode ?? 0
        guard code == 200 else {
            let msg = (try? JSONSerialization.jsonObject(with: data) as? [String: Any])?["error"] as? String
            throw ClientError.http(code, msg ?? String(decoding: data, as: UTF8.self))
        }
        return try DaemonClient.decoder.decode(ControlResponse.self, from: data)
    }
}
