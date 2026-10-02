//! Boot Debian on macOS.
//!
//! Apple's Virtualization.framework has no C API, so the VM itself is a tiny
//! Swift program (`vmcore`). This binary copies the cloud image, splits the
//! kernel and initrd out of it, and starts that helper.
//!
//!   vmagent --image debian.raw --user-data cloud-init/user-data --meta-data cloud-init/meta-data
//!
//! `ssh` and `scp` wrap the host tools. `user@vm` is the guest. The wrapper
//! derives the MAC from `--dir` and looks up that address in ARP. sshd listens on port 22.
//!
//!   vmagent --dir /tmp/vm --image debian.raw --user-data cloud-init/user-data
//!   vmagent ssh --dir /tmp/vm debian@vm
//!   vmagent scp --dir /tmp/vm ./hello debian@vm:hello
//!   vmagent run --dir /tmp/vm uname -a
//!   vmagent read --dir /tmp/vm /etc/os-release --offset 1 --limit 20
//!   vmagent write --dir /tmp/vm /tmp/hello --file ./hello
//!   vmagent edit --dir /tmp/vm /tmp/hello --old 'hello' --new 'hello world'
//!   vmagent screenshot --dir /tmp/vm --out shot.png
//!   vmagent input --dir /tmp/vm <<'EOF'
//!   type hello
//!   key enter
//!   EOF
//!   vmagent click --dir /tmp/vm 500 560
//!   vmagent key --dir /tmp/vm leftctrl+w
//!   vmagent attach --dir /tmp/vm
//!   vmagent stop --dir /tmp/vm
//!   vmagent list
//!   vmagent xrdp --dir /tmp/vm-gui
//!   vmagent xrdp --dir /tmp/vm-gui -- firefox-esr https://www.youtube.com/
//!   vmagent firefox --dir /tmp/vm-gui open https://www.youtube.com/
//!   vmagent firefox --dir /tmp/vm-gui tabs
//!   vmagent serve --dir /tmp/vm
//!
//! `serve` listens on `<dir>/rpc.sock` for JSON-RPC 2.0, one object per line.
//! Methods: screenshot, click, key, type, input, display, ping. A second serve on the same socket replaces the first.
//! The guest is started in its own session. Closing the window or the terminal
//! leaves it running. `attach` opens the window again. `stop` kills it.
//! `list` prints the directory of each running `vmcore`. No registry file.

use std::fs::{self, File};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::os::unix::process::CommandExt;
use std::process::{Command, Stdio};

extern "C" {
    fn setsid() -> i32;
}

use clap::{Parser, Subcommand};

#[derive(Parser)]
#[command(name = "vmagent", version, about = "Boot a Debian cloud image on macOS")]
#[command(subcommand_negates_reqs = true)]
struct Cli {
    #[command(subcommand)]
    cmd: Option<Cmd>,

    /// Path to a Debian generic raw image (arm64 on Apple silicon).
    #[arg(long, required = true)]
    image: Option<PathBuf>,

    /// Where the working disk, kernel, and initrd go.
    /// Defaults to a new directory under /tmp.
    #[arg(long)]
    dir: Option<PathBuf>,

    /// cloud-config. Must start with `#cloud-config`. Attached as a cidata disk.
    #[arg(long)]
    user_data: Option<PathBuf>,

    /// cloud-init meta-data. Defaults to a one-line instance-id if omitted.
    #[arg(long)]
    meta_data: Option<PathBuf>,

    #[arg(long, default_value_t = 2)]
    cpus: u32,

    #[arg(long, default_value_t = 2048)]
    mem_mb: u64,

    /// Working disk size in GiB. The copied image is grown so growroot can
    /// expand the root filesystem. XFCE does not fit in the 3G cloud image.
    /// 0 leaves the image size.
    #[arg(long, default_value_t = 8)]
    disk_gb: u64,
}

#[derive(Subcommand)]
enum Cmd {
    /// Run ssh against the VM in `--dir`. Use `user@vm` for the guest.
    Ssh {
        #[arg(long)]
        dir: PathBuf,
        /// Arguments passed to ssh after the connection options.
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        args: Vec<String>,
    },
    /// Run scp against the VM in `--dir`. Use `user@vm:path` for the guest.
    Scp {
        #[arg(long)]
        dir: PathBuf,
        /// Arguments passed to scp after the connection options.
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        args: Vec<String>,
    },
    /// Run a command in the guest. Prints stdout and stderr.
    Run {
        #[arg(long)]
        dir: PathBuf,
        /// Run as root.
        #[arg(long)]
        sudo: bool,
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        command: Vec<String>,
    },
    /// Print a guest file. `--offset` is the 1-based start line.
    Read {
        #[arg(long)]
        dir: PathBuf,
        /// Read as root.
        #[arg(long)]
        sudo: bool,
        path: String,
        /// Line to start at (1-based).
        #[arg(long, default_value_t = 1)]
        offset: usize,
        /// Maximum number of lines.
        #[arg(long, default_value_t = 2000)]
        limit: usize,
    },
    /// Create or overwrite a guest file. Parent directories are created.
    Write {
        #[arg(long)]
        dir: PathBuf,
        /// Write as root.
        #[arg(long)]
        sudo: bool,
        path: String,
        /// Local file to write. Omit to read contents from stdin.
        #[arg(long)]
        file: Option<PathBuf>,
    },
    /// Replace one exact block of text in a guest file. `--old` must match once.
    Edit {
        #[arg(long)]
        dir: PathBuf,
        /// Edit as root.
        #[arg(long)]
        sudo: bool,
        path: String,
        /// Exact text to replace. Must occur once.
        #[arg(long)]
        old: String,
        /// Replacement text.
        #[arg(long)]
        new: String,
    },
    /// Save the guest desktop as a PNG.
    ///
    /// The picture is the virtio X session (`:0`), the same XFCE `attach` and
    /// xrdp show. `cloud-init/user-data-gui` autologins `debian` there.
    Screenshot {
        #[arg(long)]
        dir: PathBuf,
        /// Where to write the PNG. Default: screenshot.png in the current directory.
        #[arg(long)]
        out: Option<PathBuf>,
    },
    /// Send keyboard and pointer events to the guest desktop.
    ///
    /// Commands are arguments, or lines on stdin when no arguments are given:
    /// `key <name> [down|up]`, `type <text>`, `move <x> <y>`,
    /// `button <left|right|middle> [down|up]`, `scroll <n>`.
    /// A `down` stays down until a later `up` in the same call.
    Input {
        #[arg(long)]
        dir: PathBuf,
        /// Commands. Each argument is one line (`move 10 20`).
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        commands: Vec<String>,
    },
    /// Click at desktop pixels.
    ///
    /// Coordinates are pixels of the virtio display, the same frame `screenshot` saves.
    Click {
        #[arg(long)]
        dir: PathBuf,
        x: i32,
        y: i32,
        /// left, right, or middle.
        #[arg(long, default_value = "left")]
        button: String,
    },
    /// Press a key, or a chord.
    ///
    /// One name is a tap: `enter`, `a`, `f4`. Names joined with `+` are held
    /// together and released together: `leftctrl+w`, `leftctrl+leftshift+tab`,
    /// `leftalt+f4`. That is how tabs, consoles, and other windows are closed.
    /// There is no app-specific command.
    Key {
        #[arg(long)]
        dir: PathBuf,
        /// Linux KEY_* names without the prefix, joined with `+`.
        name: String,
    },
    /// Type text.
    Type {
        #[arg(long)]
        dir: PathBuf,
        text: String,
    },
    /// Open the display window again. The guest keeps running if the window was closed.
    Attach {
        #[arg(long)]
        dir: PathBuf,
    },
    /// Stop the VM. This is a hard stop, not a guest shutdown.
    Stop {
        #[arg(long)]
        dir: PathBuf,
    },
    /// Print directories of VMs that are still running.
    List,
    /// Open the guest desktop over xrdp. NAT does not publish port 3389,
    /// so this tunnels it over ssh and starts an RDP client on localhost.
    ///
    /// The GUI cloud-init autologins XFCE on `:0` and points xrdp at that
    /// display over localhost VNC. Login is `debian` / `debian`.
    /// Windows App rejects that login (CredSSP against xrdp), so this uses
    /// `xfreerdp` when it is installed and falls back to Windows App only
    /// with NLA turned off.
    ///
    /// Arguments after `--` are a command to start on that desktop. Nothing
    /// is started when they are omitted. The command runs on the session
    /// that is already up, and on the next connection.
    Xrdp {
        #[arg(long)]
        dir: PathBuf,
        /// Local port forwarded to the guest's 3389. Default: 3389.
        #[arg(long, default_value_t = 3389)]
        port: u16,
        /// Only start the ssh tunnel. Print the client command and return.
        #[arg(long)]
        tunnel_only: bool,
        /// Command to start on the xrdp desktop, after `--`.
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        open: Vec<String>,
    },
    /// Start Firefox on the xrdp desktop and drive it over WebDriver BiDi.
    ///
    /// A clean profile, `--remote-debugging-port`, then commands on
    /// `ws://127.0.0.1:<port>/session`. The port stays on the guest.
    /// `ssh` forwards it. Nothing is typed into the window.
    ///
    /// `open` kills any Firefox this command started, wipes the profile, and
    /// starts one window. Extra URLs are extra tabs. `tabs`, `goto`, `eval`,
    /// and `close` talk to the Firefox that is already listening.
    Firefox {
        #[arg(long)]
        dir: PathBuf,
        /// Guest BiDi port. Default: 9333.
        #[arg(long, default_value_t = 9333)]
        port: u16,
        #[command(subcommand)]
        action: FirefoxAction,
    },
    /// JSON-RPC 2.0 server for screenshot and input. One process, one socket.
    ///
    /// Methods: screenshot, click, key, type, input, display.
    /// `input` takes a `lines` array of helper commands.
    /// A second `serve` against the same `--dir` replaces the first.
    Serve {
        #[arg(long)]
        dir: PathBuf,
        /// Unix socket path. Default: `<dir>/rpc.sock`.
        #[arg(long)]
        socket: Option<PathBuf>,
    },
}

#[derive(Subcommand)]
enum FirefoxAction {
    /// Close the previous window, start a clean one, open these URLs.
    Open {
        /// Pages to open. The first is the only tab when no others are given.
        urls: Vec<String>,
    },
    /// Print id, url, and title of each tab. The active tab is marked.
    Tabs,
    /// Load a URL in the active tab.
    Goto { url: String },
    /// Run JavaScript in the active tab and print the result.
    Eval { expression: String },
    /// Close a tab by id. With no id, close every tab except the active one.
    Close { id: Option<String> },
}

