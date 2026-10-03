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
//!   vmagent firefox --dir /tmp/vm-gui open https://www.youtube.com/
//!   vmagent firefox --dir /tmp/vm-gui open --xpi ./uBlock0.firefox.xpi https://www.youtube.com/
//!   vmagent firefox --dir /tmp/vm-gui tabs
//!   vmagent attach --dir /tmp/vm
//!   vmagent stop --dir /tmp/vm
//!   vmagent list
//!   vmagent list --json
//!
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
    List {
        /// `[{"dir":"...","pid":1}]` instead of one directory per line.
        #[arg(long)]
        json: bool,
    },
    /// Start Firefox on the guest desktop and drive it over WebDriver BiDi.
    ///
    /// A clean profile, `--remote-debugging-port`, then commands on
    /// `ws://127.0.0.1:<port>/session`. The port stays on the guest.
    /// Nothing is typed into the window.
    ///
    /// `open` kills any Firefox this command started, wipes the profile, and
    /// starts one window. Extra URLs are extra tabs. `--xpi` sideloads an
    /// extension into that profile. `tabs`, `goto`, `eval`, `click`, `type`,
    /// `key`, `screenshot`, and `close` talk to the Firefox that is already
    /// listening.
    Firefox {
        #[arg(long)]
        dir: PathBuf,
        /// Guest BiDi port. Default: 9333.
        #[arg(long, default_value_t = 9333)]
        port: u16,
        #[command(subcommand)]
        action: FirefoxAction,
    },
}

#[derive(Subcommand)]
enum FirefoxAction {
    /// Close the previous window, start a clean one, open these URLs.
    Open {
        /// Sideload this extension. Repeatable.
        #[arg(long)]
        xpi: Vec<PathBuf>,
        /// Pages to open.
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        urls: Vec<String>,
    },
    /// Print id, url, and title of each tab. The active tab is marked.
    Tabs,
    /// Load a URL in the active tab.
    Goto { url: String },
    /// Run JavaScript in the active tab and print the result.
    Eval { expression: String },
    /// Click in the active tab, in CSS pixels from the viewport origin.
    Click {
        x: String,
        y: String,
        /// 0 left, 1 middle, 2 right.
        #[arg(long, default_value_t = 0)]
        button: i32,
    },
    /// Type into the active tab.
    Type { text: String },
    /// Press a key in the active tab. A letter, or a name such as `enter`.
    Key { name: String },
    /// JPEG of the active tab, as base64.
    Screenshot,
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
            Cmd::Attach { dir } => signal_vm(&dir, "-USR1"),
            Cmd::Stop { dir } => signal_vm(&dir, "-TERM"),
            Cmd::List { json } => list_cmd(json),
            Cmd::Firefox { dir, port, action } => firefox_cmd(&dir, port, action),
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
    ]
}

