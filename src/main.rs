//! Boot Debian on macOS.
//!
//! Apple's Virtualization.framework has no C API, so the VM itself is a tiny
//! Swift program (`vmcore`). This binary copies the cloud image, splits the
//! kernel and initrd out of it, and starts that helper.
//!
//!   vmagent --image debian.raw --user-data cloud-init/user-data --meta-data cloud-init/meta-data
//!
//! `ssh` and `scp` wrap the host tools. `user@vm` is the guest. The wrapper
//! reads the MAC from `--dir` and looks up the DHCP address. sshd listens on port 22.
//!
//!   vmagent --dir /tmp/vm --image debian.raw --user-data cloud-init/user-data
//!   vmagent ssh --dir /tmp/vm debian@vm
//!   vmagent scp --dir /tmp/vm ./hello debian@vm:hello
//!
//! Apple silicon, macOS 13+.

use std::fs::{self, File};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use clap::{Parser, Subcommand};

#[derive(Parser)]
#[command(name = "vmagent", version, about = "Boot a Debian cloud image on macOS")]
struct Cli {
    #[command(subcommand)]
    cmd: Option<Cmd>,

    /// Path to a Debian generic raw image (arm64 on Apple silicon).
    #[arg(long, required_unless_present = "cmd")]
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
}

fn main() {
    let cli = Cli::parse();
    if let Some(cmd) = cli.cmd {
        match cmd {
            Cmd::Ssh { dir, args } => ssh_cmd(&dir, &args),
            Cmd::Scp { dir, args } => scp_cmd(&dir, &args),
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

    let cloud_init = match (&cli.user_data, &cli.meta_data) {
        (None, None) => None,
        _ => Some(write_cidata(
            &dir,
            cli.user_data.as_deref(),
            cli.meta_data.as_deref(),
        )),
    };

    let append = if cloud_init.is_some() { "ds=nocloud" } else { "" };
    let cmdline = split_image(&disk, &dir, append);
    eprintln!("kernel command line: {cmdline}");

    let vmcore = find_vmcore();
    eprintln!("booting with {}", vmcore.display());
    if cloud_init.is_none() {
        eprintln!("no user-data: the generic image has no default login");
    }
    eprintln!("ssh: vmagent ssh --dir {} debian@vm", dir.display());
    eprintln!("close the window to stop");

    let mac_file = dir.join("ssh.mac");
    let _ = fs::remove_file(&mac_file);
    let mut cmd = Command::new(&vmcore);
    cmd.env("VM_SSH_MAC_FILE", &mac_file);
    cmd.arg(&disk)
        .arg(dir.join("vmlinuz"))
        .arg(dir.join("initrd"))
        .arg(&cmdline)
        .arg(cli.cpus.to_string())
        .arg(cli.mem_mb.to_string());
    if let Some(cloud_init) = &cloud_init {
        cmd.arg(cloud_init);
    }
    let status = cmd
        .stdin(Stdio::inherit())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit())
        .status();

    match status {
        Ok(s) if s.success() => {}
        Ok(s) => std::process::exit(s.code().unwrap_or(1)),
        Err(e) => die(&format!("failed to run {}: {e}", vmcore.display())),
    }
}

fn ssh_mac(dir: &Path) -> String {
    let path = dir.join("ssh.mac");
    let text = fs::read_to_string(&path).unwrap_or_else(|_| {
        die(&format!("no MAC in {} (is the VM running?)", dir.display()))
    });
    let mac = text.trim();
    if !is_mac(mac) {
        die("ssh mac file is not a MAC address");
    }
    mac.to_string()
}

fn is_mac(mac: &str) -> bool {
    let parts: Vec<&str> = mac.split(':').collect();
    parts.len() == 6
        && parts
            .iter()
            .all(|p| p.len() == 2 && p.chars().all(|c| c.is_ascii_hexdigit()))
}

/// Guest address from Apple's DHCP lease file. The lease matches this VM's MAC.
fn guest_ip(mac: &str) -> String {
    let wanted = mac.to_ascii_lowercase();
    let text = fs::read_to_string("/var/db/dhcpd_leases").unwrap_or_else(|_| {
        die("no DHCP leases yet (is the guest up?)")
    });
    let mut ip: Option<String> = None;
    let mut hit = false;
    for line in text.lines() {
        let line = line.trim();
        if line == "{" {
            ip = None;
            hit = false;
            continue;
        }
        if line == "}" {
            if hit {
                if let Some(ip) = ip {
                    return ip;
                }
                die(&format!("DHCP lease for {mac} has no address"));
            }
            continue;
        }
        let Some((key, value)) = line.split_once('=') else {
            continue;
        };
        let value = value.trim();
        if key == "ip_address" && !value.is_empty() {
            ip = Some(value.to_string());
        } else if key == "hw_address" && value.to_ascii_lowercase().ends_with(&wanted) {
            hit = true;
        }
    }
    die(&format!("no DHCP lease for {mac} yet (is the guest up?)"));
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
    let ip = guest_ip(&ssh_mac(dir));
    let args: Vec<String> = args.iter().map(|a| rewrite_vm(a, &ip)).collect();
    let status = Command::new(tool)
        .args(ssh_base())
        .args(&args)
        .status()
        .unwrap_or_else(|e| die(&format!("cannot run {tool}: {e}")));
    std::process::exit(status.code().unwrap_or(1));
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
    fn parses_mac() {
        assert!(is_mac("aa:bb:cc:dd:ee:ff"));
        assert!(!is_mac("aa-bb-cc-dd-ee-ff"));
        assert!(!is_mac("not-a-mac"));
    }
}