fn main() {
    let cli = Cli::parse();
    if let Some(cmd) = cli.cmd {
        match cmd {
            Cmd::Ssh { dir, args } => ssh_cmd(&dir, &args),
            Cmd::Scp { dir, args } => scp_cmd(&dir, &args),
            Cmd::Run { dir, sudo, command } => run_cmd(&dir, sudo, &command),
            Cmd::Read {
                dir,
                sudo,
                path,
                offset,
                limit,
            } => read_cmd(&dir, sudo, &path, offset, limit),
            Cmd::Write { dir, sudo, path, file } => write_cmd(&dir, sudo, &path, file.as_deref()),
            Cmd::Edit {
                dir,
                sudo,
                path,
                old,
                new,
            } => edit_cmd(&dir, sudo, &path, &old, &new),
            Cmd::Screenshot { dir, out } => screenshot_cmd(&dir, out.as_deref()),
            Cmd::Input { dir, commands } => input_cmd(&dir, &commands),
            Cmd::Click { dir, x, y, button } => click_cmd(&dir, x, y, &button),
            Cmd::Key { dir, name } => key_cmd(&dir, &name),
            Cmd::Type { dir, text } => type_cmd(&dir, &text),
            Cmd::Attach { dir } => signal_vm(&dir, "-USR1"),
            Cmd::Stop { dir } => signal_vm(&dir, "-TERM"),
            Cmd::List => list_cmd(),
            Cmd::Xrdp {
                dir,
                port,
                tunnel_only,
                open,
            } => xrdp_cmd(&dir, port, tunnel_only, &open),
            Cmd::Firefox { dir, port, action } => firefox_cmd(&dir, port, action),
            Cmd::Serve { dir, socket } => serve_cmd(&dir, socket.as_deref()),
        }
    }

    if !cfg!(target_os = "macos") {
        die("this only runs on macOS");
    }
    let image = cli.image.unwrap_or_else(|| die("--image is required"));
    if !image.is_file() {
        die(&format!("image not found: {}", image.display()));
    }

    let dir = cli.dir.unwrap_or_else(tmp_dir);
    if vm_alive(&dir) {
        die("already running; attach to open the window");
    }
    if let Err(e) = fs::create_dir_all(&dir) {
        die(&format!("cannot create {}: {e}", dir.display()));
    }
    let disk = dir.join("disk.img");
    if !disk.exists() {
        eprintln!("copying image to {}", disk.display());
        if let Err(e) = fs::copy(&image, &disk) {
            die(&format!("copy failed: {e}"));
        }
    }
    grow_disk(&disk, cli.disk_gb);

    let cloud_init = match (&cli.user_data, &cli.meta_data) {
        (None, None) => None,
        _ => Some(write_cidata(
            &dir,
            cli.user_data.as_deref(),
            cli.meta_data.as_deref(),
        )),
    };

    let append = if cloud_init.is_some() {
        "console=hvc0 ds=nocloud"
    } else {
        "console=hvc0"
    };
    let cmdline = split_image(&disk, &dir, append);
    eprintln!("kernel command line: {cmdline}");

    let vmcore = find_vmcore();
    eprintln!("booting with {}", vmcore.display());
    if cloud_init.is_none() {
        eprintln!("no user-data: the generic image has no default login");
    }
    let mac = mac_for_dir(&dir);
    eprintln!("ssh: vmagent ssh --dir {} debian@vm", dir.display());
    eprintln!("close the window or this terminal; attach to open the window again");

    let log = File::create(dir.join("vm.log"))
        .unwrap_or_else(|e| die(&format!("cannot write log: {e}")));
    let err = log
        .try_clone()
        .unwrap_or_else(|e| die(&format!("cannot write log: {e}")));
    let mut cmd = Command::new(&vmcore);
    cmd.env("VM_MAC", &mac);
    cmd.arg(&disk)
        .arg(dir.join("vmlinuz"))
        .arg(dir.join("initrd"))
        .arg(&cmdline)
        .arg(cli.cpus.to_string())
        .arg(cli.mem_mb.to_string());
    if let Some(cloud_init) = &cloud_init {
        cmd.arg(cloud_init);
    }
    // Own session so Ctrl+C in this terminal does not kill the guest.
    cmd.stdin(Stdio::null())
        .stdout(Stdio::from(log))
        .stderr(Stdio::from(err));
    unsafe {
        cmd.pre_exec(|| {
            setsid();
            Ok(())
        });
    }
    match cmd.spawn() {
        Ok(_) => {}
        Err(e) => die(&format!("failed to run {}: {e}", vmcore.display())),
    }
    eprintln!("guest started in the background");
    eprintln!("log: {}", dir.join("vm.log").display());
}

/// Same directory always gets the same locally-administered MAC.
fn mac_for_dir(dir: &Path) -> String {
    let path = fs::canonicalize(dir).unwrap_or_else(|_| dir.to_path_buf());
    let mut h: u64 = 0xcbf29ce484222325;
    for b in path.to_string_lossy().as_bytes() {
        h ^= *b as u64;
        h = h.wrapping_mul(0x100000001b3);
    }
    let bytes = [
        ((h >> 40) as u8 & 0xfe) | 0x02,
        (h >> 32) as u8,
        (h >> 24) as u8,
        (h >> 16) as u8,
        (h >> 8) as u8,
        h as u8,
    ];
    format!(
        "{:02x}:{:02x}:{:02x}:{:02x}:{:02x}:{:02x}",
        bytes[0], bytes[1], bytes[2], bytes[3], bytes[4], bytes[5]
    )
}

#[cfg(test)]
fn is_mac(mac: &str) -> bool {
    normalize_mac(mac).is_some()
}

/// Lowercase, colon-separated, two digits per octet. ARP omits leading zeros.
fn normalize_mac(mac: &str) -> Option<String> {
    let parts: Vec<&str> = mac.split(|c| c == ':' || c == '-').collect();
    if parts.len() != 6 {
        return None;
    }
    let mut out = String::with_capacity(17);
    for (i, p) in parts.iter().enumerate() {
        if p.is_empty() || p.len() > 2 || !p.chars().all(|c| c.is_ascii_hexdigit()) {
            return None;
        }
        if i > 0 {
            out.push(':');
        }
        if p.len() == 1 {
            out.push('0');
        }
        out.push_str(&p.to_ascii_lowercase());
    }
    Some(out)
}

fn ip_from_arp(text: &str, wanted: &str) -> Option<String> {
    for line in text.lines() {
        let Some((before, after)) = line.split_once(") at ") else {
            continue;
        };
        let Some((_, ip)) = before.rsplit_once('(') else {
            continue;
        };
        let Some(mac_part) = after.split_whitespace().next() else {
            continue;
        };
        if mac_part.eq_ignore_ascii_case("(incomplete)") {
            continue;
        }
        if normalize_mac(mac_part).as_deref() == Some(wanted) {
            return Some(ip.to_string());
        }
    }
    None
}

/// Guest address from the ARP cache. ARP is IP→MAC; the MAC picks this VM.
fn guest_ip(mac: &str) -> String {
    let wanted = normalize_mac(mac).unwrap_or_else(|| die("bad MAC"));
    let arp = Command::new("arp")
        .arg("-an")
        .output()
        .unwrap_or_else(|e| die(&format!("cannot run arp: {e}")));
    if arp.status.success() {
        let text = String::from_utf8_lossy(&arp.stdout);
        if let Some(ip) = ip_from_arp(&text, &wanted) {
            return ip;
        }
    }
    die(&format!("no address for {wanted} yet (is the guest up?)"));
}

fn ssh_base() -> Vec<String> {
    vec![
        "-o".into(),
        "StrictHostKeyChecking=accept-new".into(),
        "-o".into(),
        "UserKnownHostsFile=/dev/null".into(),
        "-o".into(),
        "LogLevel=ERROR".into(),
        "-o".into(),
        "PreferredAuthentications=password".into(),
        "-o".into(),
        "PubkeyAuthentication=no".into(),
        "-o".into(),
        "NumberOfPasswordPrompts=1".into(),
    ]
}

/// Guest login is `debian`. OpenSSH 8.4+ reads this instead of a tty.
fn ssh_cmd_with_password(tool: &str) -> Command {
    let mut cmd = Command::new(tool);
    cmd.env("DISPLAY", "1")
        .env("SSH_ASKPASS_REQUIRE", "force")
        .env("SSH_ASKPASS", askpass_path());
    cmd
}

fn askpass_path() -> PathBuf {
    if let Some(p) = std::env::var_os("VMAGENT_ASKPASS") {
        return PathBuf::from(p);
    }
    if let Ok(exe) = std::env::current_exe() {
        if let Some(dir) = exe.parent() {
            let next_to = dir.join("askpass.sh");
            if next_to.is_file() {
                return next_to;
            }
        }
    }
    PathBuf::from("bin/askpass.sh")
}

/// Replace the dummy host `vm` with the guest address. `user@vm` or `user@vm:path`.
fn rewrite_vm(tok: &str, ip: &str) -> String {
    let Some((user, rest)) = tok.split_once('@') else {
        return tok.to_string();
    };
    if user.is_empty() || user.contains(':') {
        return tok.to_string();
    }
    let host = rest.split(':').next().unwrap_or("");
    if host != "vm" {
        return tok.to_string();
    }
    let suffix = &rest[host.len()..];
    format!("{user}@{ip}{suffix}")
}

fn ssh_cmd(dir: &Path, args: &[String]) -> ! {
    run_ssh_tool("ssh", dir, args);
}

fn scp_cmd(dir: &Path, args: &[String]) -> ! {
    run_ssh_tool("scp", dir, args);
}

/// Display and `XAUTHORITY` from the Xorg that paints the virtio scanout.
///
/// LightDM's line looks like `/usr/lib/xorg/Xorg :0 -auth /var/run/lightdm/root/:0`.
/// That is the XFCE `attach` shows. xrdp's libvnc.so shares the same pixels.
/// A leftover xorgxrdp `:10` is used only if `:0` is not up yet.
fn xrdp_session(ps: &str) -> Option<(String, String)> {
    parse_xorg_line(ps, |line| line.contains("Xorg") && !line.contains("xrdp"))
        .or_else(|| parse_xorg_line(ps, |line| line.contains("Xorg") && line.contains("xrdp")))
}

fn parse_xorg_line(ps: &str, want: impl Fn(&str) -> bool) -> Option<(String, String)> {
    for line in ps.lines() {
        if !want(line) {
            continue;
        }
        let parts: Vec<&str> = line.split_whitespace().collect();
        let display = parts.iter().copied().find(|part| {
            part.strip_prefix(':')
                .is_some_and(|digits| !digits.is_empty() && digits.chars().all(|c| c.is_ascii_digit()))
        });
        let Some(display) = display else {
            continue;
        };
        let Some(auth) = parts
            .windows(2)
            .find(|pair| pair[0] == "-auth")
            .map(|pair| pair[1])
        else {
            continue;
        };
        let auth = if auth.starts_with('/') {
            auth.to_string()
        } else {
            format!("/home/debian/{auth}")
        };
        // LightDM's Xorg cookie is root-only. The autologin session has the
        // same display in debian's Xauthority, which xdotool can read.
        let auth = if display == ":0" && auth.contains("lightdm") {
            "/home/debian/.Xauthority".to_string()
        } else {
            auth
        };
        return Some((display.to_string(), auth));
    }
    None
}

