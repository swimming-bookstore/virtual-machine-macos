# vmagent

Boot a Debian cloud image on macOS. Apple silicon, macOS 13+.

Rust does the setup. A small Swift program (`vmcore`) calls Virtualization.framework, because that framework has no C API and Rust cannot link it.

```bash
scripts/build.sh
scripts/fetch-debian.sh          # once
bin/vmagent --image images/debian.raw
bin/vmagent --image images/debian.raw --user-data cloud-init/user-data --meta-data cloud-init/meta-data
```

A window opens with the guest console. The disk is not booted through GRUB. `vmagent` splits `vmlinuz` and `initrd.img` out of `/boot` and `vmcore` starts them with `VZLinuxBootLoader`. The kernel file Debian ships is already an uncompressed ARM64 Image (it also has an EFI stub). `root=` is copied from `grub.cfg`. Close the window to stop. The working disk, kernel, and initrd are under `/tmp/vmagent-<time>` unless you pass `--dir`.

The generic image has no default password. Pass `--user-data` so cloud-init can create one. That attaches a disk labeled `cidata` and points cloud-init at it from the kernel command line. `user-data` must start with `#cloud-config`. Cloud-init applies it once per `instance-id`. `cloud-init/user-data` creates `debian` with password `debian` and starts sshd on port 22. Each VM gets its own MAC, written to `<dir>/ssh.mac`. Pass that same `--dir` to `ssh` or `scp`. The wrapper looks the MAC up in the DHCP leases and replaces `vm` with the guest address.

```bash
bin/vmagent ssh --dir /tmp/vmagent-... debian@vm
bin/vmagent scp --dir /tmp/vmagent-... ./hello debian@vm:hello
```

Wait until cloud-init has installed sshd before the first connection. That takes a minute on a fresh disk.
