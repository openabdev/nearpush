import XCTest
@testable import NearpushCore

/// Same shape as nearpushd's `GET /status` (crates/nearpushd/src/server.rs `StatusView`).
let statusFixture = """
{"version":"0.1.0","ui_version":"0.1.0-abc","pid":4242,"exe":"/Users/p/Applications/Nearpush.app/Contents/MacOS/nearpushd",
 "started_at_ms":1700000000000,"uptime_secs":61,"viewers":1,"last_event_id":12,"paused":false,
 "filter":{"states":["all_set","your_turn","allow"],"source":"config"},
 "summarizer":{"enabled":true,"healthy":true,"last_ok_ms":5,"last_error_ms":0},
 "public_url":"https://lap.tail.ts.net/",
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
        XCTAssertEqual(s.publicUrl, "https://lap.tail.ts.net/")
        XCTAssertEqual(s.recent.count, 3)
        XCTAssertEqual(s.recent[0].menuLine, "#12 all set — took 3m")
        XCTAssertEqual(s.recent[0].copyText, "all set. — took 3m")
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
        let json = #"{"TCP":{"443":{"HTTPS":true}},"Web":{"lap.tail.ts.net:443":{"Handlers":{"/":{"Proxy":"http://127.0.0.1:8787"}}}}}"#
        XCTAssertEqual(ServeStatus.parse(Data(json.utf8)), .mapped("https://lap.tail.ts.net/"))
    }

    func testMappedOn8443() {
        let json = #"{"TCP":{"8443":{"HTTPS":true}},"Web":{"lap.tail.ts.net:8443":{"Handlers":{"/":{"Proxy":"localhost:8787"}}}}}"#
        XCTAssertEqual(ServeStatus.parse(Data(json.utf8)), .mapped("https://lap.tail.ts.net:8443/"))
    }

    func testOtherConfig() {
        let json = #"{"TCP":{"443":{"HTTPS":true}},"Web":{"lap.tail.ts.net:443":{"Handlers":{"/":{"Proxy":"http://127.0.0.1:3000"}}}}}"#
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
