use std::fs::{self, OpenOptions};
use std::io::Write;
use std::net::TcpStream;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::{Duration, Instant};

use anyhow::{anyhow, Context, Result};
use clap::{Parser, Subcommand};
use nix::sys::signal::{kill, Signal};
use nix::unistd::Pid;
use tokio::process::Command;

const RUN_DIR: &str = "/run/qgui";
const DATA_DIR: &str = "/var/lib/qgui";
const DEFAULT_DISPLAY: &str = ":1";
const DEFAULT_RES: &str = "1920x1080";
const DEFAULT_DEPTH: u16 = 24;
const DEFAULT_VNC_PORT: u16 = 5901;
const DEFAULT_NOVNC_PORT: u16 = 6080;

#[derive(Debug, Parser)]
#[command(
    name = "qgui",
    version,
    about = "Container GUI orchestrator (Xvfb + XFCE + VNC + noVNC)"
)]
struct Cli {
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Debug, Subcommand)]
enum Cmd {
    /// Start the GUI stack (detaches by default, writes PID files).
    Up(UpArgs),
    /// Stop the GUI stack (kills processes from PID files).
    Down,
    /// Show status (best-effort) of the GUI stack.
    Status,
    /// Print recent logs for GUI components.
    Logs(LogsArgs),
    /// Verify required binaries and basic runtime prerequisites.
    Doctor,
}

#[derive(Debug, Parser)]
struct UpArgs {
    /// X display to use (e.g. :1)
    #[arg(long, default_value = DEFAULT_DISPLAY)]
    display: String,

    /// Screen resolution (e.g. 1920x1080)
    #[arg(long, default_value = DEFAULT_RES)]
    res: String,

    /// Color depth (bits per pixel)
    #[arg(long, default_value_t = DEFAULT_DEPTH)]
    depth: u16,

    /// VNC bind address (default: 127.0.0.1, for intentional tunneling only)
    #[arg(long, default_value = "127.0.0.1")]
    vnc_bind: String,

    /// VNC port
    #[arg(long, default_value_t = DEFAULT_VNC_PORT)]
    vnc_port: u16,

    /// noVNC/websockify bind address (default: 0.0.0.0 so a reverse proxy can reach it)
    #[arg(long, default_value = "0.0.0.0")]
    novnc_bind: String,

    /// noVNC/websockify port
    #[arg(long, default_value_t = DEFAULT_NOVNC_PORT)]
    novnc_port: u16,

    /// noVNC web root directory (contains vnc.html, app/, etc)
    #[arg(long, default_value = "/usr/share/novnc")]
    novnc_web_root: String,

    /// Wait for readiness (TCP listen on novnc_port) for up to N seconds.
    #[arg(long, default_value_t = 10)]
    wait_ready_secs: u64,
}

#[derive(Debug, Parser)]
struct LogsArgs {
    /// Component to show logs for: xvfb, xfce, x11vnc, websockify, dbus
    #[arg(long, default_value = "websockify")]
    component: String,

    /// Number of bytes from end of file.
    #[arg(long, default_value_t = 32_768)]
    tail_bytes: usize,
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();
    match cli.cmd {
        Cmd::Up(args) => cmd_up(args).await,
        Cmd::Down => cmd_down(),
        Cmd::Status => cmd_status(),
        Cmd::Logs(args) => cmd_logs(args),
        Cmd::Doctor => cmd_doctor(),
    }
}

fn ensure_dirs() -> Result<()> {
    fs::create_dir_all(RUN_DIR).context("create /run/qgui")?;
    fs::create_dir_all(DATA_DIR).context("create /var/lib/qgui")?;
    fs::create_dir_all(Path::new(DATA_DIR).join("logs")).context("create /var/lib/qgui/logs")?;
    Ok(())
}

fn pid_path(name: &str) -> PathBuf {
    Path::new(RUN_DIR).join(format!("{}.pid", name))
}

