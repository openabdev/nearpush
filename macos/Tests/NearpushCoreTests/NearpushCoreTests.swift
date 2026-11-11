import Combine
import ServiceManagement
import XCTest
@testable import Nearpush
@testable import NearpushCore

/// Same shape as nearpushd's `GET /status` (crates/nearpushd/src/server.rs `StatusView`).
let statusFixture = """
{"version":"0.1.0","ui_version":"0.1.0-abc","pid":4242,"exe":"/Users/example/Applications/Nearpush.app/Contents/MacOS/nearpushd",
 "started_at_ms":1700000000000,"uptime_secs":61,"viewers":1,"last_event_id":12,"paused":false,
 "filter":{"states":["all_set","your_turn","allow"],"source":"config"},
 "summarizer":{"enabled":true,"healthy":true,"last_ok_ms":5,"last_error_ms":0},
 "public_url":"https://laptop.example.ts.net/",
 "recent":[{"id":12,"ts":1,"title":"all set.","state":"all_set","level":"info","session":"s1","detail":"took 3m","shown":true},
           {"id":11,"ts":1,"title":"working.","state":"working","level":"info","shown":false},
           {"id":10,"ts":1,"title":"build ok","level":"warn","shown":true}]}
"""

final class StatusTests: XCTestCase {
    func testDecodesStatusFixture() throws {
        let r = DaemonClient.classify(statusCode: 200, data: Data(statusFixture.utf8))
        guard case .ok(let s) = r else { return XCTFail("\(r)") }
        XCTAssertEqual(s.pid, 4242)
        XCTAssertEqual(s.lastEventId, 12)
        XCTAssertEqual(s.viewers, 1)
        XCTAssertEqual(s.filter.states, ["all_set", "your_turn", "allow"])
        XCTAssertEqual(s.summarizer.lastOkMs, 5)
        XCTAssertEqual(s.publicUrl, "https://laptop.example.ts.net/")
        XCTAssertEqual(s.recent.count, 3)
        XCTAssertEqual(s.recent[0].menuLine, "#12 all set — took 3m")
        XCTAssertEqual(s.recent[1].menuLine, "#11 working (hidden)")
        XCTAssertEqual(s.recent[2].menuLine, "#10 build ok")
    }

    func testClassifiesOtherAnswers() {
        XCTAssertEqual(DaemonClient.classify(statusCode: 401, data: Data()), .unauthorized)
        if case .external = DaemonClient.classify(statusCode: 404, data: Data("{\"error\":\"x\"}".utf8)) {} else { XCTFail("404") }
        if case .external = DaemonClient.classify(statusCode: 200, data: Data("<html>".utf8)) {} else { XCTFail("html") }
    }

