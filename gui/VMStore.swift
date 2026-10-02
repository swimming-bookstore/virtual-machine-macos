import AppKit
import Foundation
import SwiftUI

struct VMInfo: Identifiable, Hashable {
    var id: String { dir }
    var dir: String
    var running: Bool
    var pid: UInt32?
    var hasDisk: Bool
    var hasLog: Bool
    var logTail: String

    var name: String {
        URL(fileURLWithPath: dir).lastPathComponent
    }
}

@MainActor
final class VMStore: ObservableObject {
    @Published var vms: [VMInfo] = []
    @Published var selected: String?
    @Published var showNew = false
    @Published var busy: String?
    @Published var alert: String?

    let vmHome = FileManager.default.homeDirectoryForCurrentUser.appendingPathComponent("VMs")

    private var timer: Timer?
    private var known: [String] = []

    var selectedVM: VMInfo? {
        vms.first { $0.id == selected }
    }

    func start() {
        known = UserDefaults.standard.stringArray(forKey: "vm.dirs") ?? []
        scanHome()
        refresh()
        timer?.invalidate()
        timer = Timer.scheduledTimer(withTimeInterval: 2, repeats: true) { [weak self] _ in
            Task { @MainActor in
                self?.refresh()
            }
        }
    }

    func refresh() {
        let running = Self.parseList(run(tool: "vmagent", args: ["list", "--json"]).stdout)
        var dirs = Set(known)
        for vm in running {
            dirs.insert(vm.dir)
        }
        scanHome(&dirs)

        let fm = FileManager.default
        var next: [VMInfo] = []
        for dir in dirs.sorted() {
            let url = URL(fileURLWithPath: dir)
            let match = running.first { $0.dir == dir || Self.samePath($0.dir, dir) }
            let exists = fm.fileExists(atPath: dir)
            if !exists && match == nil {
                continue
            }
            let logURL = url.appendingPathComponent("vm.log")
            next.append(
                VMInfo(
                    dir: dir,
                    running: match != nil,
                    pid: match.flatMap { $0.pid == 0 ? nil : $0.pid },
                    hasDisk: fm.fileExists(atPath: url.appendingPathComponent("disk.img").path),
                    hasLog: fm.fileExists(atPath: logURL.path),
                    logTail: Self.tail(logURL)
                )
            )
        }
        vms = next
        known = next.map(\.dir)
        persist()
        if selected == nil || !vms.contains(where: { $0.id == selected }) {
            selected = vms.first?.id
        }
    }

    func create(name: String, desktop: Bool, cpus: Int, memMB: Int, diskGB: Int) {
        let dir = vmHome.appendingPathComponent(name).path
        addKnown(dir)
        startVM(
            VMInfo(dir: dir, running: false, pid: nil, hasDisk: false, hasLog: false, logTail: ""),
            desktop: desktop,
            cpus: cpus,
            memMB: memMB,
            diskGB: diskGB
        )
    }

    func startVM(_ vm: VMInfo, desktop: Bool, cpus: Int = 2, memMB: Int? = nil, diskGB: Int = 8) {
        guard let image = imagePath() else {
            alert = "Debian image not found. Use VM → Fetch Debian Image."
            return
        }
        guard let userData = cloudInit(desktop ? "user-data-gui" : "user-data") else {
            alert = "cloud-init user-data not found"
            return
        }
        guard let meta = cloudInit("meta-data") else {
            alert = "cloud-init meta-data not found"
            return
        }
        let mem = memMB ?? (desktop ? 4096 : 2048)
        let dir = vm.dir
        busy = desktop ? "Starting desktop VM…" : "Starting VM…"
        DispatchQueue.global(qos: .userInitiated).async {
            let result = self.run(
                tool: "vmagent",
                args: [
                    "--image", image,
                    "--user-data", userData,
                    "--meta-data", meta,
                    "--dir", dir,
                    "--cpus", String(cpus),
                    "--mem-mb", String(mem),
                    "--disk-gb", String(diskGB),
                ]
            )
            DispatchQueue.main.async {
                self.busy = nil
                if result.status != 0 {
                    self.alert = result.combined
                } else {
                    self.addKnown(dir)
                    self.selected = dir
                    self.refresh()
                }
            }
        }
    }

