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
//!   vmagent attach --dir /tmp/vm
//!   vmagent stop --dir /tmp/vm
//!   vmagent list
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
    /// expand the root filesystem. GNOME does not fit in the 3G cloud image.
    /// 0 leaves the image size.
    #[arg(long, default_value_t = 16)]
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
    List,
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
            Cmd::List => list_cmd(),
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
        "console=hvc0 ds=nocloud cloud-init=enabled"
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
    ]
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
    let status = Command::new(tool)
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
    let status = Command::new("ssh")
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
    let mut child = Command::new("ssh")
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
}