fn log_path(name: &str) -> PathBuf {
    Path::new(DATA_DIR)
        .join("logs")
        .join(format!("{}.log", name))
}

fn write_pid(name: &str, pid: u32) -> Result<()> {
    fs::write(pid_path(name), pid.to_string()).context("write pid file")?;
    Ok(())
}

fn read_pid(name: &str) -> Result<Option<u32>> {
    let p = pid_path(name);
    if !p.exists() {
        return Ok(None);
    }
    let s = fs::read_to_string(&p).context("read pid file")?;
    let pid: u32 = s.trim().parse().context("parse pid")?;
    Ok(Some(pid))
}

fn kill_pid(pid: u32, sig: Signal) -> Result<()> {
    kill(Pid::from_raw(pid as i32), sig).context("kill")?;
    Ok(())
}

fn process_alive(pid: u32) -> bool {
    Path::new(&format!("/proc/{}", pid)).exists()
}

fn tcp_listening(addr: &str, port: u16) -> bool {
    TcpStream::connect((addr, port)).is_ok()
}

async fn spawn_logged(name: &str, mut cmd: Command, extra_env: &[(&str, &str)]) -> Result<u32> {
    let lp = log_path(name);
    let mut f = OpenOptions::new()
        .create(true)
        .append(true)
        .open(&lp)
        .with_context(|| format!("open log file {}", lp.display()))?;

    writeln!(
        f,
        "\n== {}: starting at {:?} ==\n",
        name,
        chrono::Utc::now()
    )
    .ok();

    let f2 = f.try_clone().context("clone log file handle")?;
    cmd.stdout(Stdio::from(f));
    cmd.stderr(Stdio::from(f2));
    for (k, v) in extra_env {
        cmd.env(k, v);
    }

    let child = cmd.spawn().with_context(|| format!("spawn {}", name))?;
    let pid = child
        .id()
        .ok_or_else(|| anyhow!("missing pid for {}", name))?;
    Ok(pid)
}