    func attach(_ vm: VMInfo) {
        let result = run(tool: "vmagent", args: ["attach", "--dir", vm.dir])
        if result.status != 0 {
            alert = result.combined
        }
    }

    func stop(_ vm: VMInfo) {
        let result = run(tool: "vmagent", args: ["stop", "--dir", vm.dir])
        if result.status != 0 {
            alert = result.combined
        }
        refresh()
    }

    func ssh(_ vm: VMInfo) {
        let agent = toolPath("vmagent")
        let script = "clear; \(quote(agent)) ssh --dir \(quote(vm.dir)) debian@vm; echo; read -n 1 -s -p 'press any key'"
        let proc = Process()
        proc.executableURL = URL(fileURLWithPath: "/usr/bin/osascript")
        proc.arguments = ["-e", "tell application \"Terminal\" to do script \(quote(script))"]
        try? proc.run()
    }

    func openFolder(_ vm: VMInfo) {
        NSWorkspace.shared.open(URL(fileURLWithPath: vm.dir))
    }

    func openLog(_ vm: VMInfo) {
        NSWorkspace.shared.open(URL(fileURLWithPath: vm.dir).appendingPathComponent("vm.log"))
    }

    func fetchImage() {
        let script = fetchScript()
        guard FileManager.default.fileExists(atPath: script) else {
            alert = "fetch-debian.sh not found"
            return
        }
        busy = "Fetching Debian image…"
        let outdir = vmHome.path
        DispatchQueue.global(qos: .userInitiated).async {
            let result = self.run(path: script, args: [outdir])
            DispatchQueue.main.async {
                self.busy = nil
                if result.status != 0 {
                    self.alert = result.combined.isEmpty ? "fetch failed" : result.combined
                } else {
                    self.alert = "Saved \(self.imagePath() ?? "\(outdir)/debian.raw")"
                }
            }
        }
    }

    private func addKnown(_ dir: String) {
        if !known.contains(dir) {
            known.append(dir)
            persist()
        }
    }

    private func persist() {
        UserDefaults.standard.set(known, forKey: "vm.dirs")
    }

    private func scanHome() {
        var dirs = Set(known)
        scanHome(&dirs)
        known = dirs.sorted()
    }

    private func scanHome(_ dirs: inout Set<String>) {
        let fm = FileManager.default
        try? fm.createDirectory(at: vmHome, withIntermediateDirectories: true)
        if let items = try? fm.contentsOfDirectory(at: vmHome, includingPropertiesForKeys: [.isDirectoryKey]) {
            for url in items where url.hasDirectoryPath {
                if fm.fileExists(atPath: url.appendingPathComponent("disk.img").path) {
                    dirs.insert(url.path)
                }
            }
        }
    }

    private func imagePath() -> String? {
        let candidates = [
            vmHome.appendingPathComponent("debian.raw").path,
            repoFile("images/debian.raw"),
        ]
        return candidates.first { FileManager.default.fileExists(atPath: $0) }
    }

    private func fetchScript() -> String {
        if let bundled = Bundle.main.resourceURL?
            .appendingPathComponent("scripts/fetch-debian.sh").path,
           FileManager.default.isExecutableFile(atPath: bundled)
        {
            return bundled
        }
        return repoFile("scripts/fetch-debian.sh")
    }

    private func cloudInit(_ name: String) -> String? {
        let bundled = Bundle.main.resourceURL?
            .appendingPathComponent("cloud-init")
            .appendingPathComponent(name)
            .path
        let repo = repoFile("cloud-init/\(name)")
        return [bundled, repo].compactMap { $0 }.first { FileManager.default.fileExists(atPath: $0) }
    }