/// Linux KEY_* name to the name `xdotool` wants. Keys go through XTest on
/// the virtio display, the same XFCE xrdp shares.
fn xdotool_key(name: &str) -> Option<&'static str> {
    Some(match name {
        "esc" | "escape" => "Escape",
        "1" => "1",
        "2" => "2",
        "3" => "3",
        "4" => "4",
        "5" => "5",
        "6" => "6",
        "7" => "7",
        "8" => "8",
        "9" => "9",
        "0" => "0",
        "minus" => "minus",
        "equal" => "equal",
        "backspace" => "BackSpace",
        "tab" => "Tab",
        "q" => "q",
        "w" => "w",
        "e" => "e",
        "r" => "r",
        "t" => "t",
        "y" => "y",
        "u" => "u",
        "i" => "i",
        "o" => "o",
        "p" => "p",
        "leftbrace" => "bracketleft",
        "rightbrace" => "bracketright",
        "enter" => "Return",
        "leftctrl" => "ctrl",
        "a" => "a",
        "s" => "s",
        "d" => "d",
        "f" => "f",
        "g" => "g",
        "h" => "h",
        "j" => "j",
        "k" => "k",
        "l" => "l",
        "semicolon" => "semicolon",
        "apostrophe" => "apostrophe",
        "grave" => "grave",
        "leftshift" => "shift",
        "backslash" => "backslash",
        "z" => "z",
        "x" => "x",
        "c" => "c",
        "v" => "v",
        "b" => "b",
        "n" => "n",
        "m" => "m",
        "comma" => "comma",
        "dot" => "period",
        "slash" => "slash",
        "rightshift" => "shift",
        "leftalt" => "alt",
        "space" => "space",
        "capslock" => "Caps_Lock",
        "f1" => "F1",
        "f2" => "F2",
        "f3" => "F3",
        "f4" => "F4",
        "f5" => "F5",
        "f6" => "F6",
        "f7" => "F7",
        "f8" => "F8",
        "f9" => "F9",
        "f10" => "F10",
        "f11" => "F11",
        "f12" => "F12",
        "rightctrl" => "ctrl",
        "rightalt" => "alt",
        "up" => "Up",
        "left" => "Left",
        "right" => "Right",
        "down" => "Down",
        "delete" => "Delete",
        "home" => "Home",
        "end" => "End",
        "pageup" => "Page_Up",
        "pagedown" => "Page_Down",
        "leftmeta" | "rightmeta" => "super",
        _ => return None,
    })
}

/// One input line, as an `xdotool` invocation. Empty for a blank or `#` line.
fn xrdp_input_line(line: &str) -> Result<Option<String>, String> {
    let line = line.trim();
    if line.is_empty() || line.starts_with('#') {
        return Ok(None);
    }
    let mut parts = line.split_whitespace();
    let cmd = parts.next().unwrap_or("");
    let rest: Vec<&str> = parts.collect();
    let tool = match cmd {
        "key" => {
            if rest.len() != 1 && rest.len() != 2 {
                return Err("key <name> [down|up]".into());
            }
            let name = xdotool_key(&rest[0].to_ascii_lowercase())
                .ok_or_else(|| format!("unknown key {}", rest[0]))?;
            let action = rest.get(1).copied().unwrap_or("tap");
            match action {
                "tap" => format!("xdotool key --clearmodifiers {name}"),
                "down" => format!("xdotool keydown {name}"),
                "up" => format!("xdotool keyup {name}"),
                _ => return Err("key action is down, up, or omitted".into()),
            }
        }
        "type" => {
            let text = line.split_once(' ').map(|(_, t)| t).unwrap_or("");
            if text.is_empty() {
                return Err("type <text>".into());
            }
            format!("xdotool type -- {}", sh_quote(text))
        }
        "move" | "at" => {
            if rest.len() != 2 {
                return Err(format!("{cmd} <x> <y>"));
            }
            let x: i32 = rest[0].parse().map_err(|_| format!("{cmd} <x> <y>"))?;
            let y: i32 = rest[1].parse().map_err(|_| format!("{cmd} <x> <y>"))?;
            format!("xdotool mousemove --sync {x} {y}")
        }
        "button" => {
            if rest.is_empty() || rest.len() > 2 {
                return Err("button <left|right|middle> [down|up]".into());
            }
            let button = match rest[0].to_ascii_lowercase().as_str() {
                "left" => "1",
                "middle" => "2",
                "right" => "3",
                _ => return Err("button is left, right, or middle".into()),
            };
            let action = rest.get(1).copied().unwrap_or("tap");
            match action {
                // One `click` so mouseup cannot land on the next page after a navigate.
                "tap" => format!("xdotool click --clearmodifiers {button}"),
                "down" => format!("xdotool mousedown {button}"),
                "up" => format!("xdotool mouseup {button}"),
                _ => return Err("button action is down, up, or omitted".into()),
            }
        }
        "scroll" => {
            if rest.len() != 1 {
                return Err("scroll <n>".into());
            }
            let n: i32 = rest[0].parse().map_err(|_| "scroll <n>".to_string())?;
            if n == 0 {
                return Ok(None);
            }
            // xdotool button 4 is up, 5 is down. Each step is a press and release.
            let button = if n > 0 { "4" } else { "5" };
            format!(
                "i=0; while [ \"$i\" -lt {} ]; do xdotool mousedown {button} && xdotool mouseup {button}; i=$((i+1)); done",
                n.abs()
            )
        }
        "where" | "ping" => return Ok(None),
        other => return Err(format!("unknown command {other}")),
    };
    Ok(Some(tool))
}

/// Shell that runs `lines` on the guest display.
fn xrdp_input_script(display: &str, auth: &str, lines: &[String]) -> Result<String, String> {
    let mut body = String::new();
    for line in lines {
        if let Some(tool) = xrdp_input_line(line)? {
            body.push_str(&tool);
            body.push('\n');
        }
    }
    Ok(format!(
        "runuser -u debian -- env DISPLAY={} XAUTHORITY={} sh -c {}\n",
        sh_quote(display),
        sh_quote(auth),
        sh_quote(&body)
    ))
}

/// Shell that writes a PNG of the guest X session to `remote`.
///
/// `import` writes the real pixels. The file has to live in the debian home
/// directory: `runuser` cannot create one in root's `/tmp`.
fn xrdp_capture(display: &str, auth: &str, remote: &str) -> String {
    format!(
        "runuser -u debian -- env DISPLAY={} XAUTHORITY={} import -window root png24:{}\n",
        sh_quote(display),
        sh_quote(auth),
        sh_quote(remote)
    )
}

/// True when `text` is `ss` or `netstat` output with `port` in LISTEN state.
#[cfg_attr(not(test), allow(dead_code))]
fn listens_on(text: &str, port: u16) -> bool {
    let needle = format!(":{port}");
    let star = format!("*.{port}");
    text.lines().any(|line| {
        let lower = line.to_ascii_lowercase();
        lower.contains("listen") && (line.contains(&needle) || line.contains(&star))
    })
}

fn local_port_open(port: u16) -> bool {
    std::net::TcpStream::connect(("127.0.0.1", port)).is_ok()
}

/// Forward a free local port to the guest's 3389. Returns the local port.
fn start_rdp_tunnel(dir: &Path, local: u16) -> u16 {
    let ip = guest_ip(&mac_for_dir(dir));
    let mut port = local;
    if local_port_open(port) {
        port = (local + 1..local + 20)
            .find(|p| !local_port_open(*p))
            .unwrap_or_else(|| die("no free local port for xrdp"));
    }
    let spec = format!("{port}:127.0.0.1:3389");
    let mut cmd = ssh_cmd_with_password("ssh");
    cmd.args(ssh_base())
        .args(["-f", "-N", "-o", "ExitOnForwardFailure=yes", "-L", &spec])
        .arg(format!("debian@{ip}"));
    let status = cmd
        .status()
        .unwrap_or_else(|e| die(&format!("cannot run ssh: {e}")));
    if !status.success() {
        die("ssh tunnel to xrdp failed");
    }
    for _ in 0..50 {
        if local_port_open(port) {
            return port;
        }
        std::thread::sleep(std::time::Duration::from_millis(100));
    }
    die("ssh tunnel did not start listening");
}

fn which(name: &str) -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path)
        .map(|dir| dir.join(name))
        .find(|p| p.is_file())
}

fn write_rdp_file(port: u16) -> PathBuf {
    let path = std::env::temp_dir().join(format!("vmagent-{port}.rdp"));
    // enablecredsspsupport=0: xrdp has no NLA. Windows App otherwise fails
    // the login with a protocol error before the session starts.
    let body = format!(
        "\
full address:s:127.0.0.1:{port}
username:s:debian
prompt for credentials:i:1
desktopwidth:i:1280
desktopheight:i:800
screen mode id:i:1
authentication level:i:0
enablecredsspsupport:i:0
"
    );
    fs::write(&path, body).unwrap_or_else(|e| die(&format!("cannot write rdp file: {e}")));
    path
}

/// Shell that starts `command` on the xrdp display, in the background.
///
/// The command runs on the session that is already up, and it is also written
/// into `~/.xsession` so the next connection starts it. That file is restored
/// once the command is running, so a later reconnect does not start it again.
fn xrdp_open_script(display: &str, auth: &str, command: &str) -> String {
    let command = command.trim();
    format!(
        "set -e\n\
         home=/home/debian\n\
         sess=$home/.xsession\n\
         if [ -f \"$sess\" ]; then cp -a \"$sess\" \"$sess.vmagent\"; fi\n\
         printf '%s\\n' '#!/bin/sh' \
           'if [ -f \"$HOME/.xsession.vmagent\" ]; then mv -f \"$HOME/.xsession.vmagent\" \"$HOME/.xsession\"; fi' \
           {cmd} 'exec startxfce4' > \"$sess\"\n\
         chmod 755 \"$sess\"\n\
         chown debian:debian \"$sess\"\n\
         runuser -u debian -- env DISPLAY={display} XAUTHORITY={auth} sh -c {run}\n",
        cmd = sh_quote(&format!("({command}) &")),
        run = sh_quote(&format!("({command}) &")),
        display = sh_quote(display),
        auth = sh_quote(auth),
    )
}

