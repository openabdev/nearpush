import Foundation

/// Turn-end classifier providers the daemon accepts. Raw values match the
/// `classifier` key of `POST /control` and `provider` in config.toml.
public enum ClassifierProvider: String, CaseIterable, Codable, Sendable {
    case kiro
    case openrouter
    case typesafe

    public var title: String {
        switch self {
        case .kiro: return "Kiro · Haiku 5.5"
        case .openrouter: return "OpenRouter · Jev 1.13"
        case .typesafe: return "TypeSafe · Jev 1.13.0"
        }
    }

    /// Model id the daemon sends (never an alias).
    public var model: String {
        switch self {
        case .kiro: return "claude-haiku-5.5"
        case .openrouter: return "typesafe/jev-1.13"
        case .typesafe: return "jev-1.13.0"
        }
    }

    public var keychainService: String {
        switch self {
        case .kiro: return "nearpush-kiro-api-key"
        case .openrouter: return "nearpush-openrouter-api-key"
        case .typesafe: return "nearpush-typesafe-api-key"
        }
    }

    /// Terminal command that stores this provider's key in the login keychain.
    /// It holds no key value: `-w` makes `security` prompt for it.
    public static func keychainHint(_ p: ClassifierProvider) -> String {
        "security add-generic-password -s \(p.keychainService) -a \"$USER\" -w"
    }
}
