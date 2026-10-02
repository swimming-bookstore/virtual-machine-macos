// Boot a Linux guest with a direct kernel.
// Usage: vmcore <disk.img> <kernel> <initrd> <cmdline> <cpus> <memory-mb> [cidata.raw]
// VZLinuxBootLoader takes the kernel command line. The disk is still the root filesystem.
// The window is the display. Closing it leaves the guest running.
// Guest text goes to console.log next to the disk (hvc0).
// vm.pid is written next to the disk. SIGUSR1 shows the window again.
// Attach always puts the display view back.
//
// VM_MAC is the Ethernet address. sshd listens on port 22.
// The host wrapper finds the guest by that MAC in the ARP cache.

import Cocoa
import Virtualization

let args = CommandLine.arguments
guard args.count == 7 || args.count == 8 else {
    fputs("usage: vmcore <disk.img> <kernel> <initrd> <cmdline> <cpus> <memory-mb> [cidata.raw]\n", stderr)
    exit(1)
}

let diskURL = URL(fileURLWithPath: args[1])
let kernelURL = URL(fileURLWithPath: args[2])
let initrdURL = URL(fileURLWithPath: args[3])
let cmdline = args[4]
let cpus = Int(args[5]) ?? 2
let memBytes = (UInt64(args[6]) ?? 2048) * 1024 * 1024
let cloudInitURL = args.count == 8 ? URL(fileURLWithPath: args[7]) : nil

let config = VZVirtualMachineConfiguration()
config.cpuCount = max(cpus, VZVirtualMachineConfiguration.minimumAllowedCPUCount)
config.memorySize = max(memBytes, VZVirtualMachineConfiguration.minimumAllowedMemorySize)

let disk = try VZDiskImageStorageDeviceAttachment(url: diskURL, readOnly: false)
var storage = [VZVirtioBlockDeviceConfiguration(attachment: disk)]
if let cloudInitURL {
    let cloudInit = try VZDiskImageStorageDeviceAttachment(url: cloudInitURL, readOnly: true)
    storage.append(VZVirtioBlockDeviceConfiguration(attachment: cloudInit))
}
config.storageDevices = storage

let net = VZVirtioNetworkDeviceConfiguration()
let mac: VZMACAddress
if let macStr = ProcessInfo.processInfo.environment["VM_MAC"], !macStr.isEmpty {
    guard let parsed = VZMACAddress(string: macStr) else {
        fputs("bad VM_MAC: \(macStr)\n", stderr)
        exit(1)
    }
    mac = parsed
} else {
    mac = VZMACAddress.randomLocallyAdministered()
}
net.macAddress = mac
net.attachment = VZNATNetworkDeviceAttachment()
config.networkDevices = [net]
config.entropyDevices = [VZVirtioEntropyDeviceConfiguration()]

let boot = VZLinuxBootLoader(kernelURL: kernelURL)
boot.initialRamdiskURL = initrdURL
boot.commandLine = cmdline
config.bootLoader = boot

let displayWidth = 1280
let displayHeight = 800
let gui = VZVirtioGraphicsDeviceConfiguration()
gui.scanouts = [VZVirtioGraphicsScanoutConfiguration(widthInPixels: displayWidth, heightInPixels: displayHeight)]
config.graphicsDevices = [gui]
config.keyboards = [VZUSBKeyboardConfiguration()]
config.pointingDevices = [VZUSBScreenCoordinatePointingDeviceConfiguration()]

// The window is the display. This file is the guest's text console.
let consoleURL = diskURL.deletingLastPathComponent().appendingPathComponent("console.log")
FileManager.default.createFile(atPath: consoleURL.path, contents: nil)
let console = VZVirtioConsoleDeviceSerialPortConfiguration()
console.attachment = VZFileHandleSerialPortAttachment(
    fileHandleForReading: nil,
    fileHandleForWriting: try FileHandle(forWritingTo: consoleURL)
)
config.serialPorts = [console]

try config.validate()

let vm = VZVirtualMachine(configuration: config)
let delegate = VMDelegate()
vm.delegate = delegate

let pidURL = diskURL.deletingLastPathComponent().appendingPathComponent("vm.pid")

func writePid() {
    let text = "\(ProcessInfo.processInfo.processIdentifier)\n"
    try? text.write(to: pidURL, atomically: true, encoding: .utf8)
}

func removePid() {
    try? FileManager.default.removeItem(at: pidURL)
}

class AppDelegate: NSObject, NSApplicationDelegate, NSWindowDelegate {
    var window: NSWindow!
    var displayView: VZVirtualMachineView!
    func applicationDidFinishLaunching(_ note: Notification) {
        writePid()
        displayView = VZVirtualMachineView(frame: NSRect(x: 0, y: 0, width: displayWidth, height: displayHeight))
        displayView.virtualMachine = vm
        displayView.autoresizingMask = [.width, .height]
        window = NSWindow(
            contentRect: displayView.frame,
            styleMask: [.titled, .closable, .miniaturizable, .resizable],
            backing: .buffered,
            defer: false
        )
        window.title = "debian"
        window.contentView = displayView
        window.delegate = self
        window.makeKeyAndOrderFront(nil)
        vm.start { result in
            if case .failure(let err) = result {
                fputs("start failed: \(err.localizedDescription)\n", stderr)
                removePid()
                exit(1)
            }
            fputs("guest started\n", stderr)
            // Do not reconfigure here. A reconfigure before the guest has
            // programmed the scanout clears the current mode, and the window
            // stays black. The scanout is already displayWidth x displayHeight.
        }
    }
    func showWindow() {
        window.contentView = displayView
        NSApp.setActivationPolicy(.regular)
        NSApp.activate(ignoringOtherApps: true)
        window.makeKeyAndOrderFront(nil)
        fputs("window shown\n", stderr)
    }
    func windowShouldClose(_ sender: NSWindow) -> Bool {
        sender.orderOut(nil)
        NSApp.setActivationPolicy(.accessory)
        fputs("window closed, guest still running\n", stderr)
        return false
    }
}

class VMDelegate: NSObject, VZVirtualMachineDelegate {
    func guestDidStop(_ vm: VZVirtualMachine) {
        fputs("guest stopped\n", stderr)
        removePid()
        exit(0)
    }
    func virtualMachine(_ vm: VZVirtualMachine, didStopWithError err: Error) {
        fputs("guest error: \(err.localizedDescription)\n", stderr)
        removePid()
        exit(1)
    }
}

let app = NSApplication.shared
let appDelegate = AppDelegate()
app.setActivationPolicy(.regular)
app.delegate = appDelegate
app.activate(ignoringOtherApps: true)
signal(SIGUSR1, SIG_IGN)
let src = DispatchSource.makeSignalSource(signal: SIGUSR1, queue: .main)
src.setEventHandler { appDelegate.showWindow() }
src.resume()
app.run()
