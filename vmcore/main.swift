// Boot a Linux guest with a direct kernel.
// Usage: vmcore <disk.img> <kernel> <initrd> <cmdline> <cpus> <memory-mb> [cidata.raw]
// VZLinuxBootLoader takes the kernel command line. The disk is still the root filesystem.
// A GUI window is required: no serial console on this path.
//
// VM_SSH_MAC_FILE is where this VM's MAC is written. sshd listens on port 22.
// The host wrapper finds the guest by that MAC in the DHCP leases.

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
let mac = VZMACAddress.randomLocallyAdministered()
net.macAddress = mac
net.attachment = VZNATNetworkDeviceAttachment()
config.networkDevices = [net]
config.entropyDevices = [VZVirtioEntropyDeviceConfiguration()]

let boot = VZLinuxBootLoader(kernelURL: kernelURL)
boot.initialRamdiskURL = initrdURL
boot.commandLine = cmdline
config.bootLoader = boot

let gui = VZVirtioGraphicsDeviceConfiguration()
gui.scanouts = [VZVirtioGraphicsScanoutConfiguration(widthInPixels: 1280, heightInPixels: 800)]
config.graphicsDevices = [gui]
config.keyboards = [VZUSBKeyboardConfiguration()]
config.pointingDevices = [VZUSBScreenCoordinatePointingDeviceConfiguration()]

try config.validate()

let vm = VZVirtualMachine(configuration: config)
let delegate = VMDelegate()
vm.delegate = delegate

class AppDelegate: NSObject, NSApplicationDelegate, NSWindowDelegate {
    var window: NSWindow!
    func applicationDidFinishLaunching(_ note: Notification) {
        let view = VZVirtualMachineView(frame: NSRect(x: 0, y: 0, width: 1280, height: 800))
        view.virtualMachine = vm
        window = NSWindow(
            contentRect: view.frame,
            styleMask: [.titled, .closable, .miniaturizable, .resizable],
            backing: .buffered,
            defer: false
        )
        window.title = "debian"
        window.contentView = view
        window.delegate = self
        window.makeKeyAndOrderFront(nil)
        if let macFile = ProcessInfo.processInfo.environment["VM_SSH_MAC_FILE"], !macFile.isEmpty {
            do {
                try "\(mac.string)\n".write(toFile: macFile, atomically: true, encoding: .utf8)
            } catch {
                fputs("ssh: cannot write \(macFile): \(error.localizedDescription)\n", stderr)
                exit(1)
            }
            fputs("ssh mac: \(mac.string)\n", stderr)
        }
        vm.start { result in
            if case .failure(let err) = result {
                fputs("start failed: \(err.localizedDescription)\n", stderr)
                exit(1)
            }
            fputs("guest started\n", stderr)
        }
    }
    func windowWillClose(_ note: Notification) {
        exit(0)
    }
}

class VMDelegate: NSObject, VZVirtualMachineDelegate {
    func guestDidStop(_ vm: VZVirtualMachine) {
        fputs("guest stopped\n", stderr)
        exit(0)
    }
    func virtualMachine(_ vm: VZVirtualMachine, didStopWithError err: Error) {
        fputs("guest error: \(err.localizedDescription)\n", stderr)
        exit(1)
    }
}

let app = NSApplication.shared
let appDelegate = AppDelegate()
app.setActivationPolicy(.regular)
app.delegate = appDelegate
app.activate(ignoringOtherApps: true)
app.run()
