# vmagent

Boot a Debian cloud image on macOS. Apple silicon, macOS 13+.

Rust does the setup. A small Swift program (`vmcore`) calls Virtualization.framework, because that framework has no C API and Rust cannot link it.

## 1. Build

```bash
scripts/build.sh
```

## 2. Fetch the image

Once:

```bash
scripts/fetch-debian.sh
```

## 3. Start a VM

```bash
bin/vmagent --image images/debian.raw \
  --user-data cloud-init/user-data \
  --meta-data cloud-init/meta-data \
  --dir /tmp/vm
```

This returns immediately. The guest keeps running after the terminal exits. A window opens with the console. Closing it, or Ctrl+C, leaves the guest running. Console output is `vm.log` in `--dir`.

Without `--dir`, files go under `/tmp/vmagent-<time>`.

The disk is not booted through GRUB. `vmagent` splits `vmlinuz` and `initrd.img` out of `/boot` and `vmcore` starts them with `VZLinuxBootLoader`. The kernel file Debian ships is already an uncompressed ARM64 Image (it also has an EFI stub). `root=` is copied from `grub.cfg`.

The generic image has no default password. `--user-data` attaches a disk labeled `cidata` and points cloud-init at it from the kernel command line. `user-data` must start with `#cloud-config`. Cloud-init applies it once per `instance-id`. `cloud-init/user-data` creates `debian` / `debian` and starts sshd.

On a fresh disk, wait about a minute for cloud-init to install sshd.

## 4. SSH and copy files

Each `--dir` gets a stable MAC. `ssh` and `scp` look that MAC up in `arp -an` and replace `vm` with the guest address. `no address` means the guest is not up yet.

Password is `debian`.

```bash
bin/vmagent ssh --dir /tmp/vm debian@vm
echo hello > /tmp/hello
bin/vmagent scp --dir /tmp/vm /tmp/hello debian@vm:/tmp/hello
bin/vmagent ssh --dir /tmp/vm debian@vm cat /tmp/hello
```

## 5. List, reattach, stop

```bash
bin/vmagent list
bin/vmagent attach --dir /tmp/vm
bin/vmagent stop --dir /tmp/vm
```

`list` asks `ps` for running `vmcore` processes and prints each disk's directory. There is no registry file, so a VM that is already running is listed, and `/tmp` and `/private/tmp` are not two entries. `attach` opens the window again. `stop` kills the VM.

## 6. Run commands and edit files

These use the same ssh session as `debian`. `--sudo` runs as root.

```bash
bin/vmagent run --dir /tmp/vm uname -a
bin/vmagent read --dir /tmp/vm /etc/os-release --offset 1 --limit 20
bin/vmagent write --dir /tmp/vm /tmp/hello --content 'hello'
bin/vmagent edit --dir /tmp/vm /tmp/hello --old 'hello' --new 'hello world'
```