    private func repoFile(_ rel: String) -> String {
        root().appendingPathComponent(rel).path
    }

    private func quote(_ s: String) -> String {
        "'" + s.replacingOccurrences(of: "'", with: "'\\''") + "'"
    }

    nonisolated func run(tool: String, args: [String]) -> (status: Int32, stdout: String, combined: String) {
        run(path: toolPath(tool), args: args)
    }

    nonisolated func run(path: String, args: [String]) -> (status: Int32, stdout: String, combined: String) {
        let proc = Process()
        proc.executableURL = URL(fileURLWithPath: path)
        proc.arguments = args
        var env = ProcessInfo.processInfo.environment
        env["VMCORE"] = toolPath("vmcore")
        env["SPLIT_IMAGE"] = toolPath("split-image.py")
        proc.environment = env
        let out = Pipe()
        let err = Pipe()
        proc.standardOutput = out
        proc.standardError = err
        proc.standardInput = FileHandle.nullDevice
        do {
            try proc.run()
        } catch {
            return (1, "", error.localizedDescription)
        }
        proc.waitUntilExit()
        let stdout = String(data: out.fileHandleForReading.readDataToEndOfFile(), encoding: .utf8) ?? ""
        let stderr = String(data: err.fileHandleForReading.readDataToEndOfFile(), encoding: .utf8) ?? ""
        let combined = (stderr + stdout).trimmingCharacters(in: .whitespacesAndNewlines)
        return (proc.terminationStatus, stdout, combined)
    }

    nonisolated func toolPath(_ name: String) -> String {
        if let env = ProcessInfo.processInfo.environment[name.uppercased()], !env.isEmpty {
            return env
        }
        if let bundled = Bundle.main.resourceURL?
            .appendingPathComponent("bin")
            .appendingPathComponent(name).path,
           FileManager.default.fileExists(atPath: bundled)
        {
            return bundled
        }
        let exe = URL(fileURLWithPath: CommandLine.arguments[0]).deletingLastPathComponent()
        let nextTo = exe.appendingPathComponent(name).path
        if FileManager.default.fileExists(atPath: nextTo) {
            return nextTo
        }
        return root().appendingPathComponent("bin").appendingPathComponent(name).path
    }

    nonisolated func root() -> URL {
        if let env = ProcessInfo.processInfo.environment["VMAGENT_ROOT"], !env.isEmpty {
            return URL(fileURLWithPath: env)
        }
        var dir = URL(fileURLWithPath: CommandLine.arguments[0]).deletingLastPathComponent()
        for _ in 0..<8 {
            if FileManager.default.fileExists(atPath: dir.appendingPathComponent("Cargo.toml").path) {
                return dir
            }
            dir.deleteLastPathComponent()
        }
        return URL(fileURLWithPath: FileManager.default.currentDirectoryPath)
    }

    private static func parseList(_ text: String) -> [(dir: String, pid: UInt32)] {
        let trimmed = text.trimmingCharacters(in: .whitespacesAndNewlines)
        guard let data = trimmed.data(using: .utf8),
              let arr = try? JSONSerialization.jsonObject(with: data) as? [[String: Any]]
        else {
            return []
        }
        return arr.compactMap { row in
            guard let dir = row["dir"] as? String else { return nil }
            let pid = (row["pid"] as? NSNumber)?.uint32Value ?? 0
            return (dir, pid)
        }
    }

    private static func samePath(_ a: String, _ b: String) -> Bool {
        URL(fileURLWithPath: a).standardizedFileURL.path == URL(fileURLWithPath: b).standardizedFileURL.path
    }

    private static func tail(_ url: URL) -> String {
        guard let data = try? Data(contentsOf: url),
              let text = String(data: data, encoding: .utf8)
        else { return "" }
        let lines = text.split(whereSeparator: \.isNewline)
        return lines.suffix(40).joined(separator: "\n")
    }
}
