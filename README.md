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

Headless. Login is `debian` / `debian`. The command returns immediately. The guest keeps running after the terminal exits. Closing the window, or Ctrl+C, does not stop it.

`--dir` holds the disk copy, `vm.log` (host), and `console.log` (guest). Without it, files go under `/tmp/vmagent-<time>`.

`vmagent` copies the image, splits `vmlinuz` and `initrd.img` out of `/boot`, and `vmcore` starts them with `VZLinuxBootLoader`. `root=` comes from `grub.cfg`. `--user-data` attaches a `cidata` disk and points cloud-init at it. Cloud-init runs once per `instance-id`.

Wait about a minute on a fresh disk. `guest started` in `vm.log` only means the host process is up.

## 4. Desktop

Same image. Pass `cloud-init/user-data-gui` and more RAM. The working disk grows to 8G (`--disk-gb`) so XFCE fits. Use a new `--dir`. Cloud-init will not retry packages on a disk that already ran.

```bash
bin/vmagent --image images/debian.raw \
  --user-data cloud-init/user-data-gui \
  --meta-data cloud-init/meta-data \
  --dir /tmp/vm-gui \
  --mem-mb 4096
```

First boot is slow. `console.log` can sit on `Reached target Cloud-init target.` for many minutes while apt runs. LightDM autologins `debian` on the virtio display. That is the XFCE `attach` shows. x11vnc shares `:0`; xrdp's `libvnc.so` connects to it, so the RDP window is the same session. Nothing extra is launched for you.

xrdp listens on 3389 inside the guest. NAT does not publish that port. `xrdp` forwards it over ssh and opens a client:

```bash
bin/vmagent xrdp --dir /tmp/vm-gui
```

That prefers `xfreerdp` (`brew install freerdp`). Without it, Windows App opens with NLA off — Windows App's default CredSSP login fails against xrdp. `--tunnel-only` just forwards the port. `--port` picks the local port when 3389 is taken.

A command after `--` starts on that desktop. With no command, nothing is opened.

```bash
bin/vmagent xrdp --dir /tmp/vm-gui -- firefox-esr https://www.youtube.com/
```

Close the window and open it again with `bin/vmagent attach --dir /tmp/vm-gui`.

## 5. Firefox

Firefox is a process on the machine, driven over WebDriver BiDi, not by clicking the window. `open` kills the previous window, wipes `~/.cache/vmagent/firefox`, sideloads uBlock Origin, and starts `firefox-esr` on the xrdp display with `--remote-debugging-port`. Default port is 9333. The port stays inside the guest. NAT does not publish it.

[demo-firefox.mp4](docs/demo-firefox.mp4) is that path: search YouTube, open a video. Frames are `browsingContext.captureScreenshot`. Input is `input.performActions`. Nothing is typed into the X window.

```bash
bin/vmagent firefox --dir /tmp/vm-gui open https://www.youtube.com/
bin/vmagent firefox --dir /tmp/vm-gui tabs
bin/vmagent firefox --dir /tmp/vm-gui goto https://example.com/
bin/vmagent firefox --dir /tmp/vm-gui eval 'document.title'
bin/vmagent firefox --dir /tmp/vm-gui close
```

`open` with one URL is one tab. Extra URLs are extra tabs. `tabs` marks the active tab with `*`. `close` with no id closes every tab except that one. `close <id>` closes that tab.

## 6. SSH and copy files

Each `--dir` gets a stable MAC. `ssh` and `scp` look that MAC up in `arp -an` and replace `vm` with the guest address. `no address` means the guest is not up yet. Password is `debian`.

```bash
bin/vmagent ssh --dir /tmp/vm debian@vm
echo hello > /tmp/hello
bin/vmagent scp --dir /tmp/vm /tmp/hello debian@vm:/tmp/hello
bin/vmagent ssh --dir /tmp/vm debian@vm cat /tmp/hello
```

## 7. List, reattach, stop

```bash
bin/vmagent list
bin/vmagent attach --dir /tmp/vm
bin/vmagent stop --dir /tmp/vm
```

