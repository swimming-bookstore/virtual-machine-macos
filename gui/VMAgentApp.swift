import AppKit
import SwiftUI

@main
struct VMAgentApp: App {
    @StateObject private var store = VMStore()

    var body: some Scene {
        WindowGroup("VMAgent") {
            ContentView()
                .environmentObject(store)
                .frame(minWidth: 720, minHeight: 480)
        }
        .defaultSize(width: 840, height: 560)
        .commands {
            CommandGroup(replacing: .newItem) {
                Button("New VM…") { store.showNew = true }
                    .keyboardShortcut("n")
            }
            CommandMenu("VM") {
                Button("Refresh") { store.refresh() }
                    .keyboardShortcut("r")
                Divider()
                Button("Fetch Debian Image") { store.fetchImage() }
            }
        }
    }
}

struct ContentView: View {
    @EnvironmentObject var store: VMStore

    var body: some View {
        NavigationSplitView {
            List(selection: $store.selected) {
                if store.vms.isEmpty {
                    Text("No VMs yet")
                        .foregroundStyle(.secondary)
                }
                ForEach(store.vms) { vm in
                    VMRow(vm: vm)
                        .tag(vm.id)
                }
            }
            .navigationTitle("VMs")
            .toolbar {
                ToolbarItem(placement: .primaryAction) {
                    Button {
                        store.showNew = true
                    } label: {
                        Label("New VM", systemImage: "plus")
                    }
                }
            }
        } detail: {
            if let vm = store.selectedVM {
                VMDetail(vm: vm)
            } else {
                VStack(spacing: 8) {
                    Image(systemName: "desktopcomputer")
                        .font(.system(size: 48))
                        .foregroundStyle(.secondary)
                    Text("Select a VM")
                        .font(.title2)
                    Text("Start a Debian guest from the plus button.")
                        .foregroundStyle(.secondary)
                }
                .frame(maxWidth: .infinity, maxHeight: .infinity)
            }
        }
        .onAppear { store.start() }
        .sheet(isPresented: $store.showNew) {
            NewVMSheet()
                .environmentObject(store)
        }
        .alert("VMAgent", isPresented: Binding(
            get: { store.alert != nil },
            set: { if !$0 { store.alert = nil } }
        )) {
            Button("OK", role: .cancel) { store.alert = nil }
        } message: {
            Text(store.alert ?? "")
        }
        .overlay {
            if let busy = store.busy {
                ZStack {
                    Color.black.opacity(0.12).ignoresSafeArea()
                    VStack(spacing: 12) {
                        ProgressView()
                        Text(busy)
                            .font(.headline)
                    }
                    .padding(24)
                    .background(.regularMaterial, in: RoundedRectangle(cornerRadius: 12))
                }
            }
        }
    }
}

struct VMRow: View {
    let vm: VMInfo

    var body: some View {
        HStack {
            Circle()
                .fill(vm.running ? Color.green : Color.secondary.opacity(0.4))
                .frame(width: 8, height: 8)
            VStack(alignment: .leading, spacing: 2) {
                Text(vm.name)
                    .font(.headline)
                Text(vm.dir)
                    .font(.caption)
                    .foregroundStyle(.secondary)
                    .lineLimit(1)
                    .truncationMode(.middle)
            }
        }
        .padding(.vertical, 2)
    }
}

struct VMDetail: View {
    @EnvironmentObject var store: VMStore
    let vm: VMInfo