/// Open an RDP client against the tunneled guest.
///
/// `open` is a command to start on the xrdp desktop. Empty means the session
/// starts as it is, with nothing extra launched.
fn xrdp_cmd(dir: &Path, port: u16, tunnel_only: bool, open: &[String]) -> ! {
    if !open.is_empty() {
        let command = open.join(" ");
        let Some((display, auth)) = xrdp_session_of(dir) else {
            die("no desktop session yet");
        };
        let status = ssh_status(dir, true, &xrdp_open_script(&display, &auth, &command));
        if status != 0 {
            die("cannot start the command on the desktop");
        }
        eprintln!("xrdp: started: {command}");
    }
    let port = start_rdp_tunnel(dir, port);
    eprintln!("xrdp: 127.0.0.1:{port}  login debian / debian");
    if tunnel_only {
        std::process::exit(0);
    }
    if let Some(bin) = which("xfreerdp").or_else(|| which("xfreerdp3")) {
        let status = Command::new(&bin)
            .args([
                &format!("/v:127.0.0.1:{port}"),
                "/u:debian",
                "/p:debian",
                "/cert:ignore",
                "/size:1280x800",
                "/gfx:off",
                "+clipboard",
            ])
            .status()
            .unwrap_or_else(|e| die(&format!("cannot run {}: {e}", bin.display())));
        std::process::exit(status.code().unwrap_or(1));
    }
    let rdp = write_rdp_file(port);
    let app = Path::new("/Applications/Windows App.app");
    if app.is_dir() {
        let status = Command::new("open")
            .arg("-a")
            .arg(app)
            .arg(&rdp)
            .status()
            .unwrap_or_else(|e| die(&format!("cannot open Windows App: {e}")));
        if !status.success() {
            die("Windows App did not open");
        }
        eprintln!("password: debian");
        eprintln!("if it still errors, install xfreerdp: brew install freerdp");
        std::process::exit(0);
    }
    die("no RDP client found (xfreerdp or Windows App)");
}

fn run_ssh_tool(tool: &str, dir: &Path, args: &[String]) -> ! {
    let ip = guest_ip(&mac_for_dir(dir));
    let args: Vec<String> = args.iter().map(|a| rewrite_vm(a, &ip)).collect();
    let status = ssh_cmd_with_password(tool)
        .args(ssh_base())
        .args(&args)
        .status()
        .unwrap_or_else(|e| die(&format!("cannot run {tool}: {e}")));
    std::process::exit(status.code().unwrap_or(1));
}

/// Shell-quote one argument for the remote `sh -c`.
fn sh_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', "'\\''"))
}

fn guest_target(dir: &Path) -> String {
    format!("debian@{}", guest_ip(&mac_for_dir(dir)))
}

/// `sh -c` as `debian`. `--sudo` prefixes `sudo`.
fn remote_shell(sudo: bool, command: &str) -> String {
    let inner = format!("sh -c {}", sh_quote(command));
    if sudo {
        format!("sudo {inner}")
    } else {
        inner
    }
}

fn run_cmd(dir: &Path, sudo: bool, command: &[String]) -> ! {
    if command.is_empty() {
        die("run needs a command");
    }
    ssh_run(dir, sudo, &command.join(" "));
}

fn ssh_run(dir: &Path, sudo: bool, command: &str) -> ! {
    let status = ssh_cmd_with_password("ssh")
        .args(ssh_base())
        .arg(guest_target(dir))
        .arg(remote_shell(sudo, command))
        .status()
        .unwrap_or_else(|e| die(&format!("cannot run ssh: {e}")));
    std::process::exit(status.code().unwrap_or(1));
}

/// Print a slice of a guest file. The file is not copied to the host.
fn read_cmd(dir: &Path, sudo: bool, path: &str, offset: usize, limit: usize) -> ! {
    if offset == 0 {
        die("--offset starts at 1");
    }
    let end = offset + limit - 1;
    ssh_run(dir, sudo, &format!("sed -n '{offset},{end}p' {}", sh_quote(path)));
}

/// Create or overwrite a guest file. Parent directories are created.
fn write_cmd(dir: &Path, sudo: bool, path: &str, file: Option<&Path>) -> ! {
    let mut bytes = Vec::new();
    match file {
        Some(p) => File::open(p)
            .and_then(|mut f| f.read_to_end(&mut bytes))
            .unwrap_or_else(|e| die(&format!("cannot read {}: {e}", p.display()))),
        None => std::io::stdin()
            .read_to_end(&mut bytes)
            .unwrap_or_else(|e| die(&format!("cannot read stdin: {e}"))),
    };
    let parent = match path.rfind('/') {
        Some(0) | None => None,
        Some(i) => Some(&path[..i]),
    };
    let script = match parent {
        Some(p) => format!("mkdir -p {} && cat > {}", sh_quote(p), sh_quote(path)),
        None => format!("cat > {}", sh_quote(path)),
    };
    let mut child = ssh_cmd_with_password("ssh")
        .args(ssh_base())
        .arg(guest_target(dir))
        .arg(remote_shell(sudo, &script))
        .stdin(Stdio::piped())
        .spawn()
        .unwrap_or_else(|e| die(&format!("cannot run ssh: {e}")));
    child
        .stdin
        .take()
        .unwrap()
        .write_all(&bytes)
        .unwrap_or_else(|e| die(&format!("cannot send file: {e}")));
    let status = child.wait().unwrap_or_else(|e| die(&format!("ssh failed: {e}")));
    std::process::exit(status.code().unwrap_or(1));
}

/// Replace the one occurrence of `--old` with `--new`.
fn edit_cmd(dir: &Path, sudo: bool, path: &str, old: &str, new: &str) -> ! {
    if old.is_empty() {
        die("--old must not be empty");
    }
    let script = format!(
        "python3 -c 'import pathlib,sys; p=pathlib.Path(sys.argv[1]); t=p.read_text(); o=sys.argv[2]; n=t.count(o);\n\
         assert n==1, f\"old matched {{n}} times\"; p.write_text(t.replace(o, sys.argv[3], 1))' {} {} {}",
        sh_quote(path),
        sh_quote(old),
        sh_quote(new)
    );
    ssh_run(dir, sudo, &script);
}

fn ssh_status(dir: &Path, sudo: bool, command: &str) -> i32 {
    ssh_cmd_with_password("ssh")
        .args(ssh_base())
        .arg(guest_target(dir))
        .arg(remote_shell(sudo, command))
        .status()
        .unwrap_or_else(|e| die(&format!("cannot run ssh: {e}")))
        .code()
        .unwrap_or(1)
}

/// Grab the virtio X session and copy the PNG back.
///
/// `import` runs on LightDM's `:0`. That is the XFCE `attach` and xrdp show.
/// A display that is not up yet is an error.
fn screenshot_cmd(dir: &Path, out: Option<&Path>) -> ! {
    let dest = out.unwrap_or(Path::new("screenshot.png"));
    let remote = "/home/debian/vmagent-screenshot.png";
    let Some((display, auth)) = xrdp_session_of(dir) else {
        die("screenshot failed (is the desktop up?)");
    };
    let status = ssh_status(dir, true, &xrdp_capture(&display, &auth, remote));
    if status != 0 {
        die("screenshot failed (is the desktop up?)");
    }
    let scp = ssh_cmd_with_password("scp")
        .args(ssh_base())
        .arg(format!("{}:{remote}", guest_target(dir)))
        .arg(dest)
        .status()
        .unwrap_or_else(|e| die(&format!("cannot run scp: {e}")));
    if !scp.success() {
        die("cannot copy screenshot");
    }
    eprintln!("{}", dest.display());
    std::process::exit(0);
}

fn input_cmd(dir: &Path, commands: &[String]) -> ! {
    let lines = if commands.is_empty() {
        let mut body = String::new();
        std::io::Read::read_to_string(&mut std::io::stdin(), &mut body)
            .unwrap_or_else(|e| die(&format!("cannot read input: {e}")));
        body.lines().map(|s| s.to_string()).collect()
    } else {
        commands.to_vec()
    };
    for line in &lines {
        if let Err(msg) = xrdp_input_line(line) {
            die(&msg);
        }
    }
    if !input_send(dir, &lines) {
        die("input failed (is the desktop up?)");
    }
    std::process::exit(0);
}

/// Pixel size of the guest display. Clicks use the same frame `screenshot` saves.
fn display_size(dir: &Path) -> (i32, i32) {
    display_size_soft(dir).unwrap_or_else(|| die("cannot read the guest display size"))
}

/// Send one batch of input to the guest display.
fn input_send(dir: &Path, lines: &[String]) -> bool {
    let Some((display, auth)) = xrdp_session_of(dir) else {
        return false;
    };
    let Ok(script) = xrdp_input_script(&display, &auth, lines) else {
        return false;
    };
    let out = ssh_cmd_with_password("ssh")
        .args(ssh_base())
        .arg(guest_target(dir))
        .arg(remote_shell(true, &script))
        .output();
    let Ok(out) = out else {
        return false;
    };
    out.status.success()
}

fn input_must(dir: &Path, lines: &[String]) -> Result<(), String> {
    if input_send(dir, lines) {
        Ok(())
    } else {
        Err("input failed".into())
    }
}

fn guest_output(dir: &Path, sudo: bool, command: &str) -> Option<String> {
    let out = ssh_cmd_with_password("ssh")
        .args(ssh_base())
        .arg(guest_target(dir))
        .arg(remote_shell(sudo, command))
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    Some(String::from_utf8_lossy(&out.stdout).to_string())
}

/// `ps -ef` of the guest, looking for LightDM's Xorg (or xorgxrdp as fallback).
fn xrdp_session_of(dir: &Path) -> Option<(String, String)> {
    let text = guest_output(dir, true, "ps -ef")?;
    xrdp_session(&text)
}

fn pause(ms: u64) {
    std::thread::sleep(std::time::Duration::from_millis(ms));
}

fn click_cmd(dir: &Path, x: i32, y: i32, button: &str) -> ! {
    let (w, h) = display_size(dir);
    if !(0..w).contains(&x) || !(0..h).contains(&y) {
        die(&format!("pointer is {w}x{h}"));
    }
    if !matches!(button, "left" | "right" | "middle") {
        die("button is left, right, or middle");
    }
    let _ = input_must(dir, &[format!("move {x} {y}"), format!("button {button}")]);
    std::process::exit(0);
}

fn key_cmd(dir: &Path, name: &str) -> ! {
    let parts: Vec<&str> = name.split('+').filter(|s| !s.is_empty()).collect();
    if parts.is_empty() || parts.iter().any(|s| s.contains(char::is_whitespace)) {
        die("key needs one name, or names joined with +");
    }
    if parts.len() == 1 {
        let _ = input_must(dir, &[format!("key {}", parts[0])]);
        std::process::exit(0);
    }
    // One call, so a modifier is still down when the key it holds is sent.
    let mut lines = Vec::new();
    for part in &parts[..parts.len() - 1] {
        lines.push(format!("key {part} down"));
    }
    lines.push(format!("key {}", parts[parts.len() - 1]));
    for part in parts[..parts.len() - 1].iter().rev() {
        lines.push(format!("key {part} up"));
    }
    let _ = input_must(dir, &lines);
    std::process::exit(0);
}

fn type_cmd(dir: &Path, text: &str) -> ! {
    let _ = input_must(dir, &[format!("type {text}")]);
    std::process::exit(0);
}

/// Pull vmlinuz, initrd, and the grub root= line out of the disk.
fn split_image(disk: &Path, dir: &Path, append: &str) -> String {
    let script = find_script();
    let out = Command::new("python3")
        .arg(&script)
        .arg(disk)
        .arg(dir)
        .arg("--append")
        .arg(append)
        .output()
        .unwrap_or_else(|e| die(&format!("cannot run {}: {e}", script.display())));
    if !out.status.success() {
        eprint!("{}", String::from_utf8_lossy(&out.stderr));
        die("failed to split kernel and initrd out of the image");
    }
    let line = String::from_utf8_lossy(&out.stdout);
    let line = line.trim();
    if line.is_empty() {
        die("split produced an empty command line");
    }
    line.to_string()
}

