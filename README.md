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

Headless. Login is `debian` / `debian`. This returns immediately. The guest keeps running after the terminal exits. A window opens with the display. Closing it, or Ctrl+C, leaves the guest running. `vm.log` is the host. Guest text is `console.log` in `--dir`.

Without `--dir`, files go under `/tmp/vmagent-<time>`.

The disk is not booted through GRUB. `vmagent` splits `vmlinuz` and `initrd.img` out of `/boot` and `vmcore` starts them with `VZLinuxBootLoader`. The kernel file Debian ships is already an uncompressed ARM64 Image (it also has an EFI stub). `root=` is copied from `grub.cfg`.

The generic image has no default password. `--user-data` attaches a disk labeled `cidata` and points cloud-init at it from the kernel command line. `user-data` must start with `#cloud-config`. Cloud-init applies it once per `instance-id`. `cloud-init/user-data` creates the user and starts sshd.

On a fresh disk, wait about a minute for cloud-init to install sshd. `console.log` shows the boot. With `--user-data`, the kernel command line also has `cloud-init=enabled`, so the same file shows `cloud-init` stage lines (`init-local`, `init`, `modules`, `final`). `guest started` in `vm.log` only means the host process is up.

## 4. Desktop

GNOME needs a bigger disk and more RAM than the headless boot. The Debian cloud image is about 3G; `task-gnome-desktop` does not fit. `vmagent` grows the working copy to 16G (`--disk-gb`) so growroot can expand the root filesystem on first boot.

Use a new `--dir`. Cloud-init runs once per disk. An earlier 3G boot that hit `No space left on device` will not retry packages.

```bash
bin/vmagent --image images/debian.raw \
  --user-data cloud-init/user-data-gui \
  --meta-data cloud-init/meta-data \
  --dir /tmp/vm-gui \
  --disk-gb 16 \
  --mem-mb 4096
```

`cloud-init/user-data-gui` is the same `debian` / `debian` user, plus `openssh-server` and `task-gnome-desktop`. It sets `graphical.target` and isolates it on first boot. `set-default` alone only changes the next boot; this image is already on `multi-user.target` by then. The window is the display: GDM, then the GNOME session. Close it and `bin/vmagent attach --dir /tmp/vm-gui` opens it again.

The first boot is slow. cloud-init refreshes apt, then installs the desktop. `console.log` can sit on `Reached target Cloud-init target.` for many minutes with no new line. That is the package install, not a hang. It is stuck if that line is unchanged for a long time and `bin/vmagent ssh --dir /tmp/vm-gui debian@vm` still says `no address`, or if `console.log` has `No space left on device`.

After cloud-init finishes, the login screen should appear in the window.

![GNOME login](docs/vm-desktop.png)

Log in as `debian` / `debian`. If this disk already ran the old `user-data-gui` (default only), GDM is installed but not started: `bin/vmagent ssh --dir /tmp/vm-gui debian@vm` then `sudo systemctl isolate graphical.target`. SSH works the same as headless, with `--dir /tmp/vm-gui`.

## 5. SSH and copy files

Each `--dir` gets a stable MAC. `ssh` and `scp` look that MAC up in `arp -an` and replace `vm` with the guest address. `no address` means the guest is not up yet.

Password is `debian`.

```bash
bin/vmagent ssh --dir /tmp/vm debian@vm
echo hello > /tmp/hello
bin/vmagent scp --dir /tmp/vm /tmp/hello debian@vm:/tmp/hello
bin/vmagent ssh --dir /tmp/vm debian@vm cat /tmp/hello
```

## 6. List, reattach, stop

```bash
bin/vmagent list
bin/vmagent attach --dir /tmp/vm
bin/vmagent stop --dir /tmp/vm
```

`list` asks `ps` for running `vmcore` processes and prints each disk's directory. There is no registry file, so a VM that is already running is listed, and `/tmp` and `/private/tmp` are not two entries. `attach` opens the window again. `stop` kills the VM.

## 7. Run commands and edit files

These use the same ssh session as `debian`. `--sudo` runs as root.

```bash
bin/vmagent run --dir /tmp/vm uname -a
bin/vmagent read --dir /tmp/vm /etc/os-release --offset 1 --limit 20
bin/vmagent write --dir /tmp/vm /tmp/hello --file ./hello
bin/vmagent edit --dir /tmp/vm /tmp/hello --old 'hello' --new 'hello world'
```
