import Foundation

/// Whether the menu bar icon is shown. Stored in UserDefaults; on by default.
public enum MenuIconSetting {
    public static let key = "showMenuBarIcon"

    public static func isOn(_ defaults: UserDefaults) -> Bool {
        defaults.object(forKey: key) == nil ? true : defaults.bool(forKey: key)
    }

    public static func set(_ on: Bool, _ defaults: UserDefaults) {
        defaults.set(on, forKey: key)
    }
}

/// The "Show on iPhone" rows. Generic events have no state, so they have no row:
/// the daemon's `Event::visible()` shows them whatever the filter says.
public enum ShowOnPhone {
    public static let rows: [AgentState] = [.allSet, .yourTurn, .allow, .working]

    public static func isShown(_ state: AgentState, filter: [String]) -> Bool {
        filter.contains(state.rawValue)
    }

    /// The filter with one state turned on or off. Order is kept, unknown names are kept.
    public static func toggled(_ filter: [String], _ state: AgentState, on: Bool) -> [String] {
        var next = filter.filter { $0 != state.rawValue }
        if on { next.append(state.rawValue) }
        return next
    }
}

/// The settings window's Summarizer toggle (session labels). The daemon's
/// `/status summarizer.enabled` wins; before the first status it falls back to
/// `[summarizer] enabled` read from config.toml.
public enum SummarizerToggle {
    public static func isOn(_ summarizer: SummarizerStatus?, inConfig: Bool) -> Bool {
        summarizer?.enabled ?? inConfig
    }
}

/// Maps the daemon's classifier status to the classifier picker.
public enum ProviderPicker {
    public static func selected(_ classifier: ClassifierStatus?) -> ClassifierProvider? {
        classifier.flatMap { ClassifierProvider(rawValue: $0.provider) }
    }

    public static func isEnabled(_ classifier: ClassifierStatus?) -> Bool {
        classifier?.enabled == true
    }
}