fn find_script() -> PathBuf {
    if let Some(p) = std::env::var_os("SPLIT_IMAGE") {
        return PathBuf::from(p);
    }
    if let Ok(exe) = std::env::current_exe() {
        if let Some(dir) = exe.parent() {
            let next_to = dir.join("split-image.py");
            if next_to.is_file() {
                return next_to;
            }
        }
    }
    PathBuf::from("scripts/split-image.py")
}

/// Cloud-init config: an 8 MiB FAT image labeled `cidata` with user-data and meta-data.
/// Built with hdiutil so this stays a Mac tool and does not need mtools.
fn write_cidata(dir: &Path, user_data: Option<&Path>, meta_data: Option<&Path>) -> PathBuf {
    let user = match user_data {
        Some(p) => fs::read(p).unwrap_or_else(|e| die(&format!("cannot read {}: {e}", p.display()))),
        None => b"#cloud-config\n".to_vec(),
    };
    if !user.starts_with(b"#cloud-config") {
        die("user-data must start with #cloud-config");
    }
    let meta = match meta_data {
        Some(p) => fs::read(p).unwrap_or_else(|e| die(&format!("cannot read {}: {e}", p.display()))),
        None => b"instance-id: vmagent-1\nlocal-hostname: debian\n".to_vec(),
    };

    let staging = dir.join("cidata-src");
    if let Err(e) = fs::create_dir_all(&staging) {
        die(&format!("cannot create {}: {e}", staging.display()));
    }
    write_file(&staging.join("user-data"), &user);
    write_file(&staging.join("meta-data"), &meta);

    let img = dir.join("cidata.raw");
    let _ = fs::remove_file(&img);
    let status = Command::new("hdiutil")
        .args([
            "create",
            "-size",
            "8m",
            "-fs",
            "MS-DOS",
            "-volname",
            "cidata",
            "-format",
            "UDRW",
            "-srcfolder",
            &staging.display().to_string(),
            "-ov",
        ])
        .arg(&img)
        .status();
    match status {
        Ok(s) if s.success() => {}
        Ok(s) => die(&format!("hdiutil exited {}", s.code().unwrap_or(1))),
        Err(e) => die(&format!("hdiutil failed: {e}")),
    }
    if !img.is_file() {
        let dmg = dir.join("cidata.raw.dmg");
        if dmg.is_file() {
            if let Err(e) = fs::rename(&dmg, &img) {
                die(&format!("cannot rename {}: {e}", dmg.display()));
            }
        } else {
            die("hdiutil did not write cidata.raw");
        }
    }
    img
}

fn write_file(path: &Path, bytes: &[u8]) {
    let mut f = File::create(path).unwrap_or_else(|e| die(&format!("cannot write {}: {e}", path.display())));
    if let Err(e) = f.write_all(bytes) {
        die(&format!("cannot write {}: {e}", path.display()));
    }
}

/// Grow the working disk. cloud-initramfs-growroot expands the partition on boot.
fn grow_disk(disk: &Path, gb: u64) {
    if gb == 0 {
        return;
    }
    let want = gb.saturating_mul(1024 * 1024 * 1024);
    let len = fs::metadata(disk)
        .unwrap_or_else(|e| die(&format!("cannot stat {}: {e}", disk.display())))
        .len();
    if len >= want {
        return;
    }
    eprintln!("growing {} to {gb}G", disk.display());
    let f = fs::OpenOptions::new()
        .write(true)
        .open(disk)
        .unwrap_or_else(|e| die(&format!("cannot grow {}: {e}", disk.display())));
    if let Err(e) = f.set_len(want) {
        die(&format!("cannot grow {}: {e}", disk.display()));
    }
}

fn tmp_dir() -> PathBuf {
    let n = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    PathBuf::from(format!("/tmp/vmagent-{n}"))
}

/// vmcore sits next to this binary, or at VMCORE=/path, or in ./vmcore after a local build.
fn find_vmcore() -> PathBuf {
    if let Some(p) = std::env::var_os("VMCORE") {
        return PathBuf::from(p);
    }
    if let Ok(exe) = std::env::current_exe() {
        if let Some(dir) = exe.parent() {
            let next_to = dir.join("vmcore");
            if next_to.is_file() {
                return next_to;
            }
        }
    }
    PathBuf::from("bin/vmcore")
}

fn pid_file(dir: &Path) -> PathBuf {
    dir.join("vm.pid")
}

fn list_cmd() -> ! {
    let out = Command::new("ps")
        .args(["-axww", "-o", "pid=,command="])
        .output()
        .unwrap_or_else(|e| die(&format!("ps failed: {e}")));
    if !out.status.success() {
        die("ps failed");
    }
    let text = String::from_utf8_lossy(&out.stdout);
    let mut seen = std::collections::BTreeSet::new();
    for line in text.lines() {
        let Some(dir) = vm_dir_from_ps(line) else {
            continue;
        };
        // One process, one line. Canonicalize only to drop a second spelling.
        let key = fs::canonicalize(&dir).unwrap_or_else(|_| dir.clone());
        if seen.insert(key) {
            println!("{}", dir.display());
        }
    }
    std::process::exit(0);
}

/// `ps` line: pid, vmcore path, disk.img, then the rest of the boot args.
fn vm_dir_from_ps(line: &str) -> Option<PathBuf> {
    let mut parts = line.split_whitespace();
    let _pid = parts.next()?;
    let bin = parts.next()?;
    if Path::new(bin).file_name()?.to_str()? != "vmcore" {
        return None;
    }
    let disk = parts.next()?;
    if !disk.ends_with("disk.img") {
        return None;
    }
    Path::new(disk).parent().map(|p| p.to_path_buf())
}

fn vm_pid(dir: &Path) -> u32 {
    let text = fs::read_to_string(pid_file(dir)).unwrap_or_else(|_| die("vm is not running"));
    text.trim().parse().unwrap_or_else(|_| die("bad vm.pid"))
}

fn live_pid(dir: &Path) -> Option<u32> {
    let pid: u32 = fs::read_to_string(pid_file(dir)).ok()?.trim().parse().ok()?;
    let alive = Command::new("kill")
        .args(["-0", &pid.to_string()])
        .status()
        .map(|s| s.success())
        .unwrap_or(false);
    if alive { Some(pid) } else { None }
}

fn vm_alive(dir: &Path) -> bool {
    live_pid(dir).is_some()
}

fn signal_vm(dir: &Path, sig: &str) -> ! {
    let pid = vm_pid(dir);
    if !vm_alive(dir) {
        die("vm is not running");
    }
    let status = Command::new("kill")
        .args([sig, &pid.to_string()])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .unwrap_or_else(|e| die(&format!("cannot signal vm: {e}")));
    // SIGTERM can reap the process before kill runs. That is a stop, not an error.
    if sig == "-TERM" && !vm_alive(dir) {
        std::process::exit(0);
    }
    std::process::exit(status.code().unwrap_or(1));
}

fn die(msg: &str) -> ! {
    eprintln!("vmagent: {msg}");
    std::process::exit(1);
}

const FIREFOX_PROFILE: &str = "/home/debian/.cache/vmagent/firefox";
const UBLOCK_ID: &str = "uBlock0@raymondhill.net";
const UBLOCK_REMOTE: &str = "/tmp/vmagent-ublock.xpi";

/// Prefs for a clean guest profile.
///
/// Remote debugging has to be on, the welcome page has to stay off, and
/// hardware video has to stay off so a screenshot of YouTube is a real frame.
fn firefox_user_js() -> String {
    let prefs = [
        ("devtools.debugger.remote-enabled", "true"),
        ("devtools.debugger.prompt-connection", "false"),
        ("marionette.enabled", "true"),
        ("extensions.autoDisableScopes", "0"),
        ("extensions.enabledScopes", "15"),
        ("extensions.sideloadScopes", "15"),
        ("extensions.installDistroAddons", "true"),
        ("xpinstall.signatures.required", "false"),
        ("browser.startup.homepage", "\"about:blank\""),
        ("browser.startup.page", "0"),
        ("browser.sessionstore.enabled", "false"),
        ("browser.sessionstore.resume_from_crash", "false"),
        ("browser.sessionstore.max_resumed_crashes", "0"),
        ("browser.aboutwelcome.enabled", "false"),
        ("startup.homepage_welcome_url", "\"\""),
        ("startup.homepage_welcome_url.additional", "\"\""),
        ("browser.shell.checkDefaultBrowser", "false"),
        ("browser.tabs.warnOnClose", "false"),
        ("places.history.enabled", "false"),
        ("browser.urlbar.suggest.history", "false"),
        ("signon.rememberSignons", "false"),
        ("dom.disable_open_during_load", "true"),
        ("media.hardware-video-decoding.enabled", "false"),
        ("media.ffmpeg.vaapi.enabled", "false"),
        ("media.ffmpeg.enabled", "true"),
        ("media.ffvpx.enabled", "true"),
        ("media.av1.enabled", "false"),
        ("media.autoplay.default", "0"),
        ("gfx.webrender.all", "false"),
        ("gfx.webrender.force-disabled", "true"),
        ("layers.acceleration.disabled", "true"),
    ];
    let mut js = String::from("// vmagent firefox profile\n");
    for (name, value) in prefs {
        js.push_str(&format!("user_pref(\"{name}\", {value});\n"));
    }
    js
}