`list` reads running `vmcore` processes. `attach` opens the window. `stop` kills the VM.

## 8. Run commands and edit files

Same ssh session as `debian`. `--sudo` runs as root.

```bash
bin/vmagent run --dir /tmp/vm uname -a
bin/vmagent read --dir /tmp/vm /etc/os-release --offset 1 --limit 20
bin/vmagent write --dir /tmp/vm /tmp/hello --file ./hello
bin/vmagent edit --dir /tmp/vm /tmp/hello --old 'hello' --new 'hello world'
```

## 9. Computer use

Desktop guests only. `screenshot` saves a PNG of the virtio XFCE (`:0`). `input`, `click`, `key`, and `type` go to that same session. `attach` and `xrdp` show it too.

[demo-computer-use.mp4](docs/demo-computer-use.mp4) is that path: open Firefox, search YouTube, play a video. Frames are the X root. Input is `click` / `key` / `type`.

Input runs `xdotool` on `:0` (XTest). The Virtualization window, the RDP client, and these commands share that display. `xdotool` is installed by `cloud-init/user-data-gui`.

Pointer coordinates are pixels of that frame, the same size `screenshot` writes. A click is a move, then a button press and release. A tap is `mousedown` and `mouseup`. Scrolling is the same, one step at a time.

```bash
bin/vmagent screenshot --dir /tmp/vm-gui --out shot.png
bin/vmagent click --dir /tmp/vm-gui 500 400
bin/vmagent input --dir /tmp/vm-gui <<'EOF'
type hello
key enter
move 640 400
button left
EOF
```

`key leftctrl down` stays down until `key leftctrl up` in the same call. `scroll -3` scrolls down. LightDM logs in as `debian` with no dialog.

```bash
bin/vmagent click --dir /tmp/vm-gui 500 560
bin/vmagent key --dir /tmp/vm-gui enter
bin/vmagent key --dir /tmp/vm-gui leftctrl+w
bin/vmagent type --dir /tmp/vm-gui hello
```

`key` takes one name, or names joined with `+`. A chord is held and released in one call, so it works on whatever has focus: a Firefox tab, a console tab, or another window. There is no command for a particular app.

## 10. JSON-RPC

`serve` keeps one process up and takes the same calls over a unix socket, so a click is not a new `vmagent` process. One JSON object per line, JSON-RPC 2.0. Default socket is `<dir>/rpc.sock`. A second `serve` on the same socket replaces the first.

```bash
bin/vmagent serve --dir /tmp/vm-gui
```

```bash
printf '%s\n' '{"jsonrpc":"2.0","id":1,"method":"click","params":{"x":640,"y":400}}' | nc -U /tmp/vm-gui/rpc.sock
printf '%s\n' '{"jsonrpc":"2.0","id":2,"method":"key","params":{"name":"leftctrl+t"}}' | nc -U /tmp/vm-gui/rpc.sock
printf '%s\n' '{"jsonrpc":"2.0","id":3,"method":"type","params":{"text":"hello"}}' | nc -U /tmp/vm-gui/rpc.sock
printf '%s\n' '{"jsonrpc":"2.0","id":4,"method":"screenshot","params":{"out":"shot.png"}}' | nc -U /tmp/vm-gui/rpc.sock
```

Methods: `click` (`x`, `y`, optional `button`), `key` (`name`), `type` (`text`), `input` (`lines`, the input commands), `screenshot` (`out`), `display`, `ping`. A notification (no `id`) gets no reply.

```bash
bin/vmagent key --dir /tmp/vm-gui leftctrl+t
bin/vmagent key --dir /tmp/vm-gui leftctrl+l
bin/vmagent type --dir /tmp/vm-gui https://www.example.com
bin/vmagent key --dir /tmp/vm-gui enter
bin/vmagent key --dir /tmp/vm-gui leftctrl+leftshift+tab
bin/vmagent key --dir /tmp/vm-gui leftalt+f4
```