    func testControlResponseDecodes() throws {
        let c = try DaemonClient.decoder.decode(ControlResponse.self,
            from: Data(#"{"paused":true,"filter":{"states":["working"],"source":"control"}}"#.utf8))
        XCTAssertTrue(c.paused)
        XCTAssertEqual(c.filter.source, "control")
    }
}

/// Drives the real `AppModel` publish routing (`applyPoll`, `applyDiagnostics`, `menuDidOpen`,
/// `menuDidClose`), the same methods the app's poll and diagnostics loops call.
@MainActor
final class MenuGateTests: XCTestCase {
    private let ok = DaemonClient.classify(statusCode: 200, data: Data(statusFixture.utf8))

    func testPollDoesNotPublishWhileMenuIsOpen() {
        let model = AppModel()
        var publishes = 0
        let cancel = model.objectWillChange.sink { publishes += 1 }
        model.menuDidOpen()
        model.applyPoll(ok, login: .notRegistered)
        model.applyPoll(.unauthorized, login: .notRegistered)
        model.applyPoll(.unauthorized, login: .notRegistered)
        XCTAssertEqual(publishes, 0)
        XCTAssertNil(model.result, "held value is not applied while open")
        cancel.cancel()
    }

    func testHeldPollAppliesLatestValueOncePerKeyOnClose() {
        let model = AppModel()
        var publishes = 0
        let cancel = model.objectWillChange.sink { publishes += 1 }
        model.menuDidOpen()
        model.applyPoll(ok, login: .notRegistered)
        model.applyPoll(.unauthorized, login: .enabled)
        model.menuDidClose()
        XCTAssertEqual(publishes, 2, "one publish for result, one for loginStatus")
        XCTAssertEqual(model.result, .unauthorized, "latest held value wins")
        XCTAssertEqual(model.loginStatus, .enabled)
        cancel.cancel()
    }

    func testDiagnosticsDoNotPublishWhileMenuIsOpen() {
        let model = AppModel()
        var publishes = 0
        let cancel = model.objectWillChange.sink { publishes += 1 }
        model.menuDidOpen()
        model.applyDiagnostics(checks: [], serve: .unknown("x"))
        XCTAssertEqual(publishes, 0)
        model.menuDidClose()
        XCTAssertEqual(publishes, 2, "checks and serve apply on close")
        XCTAssertEqual(model.serve, .unknown("x"))
        cancel.cancel()
    }

    func testPollPublishesImmediatelyWhenClosed() {
        let model = AppModel()
        var publishes = 0
        let cancel = model.objectWillChange.sink { publishes += 1 }
        model.applyPoll(.unauthorized, login: .notRegistered)
        XCTAssertEqual(publishes, 2)
        XCTAssertEqual(model.result, .unauthorized)
        cancel.cancel()
    }
}

final class MenuDeferralTests: XCTestCase {
    func testWritesWaitWhileOpenAndApplyLatestPerKeyOnClose() {
        let gate = MenuDeferral()
        var applied: [String] = []
        gate.setOpen(true)
        gate.publish("result") { applied.append("first") }
        gate.publish("result") { applied.append("second") }
        XCTAssertEqual(applied, [])
        gate.setOpen(false)
        XCTAssertEqual(applied, ["second"])
    }

    func testWriteRunsImmediatelyWhenClosed() {
        let gate = MenuDeferral()
        var applied = 0
        gate.publish("result") { applied += 1 }
        XCTAssertEqual(applied, 1)
    }

    func testHoldIsBoundedWhenCloseIsMissed() {
        let timers = FakeTimers()
        let gate = MenuDeferral(maxHold: 60, schedule: timers.schedule)
        var applied: [String] = []
        gate.setOpen(true)
        gate.publish("result") { applied.append("a") }
        XCTAssertEqual(timers.jobs.map(\.delay), [60], "fallback armed at the first held write")
        gate.publish("result") { applied.append("b") }
        XCTAssertEqual(applied, [], "still held before the bound")
        timers.jobs[0].fire()
        XCTAssertEqual(applied, ["b"], "flushes at the bound while open, latest value per key")
        XCTAssertTrue(gate.isOpen)
    }
}

/// Stands in for DispatchQueue.main.asyncAfter: records each job, and the test fires it.
private final class FakeTimers {
    private(set) var jobs: [(delay: TimeInterval, fire: () -> Void)] = []

    func schedule(_ delay: TimeInterval, _ work: @escaping () -> Void) {
        jobs.append((delay, work))
    }
}

final class MenuFallbackTests: XCTestCase {
    func testHeldWriteIsAppliedAfterMaxHoldWhenCloseNeverFires() {
        let timers = FakeTimers()
        let gate = MenuDeferral(maxHold: 60, schedule: timers.schedule)
        var applied: [String] = []
        gate.setOpen(true)
        gate.publish("result") { applied.append("a") }
        gate.publish("result") { applied.append("b") }
        XCTAssertEqual(timers.jobs.count, 1, "one fallback timer per batch, not per write")
        XCTAssertEqual(timers.jobs.first?.delay, 60)
        XCTAssertEqual(applied, [], "held until the bound")

        timers.jobs[0].fire()
        XCTAssertEqual(applied, ["b"], "latest value per key is applied at the bound, menu still open")
        XCTAssertTrue(gate.isOpen)

        gate.publish("result") { applied.append("c") }
        XCTAssertEqual(timers.jobs.count, 2, "a new batch arms a new bounded timer")
        XCTAssertEqual(applied, ["b"])
        timers.jobs[1].fire()
        XCTAssertEqual(applied, ["b", "c"])
    }

    func testStaleTimerDoesNotFlushLaterBatch() {
        let timers = FakeTimers()
        let gate = MenuDeferral(maxHold: 60, schedule: timers.schedule)
        var applied: [String] = []
        gate.setOpen(true)
        gate.publish("result") { applied.append("first") }
        gate.setOpen(false) // close flushes batch one and invalidates its timer
        XCTAssertEqual(applied, ["first"])

        gate.setOpen(true)
        gate.publish("result") { applied.append("second") }
        timers.jobs[0].fire() // batch one's timer fires during batch two
        XCTAssertEqual(applied, ["first"], "stale timer must not apply batch two early")
        timers.jobs[1].fire()
        XCTAssertEqual(applied, ["first", "second"])
    }

    func testFallbackAfterCloseAppliesNothingAgain() {
        let timers = FakeTimers()
        let gate = MenuDeferral(maxHold: 60, schedule: timers.schedule)
        var applied = 0
        gate.setOpen(true)
        gate.publish("result") { applied += 1 }
        gate.setOpen(false)
        timers.jobs[0].fire()
        XCTAssertEqual(applied, 1, "the fallback after close applies nothing again")
    }
}

final class ConfigEditorTests: XCTestCase {
    func testReplacesKeepingComments() {
        let text = "# top\nlisten = \"127.0.0.1:8787\"\n\n[summarizer]\n# note\nenabled = true   # billed\ntimeout_secs = 30\n"
        let out = ConfigEditor.setSummarizerEnabled(text, false)
        XCTAssertEqual(out, "# top\nlisten = \"127.0.0.1:8787\"\n\n[summarizer]\n# note\nenabled = false   # billed\ntimeout_secs = 30\n")
        XCTAssertFalse(ConfigEditor.summarizerEnabled(out))
        XCTAssertTrue(ConfigEditor.summarizerEnabled(ConfigEditor.setSummarizerEnabled(out, true)))
    }

    func testInsertsUnderHeader() {
        let out = ConfigEditor.setSummarizerEnabled("[summarizer]\ntimeout_secs = 5\n", false)
        XCTAssertEqual(out, "[summarizer]\nenabled = false\ntimeout_secs = 5\n")
    }

    func testAppendsSection() {
        XCTAssertEqual(ConfigEditor.setSummarizerEnabled("", false), "[summarizer]\nenabled = false\n")
        XCTAssertEqual(ConfigEditor.setSummarizerEnabled("listen = \"x\"", true), "listen = \"x\"\n\n[summarizer]\nenabled = true\n")
        XCTAssertTrue(ConfigEditor.summarizerEnabled(""), "daemon default is on")
    }

    func testOtherSectionsEnabledUntouched() {
        let text = "[filter]\nstates = []\n[other]\nenabled = true\n[summarizer]\ncommand = [\"x\"]\n[hook]\ninclude_subagents = false\n"
        let out = ConfigEditor.setSummarizerEnabled(text, false)
        XCTAssertEqual(out, "[filter]\nstates = []\n[other]\nenabled = true\n[summarizer]\nenabled = false\ncommand = [\"x\"]\n[hook]\ninclude_subagents = false\n")
        XCTAssertTrue(ConfigEditor.summarizerEnabled(text))
        XCTAssertFalse(ConfigEditor.summarizerEnabled("[summarizer] # c\nenabled=false"))
    }

    func testWriteAtomicallyIsPrivate() throws {
        let dir = FileManager.default.temporaryDirectory.appendingPathComponent("np-\(UUID().uuidString)/cfg")
        let url = dir.appendingPathComponent("config.toml")
        try ConfigEditor.writeAtomically("a = 1\n", to: url)
        try ConfigEditor.writeAtomically("a = 2\n", to: url)
        XCTAssertEqual(try String(contentsOf: url, encoding: .utf8), "a = 2\n")
        let attrs = try FileManager.default.attributesOfItem(atPath: url.path)
        XCTAssertEqual((attrs[.posixPermissions] as? NSNumber)?.intValue, 0o600)
        let dattrs = try FileManager.default.attributesOfItem(atPath: dir.path)
        XCTAssertEqual((dattrs[.posixPermissions] as? NSNumber)?.intValue, 0o700)
        let leftovers = try FileManager.default.contentsOfDirectory(atPath: dir.path)
        XCTAssertEqual(leftovers, ["config.toml"])
    }
}

final class ServeStatusTests: XCTestCase {
    func testMappedOn443() {
        let json = #"{"TCP":{"443":{"HTTPS":true}},"Web":{"laptop.example.ts.net:443":{"Handlers":{"/":{"Proxy":"http://127.0.0.1:8787"}}}}}"#
        XCTAssertEqual(ServeStatus.parse(Data(json.utf8)), .mapped("https://laptop.example.ts.net/"))
    }

    func testMappedOn8443() {
        let json = #"{"TCP":{"8443":{"HTTPS":true}},"Web":{"laptop.example.ts.net:8443":{"Handlers":{"/":{"Proxy":"localhost:8787"}}}}}"#
        XCTAssertEqual(ServeStatus.parse(Data(json.utf8)), .mapped("https://laptop.example.ts.net:8443/"))
    }

    func testOtherConfig() {
        let json = #"{"TCP":{"443":{"HTTPS":true}},"Web":{"laptop.example.ts.net:443":{"Handlers":{"/":{"Proxy":"http://127.0.0.1:3000"}}}}}"#
        XCTAssertEqual(ServeStatus.parse(Data(json.utf8)), .otherConfig)
        XCTAssertEqual(ServeStatus.parse(Data(#"{"TCP":{"22":{"TCPForward":"127.0.0.1:22"}}}"#.utf8)), .otherConfig)
    }

    func testEmpty() {
        XCTAssertEqual(ServeStatus.parse(Data()), .empty)
        XCTAssertEqual(ServeStatus.parse(Data("{}\n".utf8)), .empty)
        XCTAssertEqual(ServeStatus.parse(Data(#"{"TCP":{},"Web":{}}"#.utf8)), .empty)
        if case .unknown = ServeStatus.parse(Data("No serve config".utf8)) {} else { XCTFail("text") }
    }
}

final class IconStateTests: XCTestCase {
    func status(viewers: Int, paused: Bool) -> StatusResult {
        guard case .ok(var s) = DaemonClient.classify(statusCode: 200, data: Data(statusFixture.utf8)) else { fatalError() }
        s.viewers = viewers
        s.paused = paused
        return .ok(s)
    }

    func testMatrix() {
        XCTAssertEqual(IconState.derive(status(viewers: 0, paused: false)), .running)
        XCTAssertEqual(IconState.derive(status(viewers: 2, paused: false)), .connected)
        XCTAssertEqual(IconState.derive(status(viewers: 2, paused: true)), .paused)
        XCTAssertEqual(IconState.derive(status(viewers: 0, paused: false), supervisorFailed: true), .error)
        XCTAssertEqual(IconState.derive(.unreachable("x")), .error)
        XCTAssertEqual(IconState.derive(.unauthorized), .error)
        XCTAssertEqual(IconState.derive(.external("old")), .error)
        XCTAssertEqual(IconState.derive(nil), .error)
    }

    func testAgentStateNames() {
        XCTAssertEqual(AgentState.allCases.map(\.rawValue), ["allow", "your_turn", "all_set", "working"])
    }
}

/// Status with the classifier object and pending_your_turn, as the FEAT-002 daemon sends it.
let classifierStatusFixture = """
{"version":"0.2.0","pid":7,"started_at_ms":1,"uptime_secs":5,"viewers":0,"last_event_id":3,"paused":false,
 "filter":{"states":["all_set","your_turn"],"source":"config"},
 "summarizer":{"enabled":true,"healthy":true,"last_ok_ms":0,"last_error_ms":0},
 "classifier":{"enabled":true,"provider":"openrouter","model":"typesafe/jev-1.13","source":"control",
   "threshold":0.5,"key_service":"nearpush-openrouter-api-key","key_present":true,
   "healthy":true,"last_ok_ms":9,"last_error_ms":0},
 "pending_count":1,
 "pending_your_turn":[{"session":"s1","ts":42}],
 "recent":[]}
"""

final class QRImageTests: XCTestCase {
    func testNonEmptyURLMakesImage() throws {
        let img = try XCTUnwrap(QRImage.cgImage(for: "https://laptop.example.ts.net/"))
        XCTAssertGreaterThanOrEqual(img.width, 240)
        XCTAssertEqual(img.width, img.height)
    }

    func testEmptyURLMakesNoImage() {
        XCTAssertNil(QRImage.cgImage(for: ""))
    }
}

final class ClassifierTests: XCTestCase {
    func testProviderOrderAndRawValues() {
        XCTAssertEqual(ClassifierProvider.allCases, [.kiro, .openrouter, .typesafe])
        XCTAssertEqual(ClassifierProvider.allCases.map(\.rawValue), ["kiro", "openrouter", "typesafe"])
    }

    func testTitlesModelsAndServicesMatchPlan() {
        XCTAssertEqual(ClassifierProvider.kiro.title, "Kiro · Haiku 5.5")
        XCTAssertEqual(ClassifierProvider.openrouter.title, "OpenRouter · Jev 1.13")
        XCTAssertEqual(ClassifierProvider.typesafe.title, "TypeSafe · Jev 1.13.0")
        XCTAssertEqual(ClassifierProvider.kiro.model, "claude-haiku-5.5")
        XCTAssertEqual(ClassifierProvider.openrouter.model, "typesafe/jev-1.13")
        XCTAssertEqual(ClassifierProvider.typesafe.model, "jev-1.13.0")
        XCTAssertEqual(ClassifierProvider.kiro.keychainService, "nearpush-kiro-api-key")
        XCTAssertEqual(ClassifierProvider.openrouter.keychainService, "nearpush-openrouter-api-key")
        XCTAssertEqual(ClassifierProvider.typesafe.keychainService, "nearpush-typesafe-api-key")
        for p in ClassifierProvider.allCases {
            XCTAssertFalse(p.model.contains("latest"), "no alias model ids")
        }
    }

    func testKeychainHintHasServiceAndNoKeyValue() {
        for p in ClassifierProvider.allCases {
            let hint = ClassifierProvider.keychainHint(p)
            XCTAssertTrue(hint.contains("-s \(p.keychainService)"))
            XCTAssertTrue(hint.contains(" -w"))
            XCTAssertTrue(hint.hasPrefix("security add-generic-password"))
            XCTAssertFalse(hint.contains("sk-"), "no key-looking text")
        }
    }

    func testStatusWithClassifierDecodes() throws {
        let s = try DaemonClient.decoder.decode(DaemonStatus.self, from: Data(classifierStatusFixture.utf8))
        let cl = try XCTUnwrap(s.classifier)
        XCTAssertEqual(cl.provider, "openrouter")
        XCTAssertEqual(cl.model, "typesafe/jev-1.13")
        XCTAssertEqual(cl.source, "control")
        XCTAssertEqual(cl.threshold, 0.5)
        XCTAssertEqual(cl.keyService, "nearpush-openrouter-api-key")
        XCTAssertEqual(cl.keyPresent, true)
        XCTAssertTrue(cl.healthy)
        XCTAssertEqual(cl.lastOkMs, 9)
        XCTAssertEqual(s.pendingYourTurn, [PendingYourTurn(session: "s1", ts: 42)])
        XCTAssertEqual(s.pendingCount, 1)
    }

    func testStatusWithoutClassifierStillDecodes() throws {
        guard case .ok(let s) = DaemonClient.classify(statusCode: 200, data: Data(statusFixture.utf8)) else {
            return XCTFail("older daemon fixture must decode")
        }
        XCTAssertNil(s.classifier)
        XCTAssertNil(s.pendingYourTurn)
    }

    func testClassifierKeyPresentNullDecodes() throws {
        let json = classifierStatusFixture.replacingOccurrences(of: "\"key_present\":true", with: "\"key_present\":null")
        let s = try DaemonClient.decoder.decode(DaemonStatus.self, from: Data(json.utf8))
        XCTAssertNil(s.classifier?.keyPresent)
    }

    func testControlResponseWithClassifierKeyDecodes() throws {
        let c = try DaemonClient.decoder.decode(ControlResponse.self,
            from: Data(#"{"paused":false,"filter":{"states":[],"source":"config"},"classifier":{"provider":"typesafe"}}"#.utf8))
        XCTAssertFalse(c.paused)
    }
}

/// A status that carries the relay object, as the daemon's `RelayView` sends it.
let relayStatusFixture = """
{"version":"0.2.0","pid":8,"started_at_ms":1,"uptime_secs":5,"viewers":0,"last_event_id":77,"paused":false,
 "filter":{"states":["all_set"],"source":"config"},
 "summarizer":{"enabled":true,"healthy":true,"last_ok_ms":0,"last_error_ms":0},
 "relay":{"enabled":true,"connected":false,"last_id":77,"last_ok_ms":0,"last_error_class":"http"},
 "recent":[]}
"""

final class SettingsModelTests: XCTestCase {
    private var suite = ""
    private var defaults: UserDefaults!

    override func setUp() {
        super.setUp()
        suite = "np-test-\(UUID().uuidString)"
        defaults = UserDefaults(suiteName: suite)
    }

    override func tearDown() {
        defaults.removePersistentDomain(forName: suite)
        defaults = nil
        super.tearDown()
    }

    private func classifier(_ json: String) throws -> ClassifierStatus {
        try DaemonClient.decoder.decode(ClassifierStatus.self, from: Data(json.utf8))
    }

    func testMenuIconDefaultsOnWhenKeyAbsent() {
        XCTAssertNil(defaults.object(forKey: MenuIconSetting.key))
        XCTAssertTrue(MenuIconSetting.isOn(defaults))
    }

    func testMenuIconSetRoundTrips() {
        MenuIconSetting.set(false, defaults)
        XCTAssertFalse(MenuIconSetting.isOn(defaults))
        MenuIconSetting.set(true, defaults)
        XCTAssertTrue(MenuIconSetting.isOn(defaults))
    }

    func testMenuIconKeyName() {
        XCTAssertEqual(MenuIconSetting.key, "showMenuBarIcon")
    }

    func testShowOnPhoneRowsExcludeGenericEvents() {
        XCTAssertEqual(ShowOnPhone.rows, [.allSet, .yourTurn, .allow, .working])
    }

    func testShowOnPhoneIsShownFollowsFilter() {
        let filter = ["all_set", "your_turn", "allow"]
        XCTAssertTrue(ShowOnPhone.isShown(.allSet, filter: filter))
        XCTAssertTrue(ShowOnPhone.isShown(.yourTurn, filter: filter))
        XCTAssertTrue(ShowOnPhone.isShown(.allow, filter: filter))
        XCTAssertFalse(ShowOnPhone.isShown(.working, filter: filter))
    }

    func testShowOnPhoneTurnOnAppendsWorking() {
        let next = ShowOnPhone.toggled(["all_set", "your_turn", "allow"], .working, on: true)
        XCTAssertEqual(next, ["all_set", "your_turn", "allow", "working"])
    }

    func testShowOnPhoneTurnOffRemovesAllow() {
        let next = ShowOnPhone.toggled(["all_set", "your_turn", "allow"], .allow, on: false)
        XCTAssertEqual(next, ["all_set", "your_turn"])
    }

    func testShowOnPhoneKeepsUnknownNames() {
        let next = ShowOnPhone.toggled(["future", "all_set"], .allow, on: true)
        XCTAssertEqual(next, ["future", "all_set", "allow"])
        let off = ShowOnPhone.toggled(["future"], .allSet, on: false)
        XCTAssertEqual(off, ["future"])
    }

    func testShowOnPhoneRepeatedOnDoesNotDuplicate() {
        let once = ShowOnPhone.toggled(["all_set"], .yourTurn, on: true)
        let twice = ShowOnPhone.toggled(once, .yourTurn, on: true)
        XCTAssertEqual(twice, ["all_set", "your_turn"])
    }

    func testSummarizerToggleFollowsStatusThenConfig() {
        let on = SummarizerStatus(enabled: true, healthy: true, lastOkMs: 0, lastErrorMs: 0)
        let off = SummarizerStatus(enabled: false, healthy: true, lastOkMs: 0, lastErrorMs: 0)
        XCTAssertTrue(SummarizerToggle.isOn(on, inConfig: false), "/status wins over config.toml")
        XCTAssertFalse(SummarizerToggle.isOn(off, inConfig: true), "/status wins over config.toml")
        XCTAssertTrue(SummarizerToggle.isOn(nil, inConfig: true), "no status yet: config.toml")
        XCTAssertFalse(SummarizerToggle.isOn(nil, inConfig: false), "no status yet: config.toml")
    }

    func testSummarizerToggleReadsStatusFixture() throws {
        guard case .ok(var s) = DaemonClient.classify(statusCode: 200, data: Data(statusFixture.utf8)) else {
            return XCTFail("fixture must decode")
        }
        XCTAssertTrue(SummarizerToggle.isOn(s.summarizer, inConfig: false))
        s.summarizer.enabled = false
        XCTAssertFalse(SummarizerToggle.isOn(s.summarizer, inConfig: true))
    }

    func testProviderPickerSelectsEachRawValue() throws {
        for (raw, expected) in [("kiro", ClassifierProvider.kiro),
                                ("openrouter", .openrouter),
                                ("typesafe", .typesafe)] {
            let cl = try classifier(#"{"enabled":true,"provider":"\#(raw)","model":"m","source":"config","threshold":0.5,"key_service":"k","healthy":true,"last_ok_ms":0,"last_error_ms":0}"#)
            XCTAssertEqual(ProviderPicker.selected(cl), expected)
        }
    }

    func testProviderPickerUnknownOrMissingIsNil() throws {
        let bogus = try classifier(#"{"enabled":true,"provider":"bogus","model":"m","source":"config","threshold":0.5,"key_service":"k","healthy":true,"last_ok_ms":0,"last_error_ms":0}"#)
        XCTAssertNil(ProviderPicker.selected(bogus))
        XCTAssertNil(ProviderPicker.selected(nil))
    }

    func testProviderPickerEnabledFollowsStatus() throws {
        let off = try classifier(#"{"enabled":false,"provider":"kiro","model":"m","source":"config","threshold":0.5,"key_service":"k","healthy":true,"last_ok_ms":0,"last_error_ms":0}"#)
        XCTAssertFalse(ProviderPicker.isEnabled(off))
        XCTAssertFalse(ProviderPicker.isEnabled(nil))
        XCTAssertTrue(ProviderPicker.isEnabled(try classifier(#"{"enabled":true,"provider":"kiro","model":"m","source":"config","threshold":0.5,"key_service":"k","healthy":true,"last_ok_ms":0,"last_error_ms":0}"#)))
    }

    func testRelayStatusDecodes() throws {
        let s = try DaemonClient.decoder.decode(DaemonStatus.self, from: Data(relayStatusFixture.utf8))
        let relay = try XCTUnwrap(s.relay)
        XCTAssertTrue(relay.enabled)
        XCTAssertFalse(relay.connected)
        XCTAssertEqual(relay.lastId, 77)
        XCTAssertEqual(relay.lastErrorClass, "http")
    }

    func testStatusWithoutRelayDecodesNil() throws {
        guard case .ok(let s) = DaemonClient.classify(statusCode: 200, data: Data(statusFixture.utf8)) else {
            return XCTFail("older daemon fixture must decode")
        }
        XCTAssertNil(s.relay)
    }
}

/// The menu bar dropdown is a quick-glance menu: status, Settings…, pause, quit.
final class QuickMenuTests: XCTestCase {
    private func status() throws -> DaemonStatus {
        guard case .ok(let s) = DaemonClient.classify(statusCode: 200, data: Data(statusFixture.utf8)) else {
            throw XCTSkip("fixture must decode")
        }
        return s
    }

    private func event(_ id: UInt64, state: String?, project: String?, shown: Bool = true) -> RecentEvent {
        RecentEvent(id: id, ts: 1, title: "t", state: state, level: nil, session: nil, sessionLabel: nil,
                    project: project, detail: nil, shown: shown)
    }

    func testMenuExposesExactlyStatusSettingsPauseQuit() {
        XCTAssertEqual(QuickMenu.items, [.status, .settings, .pause, .quit])
        XCTAssertEqual(QuickMenu.items.map(\.title), [nil, "Settings…", "Pause pushes", "Quit Nearpush"])
    }

    func testSettingsAndQuitHaveCommandShortcuts() {
        XCTAssertEqual(QuickMenuItem.settings.shortcut, ",")
        XCTAssertEqual(QuickMenuItem.quit.shortcut, "q")
        XCTAssertNil(QuickMenuItem.status.shortcut)
        XCTAssertNil(QuickMenuItem.pause.shortcut)
    }

    func testStatusLineShowsStateAndProjectOfLatestCard() throws {
        var s = try status()
        s.recent = [event(3, state: "your_turn", project: "nearpush / main")]
        XCTAssertEqual(QuickMenu.statusLine(s), "● your turn · nearpush / main")
    }

    func testStatusLineWithoutProjectIsStateOnly() throws {
        XCTAssertEqual(QuickMenu.statusLine(try status()), "● all set")
    }

    func testStatusLineSkipsHiddenEvents() throws {
        var s = try status()
        s.recent = [event(5, state: "working", project: "a / b", shown: false),
                    event(4, state: "all_set", project: "c / d")]
        XCTAssertEqual(QuickMenu.statusLine(s), "● all set · c / d")
    }

    func testStatusLinePaused() throws {
        var s = try status()
        s.paused = true
        XCTAssertEqual(QuickMenu.statusLine(s), "Paused")
    }

    func testStatusLineWithNoEvents() throws {
        var s = try status()
        s.recent = []
        XCTAssertEqual(QuickMenu.statusLine(s), "● running")
    }

    func testStatusLineIsTruncatedToOneShortLine() throws {
        var s = try status()
        s.recent = [event(1, state: "your_turn", project: String(repeating: "x", count: 200))]
        let line = QuickMenu.statusLine(s)
        XCTAssertEqual(line.count, QuickMenu.maxStatusChars)
        XCTAssertTrue(line.hasSuffix("…"))
        XCTAssertTrue(line.hasPrefix("● your turn · x"))
    }

    @MainActor
    func testAppModelMenuStatusLine() {
        let model = AppModel()
        model.applyPoll(DaemonClient.classify(statusCode: 200, data: Data(statusFixture.utf8)), login: .notRegistered)
        XCTAssertEqual(model.menuStatusLine, "● all set")
        model.applyPoll(.unauthorized, login: .notRegistered)
        XCTAssertEqual(model.menuStatusLine, model.statusLine, "a daemon problem shows the window's warning")
        XCTAssertTrue(model.menuStatusLine.hasSuffix(" Daemon rejects the token"))
    }

    @MainActor
    func testAppModelMenuStatusLineShowsActionError() {
        let model = AppModel()
        model.applyPoll(DaemonClient.classify(statusCode: 200, data: Data(statusFixture.utf8)), login: .notRegistered)
        model.actionError = "Pause failed: " + String(repeating: "x", count: 200)
        XCTAssertEqual(model.menuStatusLine.count, QuickMenu.maxStatusChars)
        XCTAssertTrue(model.menuStatusLine.hasPrefix("⚠ Pause failed: x"))
        XCTAssertTrue(model.menuStatusLine.hasSuffix("…"))
        model.actionError = nil
        XCTAssertEqual(model.menuStatusLine, "● all set")
    }
}