/// Shell that stops the Firefox this command started and launches a clean one.
///
/// Visible on the virtio display, not `--headless`. The desktop is the viewer.
fn firefox_launch_script(display: &str, auth: &str, port: u16, urls: &[String]) -> String {
    let urls = if urls.is_empty() {
        "about:blank".to_string()
    } else {
        urls.iter().map(|u| sh_quote(u)).collect::<Vec<_>>().join(" ")
    };
    format!(
        "set -e\n\
         profile={profile}\n\
         if [ -f \"$profile/vmagent.pid\" ]; then kill \"$(cat \"$profile/vmagent.pid\")\" 2>/dev/null || true; fi\n\
         pkill -u debian -f 'remote-debugging-port={port}' 2>/dev/null || true\n\
         pkill -u debian -f '[p]ython3 /tmp/vmagent-firefox-bidi.py' 2>/dev/null || true\n\
         rm -f /tmp/vmagent-firefox.log /tmp/vmagent-bidi.log\n\
         sleep 0.4\n\
         rm -rf \"$profile\"\n\
         mkdir -p \"$profile/extensions\"\n\
         cat > \"$profile/user.js\" <<'EOF'\n\
         {prefs}\
         EOF\n\
         cp {ublock} \"$profile/extensions/{ublock_id}.xpi\"\n\
         chown -R debian:debian \"$(dirname \"$profile\")\"\n\
         runuser -u debian -- env DISPLAY={display} XAUTHORITY={auth} MOZ_WEBRENDER=0 MOZ_ACCELERATED=0 LIBGL_ALWAYS_SOFTWARE=1 \
           firefox-esr --no-remote --profile \"$profile\" --remote-debugging-port={port} --width 1280 --height 720 {urls} \
           >\"$profile/firefox.log\" 2>&1 &\n\
         echo $! > \"$profile/vmagent.pid\"\n\
         chown debian:debian \"$profile/vmagent.pid\"\n\
         i=0\n\
         while [ \"$i\" -lt 30 ]; do\n\
           if curl -sf http://127.0.0.1:{port}/ | grep -q httpd.js; then break; fi\n\
           i=$((i + 1))\n\
           sleep 0.4\n\
         done\n\
         if ! curl -sf http://127.0.0.1:{port}/ | grep -q httpd.js; then\n\
           echo 'firefox did not open the debug port' >&2\n\
           tail -n 40 \"$profile/firefox.log\" >&2 || true\n\
           exit 1\n\
         fi\n\
         pkill -u debian -f '[p]ython3 /tmp/vmagent-firefox-bidi.py' 2>/dev/null || true\n\
         rm -f /tmp/vmagent-bidi.log\n\
         runuser -u debian -- env BIDI_PORT={port} BIDI_CMD_PORT={cmdport} \
           nohup python3 /tmp/vmagent-firefox-bidi.py >/tmp/vmagent-bidi.log 2>&1 &\n\
         i=0\n\
         while [ \"$i\" -lt 80 ]; do\n\
           if grep -q bidi.ready /tmp/vmagent-bidi.log 2>/dev/null; then exit 0; fi\n\
           i=$((i + 1))\n\
           sleep 0.25\n\
         done\n\
         echo 'bidi helper did not start' >&2\n\
         cat /tmp/vmagent-bidi.log >&2 || true\n\
         exit 1\n",
        profile = sh_quote(FIREFOX_PROFILE),
        prefs = firefox_user_js(),
        ublock = sh_quote(UBLOCK_REMOTE),
        ublock_id = UBLOCK_ID,
        display = sh_quote(display),
        auth = sh_quote(auth),
        port = port,
        cmdport = port.saturating_add(1),
        urls = urls,
    )
}

fn firefox_cmd(dir: &Path, port: u16, action: FirefoxAction) -> ! {
    match action {
        FirefoxAction::Open { urls } => {
            for url in &urls {
                if !(url.starts_with("http://")
                    || url.starts_with("https://")
                    || url.starts_with("about:"))
                {
                    die("firefox open takes http, https, or about: URLs");
                }
            }
            let Some((display, auth)) = xrdp_session_of(dir) else {
                die("no desktop session yet");
            };
            install_firefox_bidi(dir);
            install_ublock(dir);
            let status = ssh_status(
                dir,
                true,
                &firefox_launch_script(&display, &auth, port, &urls),
            );
            if status != 0 {
                die("firefox did not start");
            }
            eprintln!("firefox: 127.0.0.1:{port} in the guest");
            std::process::exit(0);
        }
        FirefoxAction::Tabs => firefox_bidi(dir, port, "tabs", ""),
        FirefoxAction::Goto { url } => firefox_bidi(dir, port, "goto", &url),
        FirefoxAction::Eval { expression } => firefox_bidi(dir, port, "eval", &expression),
        FirefoxAction::Close { id } => {
            firefox_bidi(dir, port, "close", id.as_deref().unwrap_or(""))
        }
    }
}

/// Copy the BiDi helper onto the guest. The debug port is not published by NAT.
fn install_firefox_bidi(dir: &Path) {
    let remote = "/tmp/vmagent-firefox-bidi.py";
    let mut child = ssh_cmd_with_password("ssh")
        .args(ssh_base())
        .arg(guest_target(dir))
        .arg(remote_shell(false, &format!("cat > {}", sh_quote(remote))))
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap_or_else(|e| die(&format!("cannot run ssh: {e}")));
    child
        .stdin
        .take()
        .unwrap()
        .write_all(include_bytes!("firefox_bidi.py"))
        .unwrap_or_else(|e| die(&format!("cannot send firefox client: {e}")));
    let status = child
        .wait()
        .unwrap_or_else(|e| die(&format!("ssh failed: {e}")));
    if !status.success() {
        die("cannot copy the firefox client");
    }
}

/// Copy uBlock Origin onto the guest so `open` can sideload it.
fn install_ublock(dir: &Path) {
    let mut child = ssh_cmd_with_password("ssh")
        .args(ssh_base())
        .arg(guest_target(dir))
        .arg(remote_shell(false, &format!("cat > {}", sh_quote(UBLOCK_REMOTE))))
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap_or_else(|e| die(&format!("cannot run ssh: {e}")));
    child
        .stdin
        .take()
        .unwrap()
        .write_all(include_bytes!("../assets/uBlock0.firefox.xpi"))
        .unwrap_or_else(|e| die(&format!("cannot send uBlock Origin: {e}")));
    let status = child
        .wait()
        .unwrap_or_else(|e| die(&format!("ssh failed: {e}")));
    if !status.success() {
        die("cannot copy uBlock Origin");
    }
}

/// Talk to the helper that already owns the BiDi socket.
///
/// Firefox keeps one session. A second socket cannot join, so commands go
/// over localhost TCP to that helper.
fn firefox_bidi(dir: &Path, port: u16, action: &str, arg: &str) -> ! {
    install_firefox_bidi(dir);
    let cmd_port = port.saturating_add(1);
    let command = format!(
        "BIDI_PORT={port} BIDI_CMD_PORT={cmd_port} BIDI_ACTION={action} BIDI_ARG={arg} python3 /tmp/vmagent-firefox-bidi.py",
        action = sh_quote(action),
        arg = sh_quote(arg),
    );
    ssh_run(dir, false, &command);
}

fn rpc_sock_path(dir: &Path, socket: Option<&Path>) -> PathBuf {
    socket
        .map(|p| p.to_path_buf())
        .unwrap_or_else(|| dir.join("rpc.sock"))
}

fn rpc_pid_path(sock: &Path) -> PathBuf {
    let mut p = sock.as_os_str().to_os_string();
    p.push(".pid");
    PathBuf::from(p)
}

/// Stop a server already bound to this socket so the new one can take it.
fn stop_old_server(sock: &Path) {
    let pid_path = rpc_pid_path(sock);
    let Ok(text) = fs::read_to_string(&pid_path) else {
        let _ = fs::remove_file(sock);
        return;
    };
    if let Ok(pid) = text.trim().parse::<u32>() {
        if pid != std::process::id() {
            let _ = Command::new("kill")
                .args(["-TERM", &pid.to_string()])
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status();
            for _ in 0..50 {
                let alive = Command::new("kill")
                    .args(["-0", &pid.to_string()])
                    .stdout(Stdio::null())
                    .stderr(Stdio::null())
                    .status()
                    .map(|s| s.success())
                    .unwrap_or(false);
                if !alive {
                    break;
                }
                pause(20);
            }
        }
    }
    let _ = fs::remove_file(sock);
    let _ = fs::remove_file(&pid_path);
}

fn serve_cmd(dir: &Path, socket: Option<&Path>) -> ! {
    if !cfg!(target_os = "macos") && !cfg!(target_os = "linux") {
        die("serve needs a unix socket");
    }
    let sock_path = rpc_sock_path(dir, socket);
    if let Some(parent) = sock_path.parent() {
        if !parent.as_os_str().is_empty() {
            fs::create_dir_all(parent).unwrap_or_else(|e| die(&format!("cannot create {}: {e}", parent.display())));
        }
    }
    stop_old_server(&sock_path);
    let listener = std::os::unix::net::UnixListener::bind(&sock_path)
        .unwrap_or_else(|e| die(&format!("cannot bind {}: {e}", sock_path.display())));
    // The agent and a shell user both connect. The VM dir is already private.
    use std::os::unix::fs::PermissionsExt;
    let _ = fs::set_permissions(&sock_path, fs::Permissions::from_mode(0o666));
    fs::write(rpc_pid_path(&sock_path), format!("{}\n", std::process::id()))
        .unwrap_or_else(|e| die(&format!("cannot write pid: {e}")));
    eprintln!("rpc {} pid {}", sock_path.display(), std::process::id());
    for conn in listener.incoming() {
        match conn {
            Ok(stream) => {
                if let Err(e) = serve_client(dir, stream) {
                    eprintln!("rpc client: {e}");
                }
            }
            Err(e) => eprintln!("rpc accept: {e}"),
        }
    }
    std::process::exit(0);
}

fn serve_client(dir: &Path, stream: std::os::unix::net::UnixStream) -> std::io::Result<()> {
    stream.set_read_timeout(Some(std::time::Duration::from_secs(120)))?;
    let mut reader = stream.try_clone()?;
    let mut writer = stream;
    let mut buf = Vec::new();
    let mut tmp = [0u8; 8192];
    loop {
        let n = std::io::Read::read(&mut reader, &mut tmp)?;
        if n == 0 {
            break;
        }
        buf.extend_from_slice(&tmp[..n]);
        while let Some(split) = buf.iter().position(|b| *b == b'\n') {
            let line: Vec<u8> = buf.drain(..=split).collect();
            let line = String::from_utf8_lossy(&line);
            let line = line.trim();
            if line.is_empty() {
                continue;
            }
            let reply = rpc_dispatch(dir, line);
            writer.write_all(reply.as_bytes())?;
            writer.write_all(b"\n")?;
            writer.flush()?;
        }
        if buf.len() > 8 * 1024 * 1024 {
            let reply = rpc_error(None, -32700, "request too large");
            writer.write_all(reply.as_bytes())?;
            writer.write_all(b"\n")?;
            break;
        }
    }
    Ok(())
}

fn rpc_dispatch(dir: &Path, line: &str) -> String {
    let parsed = parse_json(line);
    let (id, method, params) = match parsed {
        Ok(v) => v,
        Err(msg) => return rpc_error(None, -32700, &msg),
    };
    let id = match id {
        Some(id) => id,
        None => return String::new(),
    };
    let result = match method.as_str() {
        "screenshot" => rpc_screenshot(dir, &params),
        "click" => rpc_click(dir, &params),
        "key" => rpc_key(dir, &params),
        "type" => rpc_type(dir, &params),
        "input" => rpc_input(dir, &params),
        "display" => match display_size_soft(dir) {
            Some((w, h)) => Ok(json_obj(&[("width", Json::Int(w as i64)), ("height", Json::Int(h as i64))])),
            None => Err("cannot read the guest display size".into()),
        },
        "ping" => Ok(json_obj(&[("ok", Json::Bool(true))])),
        other => return rpc_error(Some(&id), -32601, &format!("unknown method {other}")),
    };
    match result {
        Ok(value) => rpc_ok(&id, &value),
        Err(msg) => rpc_error(Some(&id), -32000, &msg),
    }
}

