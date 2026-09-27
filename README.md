# vmagent

Boot a Debian cloud image on macOS. Apple silicon, macOS 13+.

Rust does the setup. A small Swift program (`vmcore`) calls Virtualization.framework, because that framework has no C API and Rust cannot link it.

```bash
scripts/build.sh
scripts/fetch-debian.sh          # once
bin/vmagent --image images/debian.raw \
  --user-data cloud-init/user-data \
  --meta-data cloud-init/meta-data \
  --dir /tmp/vm
```

The guest starts in the background. A window opens with the console. The disk is not booted through GRUB. `vmagent` splits `vmlinuz` and `initrd.img` out of `/boot` and `vmcore` starts them with `VZLinuxBootLoader`. The kernel file Debian ships is already an uncompressed ARM64 Image (it also has an EFI stub). `root=` is copied from `grub.cfg`. Closing the window, or Ctrl+C in the terminal that started it, leaves the guest running. The working disk, kernel, and initrd are under `/tmp/vmagent-<time>` unless you pass `--dir`.

The generic image has no default password. `--user-data` attaches a disk labeled `cidata` and points cloud-init at it from the kernel command line. `user-data` must start with `#cloud-config`. Cloud-init applies it once per `instance-id`. `cloud-init/user-data` creates `debian` / `debian` and starts sshd. Each `--dir` gets a stable MAC. `ssh` and `scp` look that MAC up in the ARP cache and replace `vm` with the guest address.

On a fresh disk, wait about a minute for cloud-init to install sshd. Password is `debian`. The start command returns immediately. Console output goes to `vm.log` in `--dir`.

```bash
bin/vmagent ssh --dir /tmp/vm debian@vm
echo hello > /tmp/hello
bin/vmagent scp --dir /tmp/vm /tmp/hello debian@vm:/tmp/hello
bin/vmagent ssh --dir /tmp/vm debian@vm cat /tmp/hello
```

`no address` means the guest is not up yet. The MAC for `--dir` is matched against `arp -an`. ARP is many IPs; the MAC picks this VM.

`attach` opens the window again. `stop` kills the VM. The guest keeps running after the terminal exits.

```bash
bin/vmagent attach --dir /tmp/vm
bin/vmagent stop --dir /tmp/vm
```

`run`, `read`, `write`, and `edit` use that ssh session as `debian`. `--sudo` runs as root.

```bash
bin/vmagent run --dir /tmp/vm uname -a
bin/vmagent read --dir /tmp/vm /etc/os-release --offset 1 --limit 20
bin/vmagent write --dir /tmp/vm /tmp/hello --file ./hello
bin/vmagent edit --dir /tmp/vm /tmp/hello --old 'hello' --new 'hello world'
```