    var body: some View {
        VStack(alignment: .leading, spacing: 16) {
            HStack {
                VStack(alignment: .leading, spacing: 4) {
                    Text(vm.name)
                        .font(.largeTitle.bold())
                    Text(vm.dir)
                        .font(.callout.monospaced())
                        .foregroundStyle(.secondary)
                        .textSelection(.enabled)
                }
                Spacer()
                StatusBadge(running: vm.running)
            }

            LabeledContent("PID") {
                Text(vm.pid.map(String.init) ?? "—")
                    .monospaced()
            }
            LabeledContent("Disk") {
                Text(vm.hasDisk ? "disk.img" : "missing")
            }
            LabeledContent("Log") {
                Text(vm.hasLog ? "vm.log" : "—")
            }

            HStack {
                Button("Start") { store.startVM(vm, desktop: false) }
                    .disabled(vm.running || store.busy != nil)
                Button("Start Desktop") { store.startVM(vm, desktop: true) }
                    .disabled(vm.running || store.busy != nil)
                Button("Attach") { store.attach(vm) }
                    .disabled(!vm.running || store.busy != nil)
                Button("Stop", role: .destructive) { store.stop(vm) }
                    .disabled(!vm.running || store.busy != nil)
            }
            .buttonStyle(.bordered)

            HStack {
                Button("SSH") { store.ssh(vm) }
                    .disabled(!vm.running)
                Button("Open Folder") { store.openFolder(vm) }
                Button("Show Log") { store.openLog(vm) }
                    .disabled(!vm.hasLog)
            }
            .buttonStyle(.bordered)

            if !vm.logTail.isEmpty {
                Text("vm.log")
                    .font(.headline)
                ScrollView {
                    Text(vm.logTail)
                        .font(.system(.caption, design: .monospaced))
                        .frame(maxWidth: .infinity, alignment: .leading)
                        .textSelection(.enabled)
                }
                .background(Color(nsColor: .textBackgroundColor))
                .clipShape(RoundedRectangle(cornerRadius: 8))
            }

            Spacer()
        }
        .padding(24)
        .frame(maxWidth: .infinity, maxHeight: .infinity, alignment: .topLeading)
    }
}

struct StatusBadge: View {
    let running: Bool

    var body: some View {
        Text(running ? "Running" : "Stopped")
            .font(.caption.weight(.semibold))
            .padding(.horizontal, 10)
            .padding(.vertical, 4)
            .background(running ? Color.green.opacity(0.2) : Color.secondary.opacity(0.15), in: Capsule())
    }
}

struct NewVMSheet: View {
    @EnvironmentObject var store: VMStore
    @Environment(\.dismiss) private var dismiss
    @State private var name = "debian"
    @State private var desktop = false
    @State private var cpus = 2
    @State private var memMB = 2048
    @State private var diskGB = 8

    var body: some View {
        VStack(alignment: .leading, spacing: 16) {
            Text("New VM")
                .font(.title2.bold())
            TextField("Name", text: $name)
            Toggle("XFCE desktop (more RAM, slower first boot)", isOn: $desktop)
                .onChange(of: desktop) { on in
                    if on && memMB < 4096 { memMB = 4096 }
                    if !on && memMB == 4096 { memMB = 2048 }
                }
            Stepper("CPUs: \(cpus)", value: $cpus, in: 1...8)
            Stepper("Memory: \(memMB) MB", value: $memMB, in: 1024...8192, step: 512)
            Stepper("Disk: \(diskGB) GB", value: $diskGB, in: 3...64)
            Text("Directory: \(store.vmHome.path)/\(sanitized)")
                .font(.caption)
                .foregroundStyle(.secondary)
            HStack {
                Spacer()
                Button("Cancel") { dismiss() }
                Button("Create") {
                    store.create(
                        name: sanitized,
                        desktop: desktop,
                        cpus: cpus,
                        memMB: memMB,
                        diskGB: diskGB
                    )
                    dismiss()
                }
                .keyboardShortcut(.defaultAction)
                .disabled(sanitized.isEmpty)
            }
        }
        .padding(24)
        .frame(width: 460)
    }

    var sanitized: String {
        let trimmed = name.trimmingCharacters(in: .whitespacesAndNewlines)
        let ok = trimmed.unicodeScalars.map { CharacterSet.alphanumerics.union(CharacterSet(charactersIn: "-_")).contains($0) ? Character($0) : "-" }
        return String(ok)
    }
}