fn rpc_screenshot(dir: &Path, params: &Json) -> Result<Json, String> {
    let out = params
        .get("out")
        .and_then(|v| v.as_str())
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("screenshot.png"));
    let remote = "/home/debian/vmagent-screenshot.png";
    let Some((display, auth)) = xrdp_session_of(dir) else {
        return Err("screenshot failed (is the desktop up?)".into());
    };
    let status = ssh_status(dir, true, &xrdp_capture(&display, &auth, remote));
    if status != 0 {
        return Err("screenshot failed (is the desktop up?)".into());
    }
    let scp = ssh_cmd_with_password("scp")
        .args(ssh_base())
        .arg(format!("{}:{remote}", guest_target(dir)))
        .arg(&out)
        .status()
        .map_err(|e| format!("cannot run scp: {e}"))?;
    if !scp.success() {
        return Err("cannot copy screenshot".into());
    }
    Ok(json_obj(&[("path", Json::Str(out.display().to_string()))]))
}

fn rpc_click(dir: &Path, params: &Json) -> Result<Json, String> {
    let x = params.get("x").and_then(|v| v.as_i32()).ok_or("click needs x")?;
    let y = params.get("y").and_then(|v| v.as_i32()).ok_or("click needs y")?;
    let button = params.get("button").and_then(|v| v.as_str()).unwrap_or("left");
    if !matches!(button, "left" | "right" | "middle") {
        return Err("button is left, right, or middle".into());
    }
    let (w, h) = display_size_soft(dir).ok_or("cannot read the guest display size")?;
    if !(0..w).contains(&x) || !(0..h).contains(&y) {
        return Err(format!("pointer is {w}x{h}"));
    }
    input_must(dir, &[format!("move {x} {y}"), format!("button {button}")])?;
    Ok(json_obj(&[
        ("x", Json::Int(x as i64)),
        ("y", Json::Int(y as i64)),
        ("button", Json::Str(button.to_string())),
    ]))
}

fn rpc_key(dir: &Path, params: &Json) -> Result<Json, String> {
    let name = params.get("name").and_then(|v| v.as_str()).ok_or("key needs name")?;
    let parts: Vec<&str> = name.split('+').filter(|s| !s.is_empty()).collect();
    if parts.is_empty() || parts.iter().any(|s| s.contains(char::is_whitespace)) {
        return Err("key needs one name, or names joined with +".into());
    }
    if parts.len() == 1 {
        input_must(dir, &[format!("key {}", parts[0])])?;
    } else {
        let mut lines = Vec::new();
        for part in &parts[..parts.len() - 1] {
            lines.push(format!("key {part} down"));
        }
        lines.push(format!("key {}", parts[parts.len() - 1]));
        for part in parts[..parts.len() - 1].iter().rev() {
            lines.push(format!("key {part} up"));
        }
        input_must(dir, &lines)?;
    }
    Ok(json_obj(&[("name", Json::Str(name.to_string()))]))
}

fn rpc_type(dir: &Path, params: &Json) -> Result<Json, String> {
    let text = params.get("text").and_then(|v| v.as_str()).ok_or("type needs text")?;
    if text.contains('\n') || text.contains('\r') {
        return Err("type is one line; send key enter separately".into());
    }
    input_must(dir, &[format!("type {text}")])?;
    Ok(json_obj(&[("text", Json::Str(text.to_string()))]))
}

fn rpc_input(dir: &Path, params: &Json) -> Result<Json, String> {
    let lines = params
        .get("lines")
        .and_then(|v| v.as_array())
        .ok_or("input needs lines")?;
    let mut commands = Vec::new();
    for line in lines {
        let s = line.as_str().ok_or("input lines are strings")?;
        if s.contains('\n') || s.contains('\r') {
            return Err("each input line is one command".into());
        }
        commands.push(s.to_string());
    }
    if !input_send(dir, &commands) {
        return Err("input failed".into());
    }
    Ok(json_obj(&[("ok", Json::Bool(true))]))
}

fn display_size_soft(dir: &Path) -> Option<(i32, i32)> {
    let (display, auth) = xrdp_session_of(dir)?;
    let script = format!(
        "runuser -u debian -- env DISPLAY={} XAUTHORITY={} xdotool getdisplaygeometry",
        sh_quote(&display),
        sh_quote(&auth)
    );
    let out = guest_output(dir, true, &script)?;
    let mut parts = out.split_whitespace();
    let w = parts.next()?.parse::<i32>().ok()?;
    let h = parts.next()?.parse::<i32>().ok()?;
    if w > 1 && h > 1 {
        Some((w, h))
    } else {
        None
    }
}

fn rpc_ok(id: &Json, result: &Json) -> String {
    format!(
        "{{\"jsonrpc\":\"2.0\",\"id\":{},\"result\":{}}}",
        id.encode(),
        result.encode()
    )
}

fn rpc_error(id: Option<&Json>, code: i64, message: &str) -> String {
    let id = id.map(|v| v.encode()).unwrap_or_else(|| "null".to_string());
    format!(
        "{{\"jsonrpc\":\"2.0\",\"id\":{id},\"error\":{{\"code\":{code},\"message\":{}}}}}",
        json_escape(message)
    )
}

#[derive(Clone, Debug)]
enum Json {
    Null,
    Bool(bool),
    Int(i64),
    Str(String),
    Arr(Vec<Json>),
    Obj(Vec<(String, Json)>),
}

impl Json {
    fn encode(&self) -> String {
        match self {
            Json::Null => "null".into(),
            Json::Bool(true) => "true".into(),
            Json::Bool(false) => "false".into(),
            Json::Int(n) => n.to_string(),
            Json::Str(s) => json_escape(s),
            Json::Arr(items) => {
                let mut out = String::from("[");
                for (i, item) in items.iter().enumerate() {
                    if i > 0 {
                        out.push(',');
                    }
                    out.push_str(&item.encode());
                }
                out.push(']');
                out
            }
            Json::Obj(items) => {
                let mut out = String::from("{");
                for (i, (k, v)) in items.iter().enumerate() {
                    if i > 0 {
                        out.push(',');
                    }
                    out.push_str(&json_escape(k));
                    out.push(':');
                    out.push_str(&v.encode());
                }
                out.push('}');
                out
            }
        }
    }

    fn as_str(&self) -> Option<&str> {
        match self {
            Json::Str(s) => Some(s),
            _ => None,
        }
    }

    fn as_i32(&self) -> Option<i32> {
        match self {
            Json::Int(n) => i32::try_from(*n).ok(),
            _ => None,
        }
    }

    fn as_array(&self) -> Option<&[Json]> {
        match self {
            Json::Arr(items) => Some(items),
            _ => None,
        }
    }

    fn get(&self, key: &str) -> Option<&Json> {
        match self {
            Json::Obj(items) => items.iter().find(|(k, _)| k == key).map(|(_, v)| v),
            _ => None,
        }
    }
}

fn json_obj(items: &[(&str, Json)]) -> Json {
    Json::Obj(items.iter().map(|(k, v)| ((*k).to_string(), v.clone())).collect())
}

fn json_escape(s: &str) -> String {
    let mut out = String::from("\"");
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if c.is_control() => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

struct JsonParser<'a> {
    s: &'a [u8],
    i: usize,
}

impl<'a> JsonParser<'a> {
    fn skip(&mut self) {
        while self.i < self.s.len() && self.s[self.i].is_ascii_whitespace() {
            self.i += 1;
        }
    }

    fn peek(&mut self) -> Option<u8> {
        self.skip();
        self.s.get(self.i).copied()
    }

    fn bump(&mut self) -> Option<u8> {
        let c = self.peek()?;
        self.i += 1;
        Some(c)
    }

    fn value(&mut self) -> Result<Json, String> {
        match self.peek() {
            Some(b'n') => self.lit(b"null", Json::Null),
            Some(b't') => self.lit(b"true", Json::Bool(true)),
            Some(b'f') => self.lit(b"false", Json::Bool(false)),
            Some(b'"') => Ok(Json::Str(self.string()?)),
            Some(b'[') => self.array(),
            Some(b'{') => self.object(),
            Some(b'-') | Some(b'0'..=b'9') => self.number(),
            _ => Err("expected a json value".into()),
        }
    }

    fn lit(&mut self, lit: &[u8], value: Json) -> Result<Json, String> {
        if self.s[self.i..].starts_with(lit) {
            self.i += lit.len();
            Ok(value)
        } else {
            Err("bad json literal".into())
        }
    }

    fn number(&mut self) -> Result<Json, String> {
        let start = self.i;
        if self.peek() == Some(b'-') {
            self.i += 1;
        }
        while matches!(self.s.get(self.i), Some(b'0'..=b'9')) {
            self.i += 1;
        }
        if matches!(self.s.get(self.i), Some(b'.' | b'e' | b'E')) {
            return Err("only integers are accepted".into());
        }
        let text = std::str::from_utf8(&self.s[start..self.i]).unwrap_or("");
        text.parse::<i64>().map(Json::Int).map_err(|_| "bad number".into())
    }

    fn string(&mut self) -> Result<String, String> {
        if self.bump() != Some(b'"') {
            return Err("expected a string".into());
        }
        let mut out = String::new();
        loop {
            let c = self.s.get(self.i).copied().ok_or("unterminated string")?;
            self.i += 1;
            match c {
                b'"' => return Ok(out),
                b'\\' => {
                    let e = self.s.get(self.i).copied().ok_or("bad escape")?;
                    self.i += 1;
                    match e {
                        b'"' => out.push('"'),
                        b'\\' => out.push('\\'),
                        b'/' => out.push('/'),
                        b'n' => out.push('\n'),
                        b'r' => out.push('\r'),
                        b't' => out.push('\t'),
                        b'u' => {
                            if self.i + 4 > self.s.len() {
                                return Err("bad unicode escape".into());
                            }
                            let hex = std::str::from_utf8(&self.s[self.i..self.i + 4]).unwrap_or("");
                            let cp = u32::from_str_radix(hex, 16).map_err(|_| "bad unicode escape")?;
                            out.push(char::from_u32(cp).ok_or("bad unicode escape")?);
                            self.i += 4;
                        }
                        _ => return Err("bad escape".into()),
                    }
                }
                c => {
                    let width = utf8_width(c);
                    if self.i + width - 1 > self.s.len() {
                        return Err("bad utf-8".into());
                    }
                    let text = std::str::from_utf8(&self.s[self.i - 1..self.i - 1 + width])
                        .map_err(|_| "bad utf-8")?;
                    out.push_str(text);
                    self.i += width - 1;
                }
            }
        }
    }

    fn array(&mut self) -> Result<Json, String> {
        self.bump();
        let mut items = Vec::new();
        if self.peek() == Some(b']') {
            self.i += 1;
            return Ok(Json::Arr(items));
        }
        loop {
            items.push(self.value()?);
            match self.bump() {
                Some(b']') => return Ok(Json::Arr(items)),
                Some(b',') => continue,
                _ => return Err("expected , or ]".into()),
            }
        }
    }