async fn cmd_up(args: UpArgs) -> Result<()> {
    ensure_dirs()?;

    // Basic validation (fail fast, clear errors).
    cmd_doctor()?;

    let display = args.display.clone();

    // dbus session bus (scoped).
    let dbus_sock = Path::new(RUN_DIR).join("dbus.sock");
    let dbus_addr = format!("unix:path={}", dbus_sock.display());

    // Start dbus-daemon --session (forks itself).
    {
        let mut cmd = Command::new("dbus-daemon");
        cmd.arg("--session")
            .arg("--fork")
            .arg("--address")
            .arg(&dbus_addr);
        // dbus-daemon forks, so the spawned child PID is not useful; we still log output.
        let _ = spawn_logged("dbus", cmd, &[]).await?;
    }

    // Xvfb
    let xvfb_pid = {
        let mut cmd = Command::new("Xvfb");
        cmd.arg(&display)
            .arg("-screen")
            .arg("0")
            .arg(format!("{}x{}", args.res, args.depth))
            .arg("-nolisten")
            .arg("tcp");
        spawn_logged("xvfb", cmd, &[]).await?
    };
    write_pid("xvfb", xvfb_pid)?;

    // XFCE session
    let xfce_pid = {
        let cmd = Command::new("startxfce4");
        // Ensure DBUS session bus is used by the desktop processes.
        let envs = [
            ("DISPLAY", display.as_str()),
            ("DBUS_SESSION_BUS_ADDRESS", dbus_addr.as_str()),
        ];
        spawn_logged("xfce", cmd, &envs).await?
    };
    write_pid("xfce", xfce_pid)?;

    // x11vnc bound to loopback by default.
    let vnc_pid = {
        let mut cmd = Command::new("x11vnc");
        cmd.arg("-display")
            .arg(&display)
            .arg("-rfbaddr")
            .arg(&args.vnc_bind)
            .arg("-rfbport")
            .arg(args.vnc_port.to_string())
            .arg("-nopw")
            .arg("-forever")
            .arg("-shared")
            .arg("-repeat")
            .arg("-noxdamage");
        spawn_logged("x11vnc", cmd, &[("DISPLAY", display.as_str())]).await?
    };
    write_pid("x11vnc", vnc_pid)?;

    // websockify/noVNC (bind 0.0.0.0 so an internal reverse proxy can reach it).
    let ws_pid = {
        let web_root = args.novnc_web_root.clone();
        if !Path::new(&web_root).exists() {
            return Err(anyhow!(
                "noVNC web root not found at {} (expected vnc.html)",
                web_root
            ));
        }
        let mut cmd = Command::new("websockify");
        cmd.arg("--web")
            .arg(&web_root)
            .arg(format!("{}:{}", args.novnc_bind, args.novnc_port))
            .arg(format!("{}:{}", args.vnc_bind, args.vnc_port));
        spawn_logged("websockify", cmd, &[]).await?
    };
    write_pid("websockify", ws_pid)?;

    // Readiness: wait for websockify listen.
    let deadline = Instant::now() + Duration::from_secs(args.wait_ready_secs);
    while Instant::now() < deadline {
        if tcp_listening(&args.novnc_bind, args.novnc_port) {
            println!("qgui: display        {}", display);
            println!("qgui: resolution     {}x{}", args.res, args.depth);
            println!(
                "qgui: vnc            {}:{}  (enabled)",
                args.vnc_bind, args.vnc_port
            );
            println!(
                "qgui: novnc          {}:{}  (enabled)",
                args.novnc_bind, args.novnc_port
            );
            println!("qgui: ready");
            return Ok(());
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
    }

    Err(anyhow!(
        "qgui up: timed out waiting for novnc/websockify to listen on {}:{}",
        args.novnc_bind,
        args.novnc_port
    ))
}

fn cmd_down() -> Result<()> {
    ensure_dirs()?;

    // Stop in reverse order.
    for name in ["websockify", "x11vnc", "xfce", "xvfb"] {
        if let Some(pid) = read_pid(name)? {
            if process_alive(pid) {
                let _ = kill_pid(pid, Signal::SIGTERM);
                std::thread::sleep(Duration::from_millis(300));
                if process_alive(pid) {
                    let _ = kill_pid(pid, Signal::SIGKILL);
                }
            }
            let _ = fs::remove_file(pid_path(name));
        }
    }

    println!("qgui: stopped");
    Ok(())
}

fn cmd_status() -> Result<()> {
    ensure_dirs()?;
    for name in ["xvfb", "xfce", "x11vnc", "websockify"] {
        let pid = read_pid(name)?;
        match pid {
            Some(p) if process_alive(p) => println!("qgui: {} pid={} status=running", name, p),
            Some(p) => println!("qgui: {} pid={} status=dead", name, p),
            None => println!("qgui: {} status=missing", name),
        }
    }
    Ok(())
}

fn cmd_logs(args: LogsArgs) -> Result<()> {
    ensure_dirs()?;
    let name = args.component;
    let lp = log_path(&name);
    if !lp.exists() {
        return Err(anyhow!("log file not found: {}", lp.display()));
    }
    let bytes = fs::read(&lp).with_context(|| format!("read log file {}", lp.display()))?;
    let start = bytes.len().saturating_sub(args.tail_bytes);
    let tail = &bytes[start..];
    std::io::stdout().write_all(tail).ok();
    Ok(())
}

fn cmd_doctor() -> Result<()> {
    // Keep this cheap and strict: fail fast if the expected binaries are missing.
    for bin in ["Xvfb", "startxfce4", "x11vnc", "websockify", "dbus-daemon"] {
        if which::which(bin).is_err() {
            return Err(anyhow!("missing required binary: {}", bin));
        }
    }
    Ok(())
}