fn find_askpass() -> PathBuf {
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

fn apply_ssh_env(cmd: &mut Command) {
    cmd.env("SSH_ASKPASS", find_askpass());
    cmd.env("SSH_ASKPASS_REQUIRE", "force");
    cmd.env("DISPLAY", ":0");
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

fn run_ssh_tool(tool: &str, dir: &Path, args: &[String]) -> ! {
    let ip = guest_ip(&mac_for_dir(dir));
    let args: Vec<String> = args.iter().map(|a| rewrite_vm(a, &ip)).collect();
    let mut cmd = Command::new(tool);
    apply_ssh_env(&mut cmd);
    let status = cmd
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
    let mut cmd = Command::new("ssh");
    apply_ssh_env(&mut cmd);
    let status = cmd
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
    let mut cmd = Command::new("ssh");
    apply_ssh_env(&mut cmd);
    let mut child = cmd
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
    let mut cmd = Command::new("ssh");
    apply_ssh_env(&mut cmd);
    cmd.args(ssh_base())
        .arg(guest_target(dir))
        .arg(remote_shell(sudo, command))
        .status()
        .unwrap_or_else(|e| die(&format!("cannot run ssh: {e}")))
        .code()
        .unwrap_or(1)
}

fn guest_output(dir: &Path, sudo: bool, command: &str) -> Option<String> {
    let mut cmd = Command::new("ssh");
    apply_ssh_env(&mut cmd);
    let out = cmd
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

fn send_guest_file(dir: &Path, remote: &str, bytes: &[u8]) {
    let mut cmd = Command::new("ssh");
    apply_ssh_env(&mut cmd);
    let mut child = cmd
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
        .write_all(bytes)
        .unwrap_or_else(|e| die(&format!("cannot send {}: {e}", remote)));
    let status = child
        .wait()
        .unwrap_or_else(|e| die(&format!("ssh failed: {e}")));
    if !status.success() {
        die(&format!("cannot copy {remote}"));
    }
}

/// Display and `XAUTHORITY` from the Xorg that paints the virtio scanout.
fn x_session(ps: &str) -> Option<(String, String)> {
    parse_xorg_line(ps, |line| line.contains("Xorg") && !line.contains("xrdp"))
        .or_else(|| parse_xorg_line(ps, |line| line.contains("Xorg")))
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
        let auth = if display == ":0" && auth.contains("lightdm") {
            "/home/debian/.Xauthority".to_string()
        } else {
            auth
        };
        return Some((display.to_string(), auth));
    }
    None
}

fn x_session_of(dir: &Path) -> Option<(String, String)> {
    let text = guest_output(dir, true, "ps -ef")?;
    x_session(&text)
}

const FIREFOX_PROFILE: &str = "/home/debian/.cache/vmagent/firefox";

/// Prefs for a clean guest profile.
///
/// Remote debugging has to be on and the welcome page has to stay off.
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

/// Gecko add-on id from `manifest.json` inside the xpi.
fn firefox_addon_id(path: &Path) -> String {
    let out = Command::new("python3")
        .arg("-c")
        .arg(
            "import json,sys,zipfile\n\
             z=zipfile.ZipFile(sys.argv[1])\n\
             m=json.loads(z.read('manifest.json'))\n\
             g=(m.get('browser_specific_settings') or m.get('applications') or {}).get('gecko') or {}\n\
             print(g.get('id') or '')",
        )
        .arg(path)
        .output()
        .unwrap_or_else(|e| die(&format!("cannot read {}: {e}", path.display())));
    if !out.status.success() {
        eprint!("{}", String::from_utf8_lossy(&out.stderr));
        die(&format!("cannot read addon id from {}", path.display()));
    }
    let id = String::from_utf8_lossy(&out.stdout).trim().to_string();
    if id.is_empty() {
        die(&format!(
            "{} has no gecko id in manifest.json",
            path.display()
        ));
    }
    id
}

/// Shell that stops the Firefox this command started and launches a clean one.
///
/// Visible on the virtio display, not `--headless`. The desktop is the viewer.
/// `xpis` are guest path + add-on id pairs, already copied.
fn firefox_launch_script(
    display: &str,
    auth: &str,
    port: u16,
    urls: &[String],
    xpis: &[(String, String)],
) -> String {
    let urls = if urls.is_empty() {
        "about:blank".to_string()
    } else {
        urls.iter().map(|u| sh_quote(u)).collect::<Vec<_>>().join(" ")
    };
    let mut copies = String::new();
    for (remote, id) in xpis {
        copies.push_str(&format!(
            "cp {} \"$profile/extensions/\"{}.xpi\n",
            sh_quote(remote),
            sh_quote(id),
        ));
    }
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
         {copies}\
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
        copies = copies,
        display = sh_quote(display),
        auth = sh_quote(auth),
        port = port,
        cmdport = port.saturating_add(1),
        urls = urls,
    )
}

fn firefox_cmd(dir: &Path, port: u16, action: FirefoxAction) -> ! {
    match action {
        FirefoxAction::Open { xpi, urls } => {
            for url in &urls {
                if !(url.starts_with("http://")
                    || url.starts_with("https://")
                    || url.starts_with("about:"))
                {
                    die("firefox open takes http, https, or about: URLs");
                }
            }
            let Some((display, auth)) = x_session_of(dir) else {
                die("no desktop session yet");
            };
            install_firefox_bidi(dir);
            let mut guest_xpis = Vec::new();
            for (i, path) in xpi.iter().enumerate() {
                if !path.is_file() {
                    die(&format!("xpi not found: {}", path.display()));
                }
                let id = firefox_addon_id(path);
                let remote = format!("/tmp/vmagent-ext-{i}.xpi");
                let bytes = fs::read(path)
                    .unwrap_or_else(|e| die(&format!("cannot read {}: {e}", path.display())));
                send_guest_file(dir, &remote, &bytes);
                guest_xpis.push((remote, id));
            }
            let status = ssh_status(
                dir,
                true,
                &firefox_launch_script(&display, &auth, port, &urls, &guest_xpis),
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
        FirefoxAction::Click { x, y, button } => {
            firefox_bidi(dir, port, "click", &format!("{x} {y} {button}"))
        }
        FirefoxAction::Type { text } => firefox_bidi(dir, port, "type", &text),
        FirefoxAction::Key { name } => firefox_bidi(dir, port, "key", &name),
        FirefoxAction::Screenshot => firefox_bidi(dir, port, "screenshot", ""),
        FirefoxAction::Close { id } => {
            firefox_bidi(dir, port, "close", id.as_deref().unwrap_or(""))
        }
    }
}

/// Copy the BiDi helper onto the guest. The debug port is not published by NAT.
fn install_firefox_bidi(dir: &Path) {
    send_guest_file(
        dir,
        "/tmp/vmagent-firefox-bidi.py",
        include_bytes!("firefox_bidi.py"),
    );
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

fn list_cmd(json: bool) -> ! {
    let vms = running_vms();
    if json {
        print!("[");
        for (i, (pid, dir)) in vms.iter().enumerate() {
            if i > 0 {
                print!(",");
            }
            print!(
                "{{\"dir\":\"{}\",\"pid\":{pid}}}",
                json_escape(&dir.display().to_string())
            );
        }
        println!("]");
    } else {
        for (_, dir) in &vms {
            println!("{}", dir.display());
        }
    }
    std::process::exit(0);
}

fn json_escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            c => out.push(c),
        }
    }
    out
}

fn running_vms() -> Vec<(u32, PathBuf)> {
    let out = Command::new("ps")
        .args(["-axww", "-o", "pid=,command="])
        .output()
        .unwrap_or_else(|e| die(&format!("ps failed: {e}")));
    if !out.status.success() {
        die("ps failed");
    }
    let text = String::from_utf8_lossy(&out.stdout);
    let mut seen = std::collections::BTreeSet::new();
    let mut vms = Vec::new();
    for line in text.lines() {
        let Some((pid, dir)) = vm_from_ps(line) else {
            continue;
        };
        // One process, one line. Canonicalize only to drop a second spelling.
        let key = fs::canonicalize(&dir).unwrap_or_else(|_| dir.clone());
        if seen.insert(key) {
            vms.push((pid, dir));
        }
    }
    vms
}

/// `ps` line: pid, vmcore path, disk.img, then the rest of the boot args.
fn vm_from_ps(line: &str) -> Option<(u32, PathBuf)> {
    let mut parts = line.split_whitespace();
    let pid = parts.next()?.parse().ok()?;
    let bin = parts.next()?;
    if Path::new(bin).file_name()?.to_str()? != "vmcore" {
        return None;
    }
    let disk = parts.next()?;
    if !disk.ends_with("disk.img") {
        return None;
    }
    Some((pid, Path::new(disk).parent()?.to_path_buf()))
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

#[cfg(test)]
mod tests {
    use super::*;

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
        assert_eq!(vm_from_ps(line).unwrap(), (2585, PathBuf::from("/tmp/vm")));
        assert!(vm_from_ps("  1 /bin/launchd").is_none());
        assert!(vm_from_ps("  9 /tmp/vmcore /tmp/other.img").is_none());
    }

    #[test]
    fn json_escapes_paths() {
        assert_eq!(json_escape("/tmp/vm"), "/tmp/vm");
        assert_eq!(json_escape("a\"b\\c"), "a\\\"b\\\\c");
    }

    #[test]
    fn x_display_from_ps() {
        let lightdm = "root 700 1 0 ? 00:00:01 /usr/lib/xorg/Xorg :0 -seat seat0 -auth /var/run/lightdm/root/:0 -nolisten tcp vt7 -novtswitch\n";
        assert_eq!(
            x_session(lightdm),
            Some((":0".into(), "/home/debian/.Xauthority".into()))
        );
        let abs = "debian 1 1 ? /usr/lib/xorg/Xorg :11 -auth /home/debian/.Xauthority\n";
        assert_eq!(
            x_session(abs),
            Some((":11".into(), "/home/debian/.Xauthority".into()))
        );
        assert!(x_session("root 1 1 ? /usr/sbin/sshd").is_none());
        assert!(x_session("").is_none());
    }

    #[test]
    fn firefox_launch_sideloads_xpi() {
        let launch = firefox_launch_script(
            ":0",
            "/home/debian/.Xauthority",
            9333,
            &["https://www.youtube.com/".into()],
            &[("/tmp/vmagent-ext-0.xpi".into(), "uBlock0@raymondhill.net".into())],
        );
        assert!(launch.contains("/tmp/vmagent-ext-0.xpi"));
        assert!(launch.contains("uBlock0@raymondhill.net"));
        assert!(launch.contains("--remote-debugging-port=9333"));
        assert!(launch.contains("https://www.youtube.com/"));
        assert!(launch.contains("firefox-esr"));
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
}