    fn object(&mut self) -> Result<Json, String> {
        self.bump();
        let mut items = Vec::new();
        if self.peek() == Some(b'}') {
            self.i += 1;
            return Ok(Json::Obj(items));
        }
        loop {
            if self.peek() != Some(b'"') {
                return Err("expected a key".into());
            }
            let key = self.string()?;
            if self.bump() != Some(b':') {
                return Err("expected :".into());
            }
            items.push((key, self.value()?));
            match self.bump() {
                Some(b'}') => return Ok(Json::Obj(items)),
                Some(b',') => continue,
                _ => return Err("expected , or }".into()),
            }
        }
    }
}

fn utf8_width(first: u8) -> usize {
    if first < 0x80 {
        1
    } else if first & 0xe0 == 0xc0 {
        2
    } else if first & 0xf0 == 0xe0 {
        3
    } else if first & 0xf8 == 0xf0 {
        4
    } else {
        1
    }
}

fn parse_json(line: &str) -> Result<(Option<Json>, String, Json), String> {
    let mut p = JsonParser { s: line.as_bytes(), i: 0 };
    let value = p.value()?;
    p.skip();
    if p.i != p.s.len() {
        return Err("trailing junk".into());
    }
    let obj = match value {
        Json::Obj(items) => items,
        _ => return Err("request must be an object".into()),
    };
    let mut method = None;
    let mut id = None;
    let mut params = Json::Obj(Vec::new());
    let mut saw_id = false;
    for (k, v) in obj {
        match k.as_str() {
            "jsonrpc" => {
                if v.as_str() != Some("2.0") {
                    return Err("jsonrpc must be 2.0".into());
                }
            }
            "method" => method = v.as_str().map(|s| s.to_string()),
            "id" => {
                saw_id = true;
                id = Some(v);
            }
            "params" => params = v,
            _ => {}
        }
    }
    let method = method.ok_or("missing method")?;
    if !saw_id {
        return Ok((None, method, params));
    }
    Ok((id, method, params))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sees_a_listening_port() {
        let text = "LISTEN 0 2 *:3389 *:*\nESTAB 0 0 10.0.0.1:22 10.0.0.2:5000\n";
        assert!(listens_on(text, 3389));
        assert!(!listens_on(text, 22));
        assert!(listens_on("tcp4 0 0 *.3390 *.* LISTEN", 3390));
        assert!(!listens_on("tcp4 0 0 *.3390 *.* ESTABLISHED", 3390));
    }

    #[test]
    fn xrdp_display_from_sesman() {
        let lightdm = "root 700 1 0 ? 00:00:01 /usr/lib/xorg/Xorg :0 -seat seat0 -auth /var/run/lightdm/root/:0 -nolisten tcp vt7 -novtswitch\n";
        assert_eq!(
            xrdp_session(lightdm),
            Some((":0".into(), "/home/debian/.Xauthority".into()))
        );
        let both = "\
root 700 1 0 ? 00:00:01 /usr/lib/xorg/Xorg :0 -seat seat0 -auth /var/run/lightdm/root/:0 -nolisten tcp vt7\n\
debian 2794 1 0 04:31 ? 00:00:01 /usr/lib/xorg/Xorg :10 -auth .Xauthority -config xrdp/xorg.conf -noreset -nolisten tcp -logfile .xorgxrdp.%s.log\n";
        assert_eq!(
            xrdp_session(both),
            Some((":0".into(), "/home/debian/.Xauthority".into()))
        );
        let text = "debian 2794 1 0 04:31 ? 00:00:01 /usr/lib/xorg/Xorg :10 -auth .Xauthority -config xrdp/xorg.conf -noreset -nolisten tcp -logfile .xorgxrdp.%s.log\n";
        assert_eq!(
            xrdp_session(text),
            Some((":10".into(), "/home/debian/.Xauthority".into()))
        );
        let abs = "debian 1 1 ? /usr/lib/xorg/Xorg :11 -auth /home/debian/.Xauthority -config xrdp/xorg.conf\n";
        assert_eq!(
            xrdp_session(abs),
            Some((":11".into(), "/home/debian/.Xauthority".into()))
        );
        assert!(xrdp_session("root 1 1 ? /usr/sbin/xrdp --nodaemon").is_none());
        assert!(xrdp_session("").is_none());
    }

    #[test]
    fn xrdp_input_is_xdotool() {
        assert_eq!(
            xrdp_input_line("key leftctrl down").unwrap(),
            Some("xdotool keydown ctrl".into())
        );
        assert_eq!(
            xrdp_input_line("key enter").unwrap(),
            Some("xdotool key --clearmodifiers Return".into())
        );
        assert_eq!(
            xrdp_input_line("move 12 34").unwrap(),
            Some("xdotool mousemove --sync 12 34".into())
        );
        assert_eq!(
            xrdp_input_line("button right").unwrap(),
            Some("xdotool click --clearmodifiers 3".into())
        );
        assert_eq!(
            xrdp_input_line("scroll -2").unwrap(),
            Some("i=0; while [ \"$i\" -lt 2 ]; do xdotool mousedown 5 && xdotool mouseup 5; i=$((i+1)); done".into())
        );
        assert_eq!(
            xrdp_input_line("type a'b").unwrap(),
            Some("xdotool type -- 'a'\\''b'".into())
        );
        assert!(xrdp_input_line("where").unwrap().is_none());
        assert!(xrdp_input_line("key nosuch").is_err());
        let script = xrdp_input_script(":10", "/home/debian/.Xauthority", &["key enter".into()]).unwrap();
        assert!(script.contains("DISPLAY=':10'"));
        assert!(script.contains("xdotool key --clearmodifiers Return"));
        assert!(!script.contains("getmouselocation"));
        let open = xrdp_open_script(":10", "/home/debian/.Xauthority", "firefox-esr https://example.com");
        assert!(open.contains("DISPLAY=':10'"));
        assert!(open.contains("(firefox-esr https://example.com) &"));
        assert!(open.contains(".xsession.vmagent"));
        let launch = firefox_launch_script(
            ":0",
            "/home/debian/.Xauthority",
            9333,
            &["https://www.youtube.com/".into()],
        );
        assert!(launch.contains("/tmp/vmagent-ublock.xpi"));
        assert!(launch.contains("uBlock0@raymondhill.net.xpi"));
    }

    #[test]
    fn rewrites_vm_host() {
        assert_eq!(rewrite_vm("debian@vm", "192.168.64.2"), "debian@192.168.64.2");
        assert_eq!(rewrite_vm("debian@vm:hello", "192.168.64.2"), "debian@192.168.64.2:hello");
        assert_eq!(rewrite_vm("./hello", "192.168.64.2"), "./hello");
        assert_eq!(rewrite_vm("debian@other:x", "192.168.64.2"), "debian@other:x");
    }

    #[test]
    fn quotes_for_remote_shell() {
        assert_eq!(sh_quote("ok"), "'ok'");
        assert_eq!(sh_quote("a'b"), "'a'\\''b'");
        assert_eq!(remote_shell(false, "id"), "sh -c 'id'");
        assert_eq!(remote_shell(true, "id"), "sudo sh -c 'id'");
    }

    #[test]
    fn parses_mac() {
        assert!(is_mac("aa:bb:cc:dd:ee:ff"));
        assert!(is_mac("aa-bb-cc-dd-ee-ff"));
        assert!(is_mac("36:7c:3:71:7b:5b"));
        assert!(!is_mac("not-a-mac"));
    }

    #[test]
    fn mac_for_dir_is_local_unicast() {
        let a = mac_for_dir(Path::new("/tmp/vm"));
        let b = mac_for_dir(Path::new("/tmp/vm"));
        assert_eq!(a, b);
        let n = normalize_mac(&a).unwrap();
        let first = u8::from_str_radix(&n[..2], 16).unwrap();
        assert_eq!(first & 1, 0);
        assert_eq!(first & 2, 2);
    }

    #[test]
    fn reads_vm_dir_from_ps() {
        let line = "  2585 /usr/local/bin/vmcore /tmp/vm/disk.img /tmp/vm/vmlinuz /tmp/vm/initrd root=LABEL=root 2 2048";
        assert_eq!(vm_dir_from_ps(line).unwrap(), PathBuf::from("/tmp/vm"));
        assert!(vm_dir_from_ps("  1 /bin/launchd").is_none());
        assert!(vm_dir_from_ps("  9 /tmp/vmcore /tmp/other.img").is_none());
    }

    #[test]
    fn grows_a_small_disk() {
        let dir = std::env::temp_dir().join(format!("vmagent-grow-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let disk = dir.join("disk.img");
        fs::write(&disk, [0u8; 8]).unwrap();
        grow_disk(&disk, 0);
        assert_eq!(fs::metadata(&disk).unwrap().len(), 8);
        grow_disk(&disk, 1);
        assert_eq!(fs::metadata(&disk).unwrap().len(), 1024 * 1024 * 1024);
        grow_disk(&disk, 1);
        assert_eq!(fs::metadata(&disk).unwrap().len(), 1024 * 1024 * 1024);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn finds_ip_in_arp() {
        let arp = "? (192.168.64.3) at 36:7c:3:71:7b:5b on bridge100 ifscope [bridge]\n";
        assert_eq!(
            ip_from_arp(arp, "36:7c:03:71:7b:5b").as_deref(),
            Some("192.168.64.3")
        );
    }

    #[test]
    fn parses_rpc_request() {
        let (id, method, params) = parse_json(
            r#"{"jsonrpc":"2.0","id":7,"method":"click","params":{"x":12,"y":34,"button":"left"}}"#,
        )
        .unwrap();
        assert_eq!(method, "click");
        assert_eq!(id.unwrap().encode(), "7");
        assert_eq!(params.get("x").and_then(|v| v.as_i32()), Some(12));
        assert_eq!(params.get("button").and_then(|v| v.as_str()), Some("left"));
    }

    #[test]
    fn rpc_roundtrip_escapes() {
        let (id, method, params) = parse_json(
            "{\"jsonrpc\":\"2.0\",\"id\":\"a\\nb\",\"method\":\"type\",\"params\":{\"text\":\"hi \\\"there\\\"\"}}",
        )
        .unwrap();
        assert_eq!(method, "type");
        assert_eq!(id.unwrap().as_str(), Some("a\nb"));
        assert_eq!(params.get("text").and_then(|v| v.as_str()), Some("hi \"there\""));
        let reply = rpc_ok(&Json::Int(1), &json_obj(&[("text", Json::Str("a\"b\\c".into()))]));
        assert!(reply.contains(r#""text":"a\"b\\c""#));
        let err = rpc_error(None, -32601, "unknown method click");
        assert!(err.contains("\"id\":null"));
        assert!(err.contains("-32601"));
    }

    #[test]
    fn rpc_notification_has_no_id() {
        let (id, method, _) = parse_json(r#"{"jsonrpc":"2.0","method":"ping"}"#).unwrap();
        assert!(id.is_none());
        assert_eq!(method, "ping");
        assert!(parse_json("{").is_err());
        assert!(parse_json(r#"{"method":"x","params":{"n":1.5}}"#).is_err());
    }
}
