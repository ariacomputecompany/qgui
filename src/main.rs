use std::ffi::OsString;
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::net::{TcpStream, ToSocketAddrs};
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{ExitStatus, Stdio};
use std::time::{Duration, Instant};

use anyhow::{anyhow, bail, Context, Result};
use clap::{Args, Parser, Subcommand, ValueEnum};
use nix::sys::signal::{kill, Signal};
use nix::unistd::Pid;
use serde::{Deserialize, Serialize};
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
    about = "Quilt GUI session manager (Xvfb + XFCE + VNC + noVNC)"
)]
struct Cli {
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Debug, Subcommand)]
enum Cmd {
    /// Start and supervise the GUI session in the foreground.
    Up(UpArgs),
    /// Stop the GUI session.
    Down,
    /// Show the current GUI session contract and component health.
    Status,
    /// Print recent logs for GUI components.
    Logs(LogsArgs),
    /// Print environment variables required to launch GUI apps in the active session.
    Env(EnvArgs),
    /// Run a command inside the active GUI session with the correct environment.
    Run(RunArgs),
    /// Verify required binaries and runtime prerequisites.
    Doctor,
}

#[derive(Debug, Clone, Args)]
struct UpArgs {
    #[arg(long, default_value = DEFAULT_DISPLAY)]
    display: String,
    #[arg(long, default_value = DEFAULT_RES)]
    res: String,
    #[arg(long, default_value_t = DEFAULT_DEPTH)]
    depth: u16,
    #[arg(long, default_value = "127.0.0.1")]
    vnc_bind: String,
    #[arg(long, default_value_t = DEFAULT_VNC_PORT)]
    vnc_port: u16,
    #[arg(long, default_value = "0.0.0.0")]
    novnc_bind: String,
    #[arg(long, default_value_t = DEFAULT_NOVNC_PORT)]
    novnc_port: u16,
    #[arg(long, default_value = "/usr/share/novnc")]
    novnc_web_root: String,
    #[arg(long, default_value_t = 10)]
    wait_ready_secs: u64,
}

#[derive(Debug, Parser)]
struct LogsArgs {
    #[arg(long, default_value = "websockify")]
    component: String,
    #[arg(long, default_value_t = 32_768)]
    tail_bytes: usize,
}

#[derive(Debug, Clone, Copy, ValueEnum)]
enum EnvFormat {
    Shell,
    Json,
}

#[derive(Debug, Parser)]
struct EnvArgs {
    #[arg(long, value_enum, default_value_t = EnvFormat::Shell)]
    format: EnvFormat,
}

#[derive(Debug, Parser)]
struct RunArgs {
    #[arg(required = true, trailing_var_arg = true)]
    command: Vec<OsString>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct SessionState {
    display: String,
    res: String,
    depth: u16,
    vnc_bind: String,
    vnc_port: u16,
    novnc_bind: String,
    novnc_port: u16,
    novnc_web_root: String,
    dbus_addr: String,
    dbus_socket: String,
    xdg_runtime_dir: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum HealthState {
    Running,
    Dead,
    Missing,
}

impl HealthState {
    fn as_str(self) -> &'static str {
        match self {
            HealthState::Running => "running",
            HealthState::Dead => "dead",
            HealthState::Missing => "missing",
        }
    }
}

#[derive(Debug, Clone)]
struct ComponentReport {
    name: &'static str,
    pid: Option<u32>,
    state: HealthState,
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();
    match cli.cmd {
        Cmd::Up(args) => cmd_up(args).await,
        Cmd::Down => cmd_down(),
        Cmd::Status => cmd_status(),
        Cmd::Logs(args) => cmd_logs(args),
        Cmd::Env(args) => cmd_env(args),
        Cmd::Run(args) => cmd_run(args).await,
        Cmd::Doctor => cmd_doctor(),
    }
}

fn ensure_dirs() -> Result<()> {
    fs::create_dir_all(RUN_DIR).context("create /run/qgui")?;
    fs::create_dir_all(DATA_DIR).context("create /var/lib/qgui")?;
    fs::create_dir_all(Path::new(DATA_DIR).join("logs")).context("create /var/lib/qgui/logs")?;
    Ok(())
}

fn state_path() -> PathBuf {
    Path::new(RUN_DIR).join("session.json")
}

fn pid_path(name: &str) -> PathBuf {
    Path::new(RUN_DIR).join(format!("{}.pid", name))
}

fn log_path(name: &str) -> PathBuf {
    Path::new(DATA_DIR)
        .join("logs")
        .join(format!("{}.log", name))
}

fn write_state(state: &SessionState) -> Result<()> {
    let data = serde_json::to_vec_pretty(state).context("serialize qgui session state")?;
    fs::write(state_path(), data).context("write qgui session state")?;
    Ok(())
}

fn load_state() -> Result<SessionState> {
    let data = fs::read(state_path()).context("read qgui session state")?;
    serde_json::from_slice(&data).context("parse qgui session state")
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
    (addr, port)
        .to_socket_addrs()
        .ok()
        .into_iter()
        .flatten()
        .any(|sock| TcpStream::connect_timeout(&sock, Duration::from_millis(200)).is_ok())
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

fn display_lock_path(display: &str) -> Option<PathBuf> {
    display
        .strip_prefix(':')
        .map(|name| Path::new("/tmp").join(format!(".X{}-lock", name)))
}

fn display_socket_path(display: &str) -> Option<PathBuf> {
    display
        .strip_prefix(':')
        .map(|name| Path::new("/tmp/.X11-unix").join(format!("X{}", name)))
}

fn clear_display_artifacts(display: &str) {
    if let Some(lock) = display_lock_path(display) {
        let _ = fs::remove_file(lock);
    }
    if let Some(socket) = display_socket_path(display) {
        let _ = fs::remove_file(socket);
    }
}

async fn wait_for_display_ready(display: &str, timeout: Duration) -> Result<()> {
    let deadline = Instant::now() + timeout;
    let socket_path =
        display_socket_path(display).ok_or_else(|| anyhow!("invalid X display '{}'", display))?;

    while Instant::now() < deadline {
        if socket_path.exists() {
            return Ok(());
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
    }

    Err(anyhow!(
        "timed out waiting for X display {} to become ready",
        display
    ))
}

fn component_reports() -> Result<Vec<ComponentReport>> {
    ["xvfb", "xfce", "x11vnc", "websockify"]
        .into_iter()
        .map(|name| {
            let pid = read_pid(name)?;
            let state = match pid {
                Some(pid) if process_alive(pid) => HealthState::Running,
                Some(_) => HealthState::Dead,
                None => HealthState::Missing,
            };
            Ok(ComponentReport { name, pid, state })
        })
        .collect()
}

fn first_failed_component() -> Result<Option<&'static str>> {
    for report in component_reports()? {
        if report.state != HealthState::Running {
            return Ok(Some(report.name));
        }
    }
    Ok(None)
}

fn session_env_pairs(state: &SessionState) -> [(&'static str, &str); 4] {
    [
        ("DISPLAY", state.display.as_str()),
        ("DBUS_SESSION_BUS_ADDRESS", state.dbus_addr.as_str()),
        ("XDG_RUNTIME_DIR", state.xdg_runtime_dir.as_str()),
        ("QT_X11_NO_MITSHM", "1"),
    ]
}

fn require_active_session_state() -> Result<SessionState> {
    let state = load_state().context("no active qgui session; run `qgui up` first")?;
    let reports = component_reports()?;
    let first_bad = reports
        .into_iter()
        .find(|report| report.state != HealthState::Running);
    if let Some(report) = first_bad {
        bail!(
            "qgui session is not usable: component '{}' is {}",
            report.name,
            report.state.as_str()
        );
    }
    if !Path::new(&state.dbus_socket).exists() {
        bail!(
            "qgui session is not usable: dbus session socket missing at {}",
            state.dbus_socket
        );
    }
    if !tcp_listening(&state.vnc_bind, state.vnc_port) {
        bail!(
            "qgui session is not usable: VNC is not listening on {}:{}",
            state.vnc_bind,
            state.vnc_port
        );
    }
    if !tcp_listening(&state.novnc_bind, state.novnc_port) {
        bail!(
            "qgui session is not usable: noVNC/websockify is not listening on {}:{}",
            state.novnc_bind,
            state.novnc_port
        );
    }
    Ok(state)
}

fn ensure_runtime_dir(path: &Path) -> Result<()> {
    fs::create_dir_all(path).with_context(|| format!("create {}", path.display()))?;
    let mut perms = fs::metadata(path)
        .with_context(|| format!("stat {}", path.display()))?
        .permissions();
    perms.set_mode(0o700);
    fs::set_permissions(path, perms).with_context(|| format!("chmod 0700 {}", path.display()))?;
    Ok(())
}

fn validate_resolution(res: &str) -> Result<()> {
    let mut parts = res.split('x');
    let width = parts
        .next()
        .ok_or_else(|| anyhow!("resolution must be WIDTHxHEIGHT"))?;
    let height = parts
        .next()
        .ok_or_else(|| anyhow!("resolution must be WIDTHxHEIGHT"))?;
    if parts.next().is_some() || width.parse::<u32>().is_err() || height.parse::<u32>().is_err() {
        bail!("resolution must be WIDTHxHEIGHT");
    }
    Ok(())
}

fn no_vnc_root_checks(web_root: &Path) -> Result<()> {
    for required in ["vnc.html", "app/ui.js", "app/images"] {
        let path = web_root.join(required);
        if !path.exists() {
            bail!("missing noVNC asset: {}", path.display());
        }
    }
    Ok(())
}

fn file_descriptor_limit() -> Result<u64> {
    let mut limit = libc::rlimit {
        rlim_cur: 0,
        rlim_max: 0,
    };
    let rc = unsafe { libc::getrlimit(libc::RLIMIT_NOFILE, &mut limit) };
    if rc != 0 {
        return Err(anyhow!("getrlimit(RLIMIT_NOFILE) failed"));
    }
    Ok(limit.rlim_cur)
}

fn run_doctor(check_ports: bool) -> Result<()> {
    ensure_dirs()?;
    for bin in [
        "Xvfb",
        "xfce4-session",
        "x11vnc",
        "websockify",
        "dbus-daemon",
        "xrdb",
        "xauth",
        "xsetroot",
    ] {
        which::which(bin).with_context(|| format!("missing required binary: {}", bin))?;
    }

    let fd_limit = file_descriptor_limit()?;
    if fd_limit < 4096 {
        bail!(
            "RLIMIT_NOFILE is too low for GUI workloads: {} (need >= 4096)",
            fd_limit
        );
    }

    let run_dir = Path::new(RUN_DIR);
    ensure_runtime_dir(run_dir)?;
    ensure_runtime_dir(&Path::new(RUN_DIR).join("xdg-runtime"))?;
    no_vnc_root_checks(Path::new("/usr/share/novnc"))?;

    if let Some(lock_path) = display_lock_path(DEFAULT_DISPLAY) {
        let socket_path = display_socket_path(DEFAULT_DISPLAY).unwrap_or_default();
        if lock_path.exists() && !socket_path.exists() {
            bail!(
                "stale X lock detected at {} without a matching X socket; run `qgui down` or remove it",
                lock_path.display()
            );
        }
    }

    if check_ports {
        let state = require_active_session_state()?;
        if !Path::new(&state.novnc_web_root).exists() {
            bail!("noVNC web root missing at {}", state.novnc_web_root);
        }
    }

    Ok(())
}

async fn start_stack(args: &UpArgs) -> Result<SessionState> {
    ensure_dirs()?;
    validate_resolution(&args.res)?;
    run_doctor(false)?;
    let _ = cmd_down();
    clear_display_artifacts(&args.display);

    let display = args.display.clone();
    let dbus_sock = Path::new(RUN_DIR).join("dbus.sock");
    let dbus_addr = format!("unix:path={}", dbus_sock.display());
    let xdg_runtime_dir = Path::new(RUN_DIR).join("xdg-runtime");
    ensure_runtime_dir(&xdg_runtime_dir)?;

    {
        let mut cmd = Command::new("dbus-daemon");
        cmd.arg("--session")
            .arg("--fork")
            .arg("--address")
            .arg(&dbus_addr)
            .arg("--print-address");
        let _ = spawn_logged("dbus", cmd, &[]).await?;
    }

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
    wait_for_display_ready(&display, Duration::from_secs(5)).await?;

    let state = SessionState {
        display: display.clone(),
        res: args.res.clone(),
        depth: args.depth,
        vnc_bind: args.vnc_bind.clone(),
        vnc_port: args.vnc_port,
        novnc_bind: args.novnc_bind.clone(),
        novnc_port: args.novnc_port,
        novnc_web_root: args.novnc_web_root.clone(),
        dbus_addr: dbus_addr.clone(),
        dbus_socket: dbus_sock.display().to_string(),
        xdg_runtime_dir: xdg_runtime_dir.display().to_string(),
    };

    let xfce_pid = {
        let cmd = Command::new("xfce4-session");
        let envs = session_env_pairs(&state);
        spawn_logged("xfce", cmd, &envs).await?
    };
    write_pid("xfce", xfce_pid)?;

    let vnc_pid = {
        let mut cmd = Command::new("x11vnc");
        cmd.arg("-display")
            .arg(&display)
            .arg("-rfbport")
            .arg(args.vnc_port.to_string())
            .arg("-nopw")
            .arg("-forever")
            .arg("-shared")
            .arg("-loop")
            .arg("-repeat")
            .arg("-noxdamage");
        if args.vnc_bind == "127.0.0.1" || args.vnc_bind == "localhost" {
            cmd.arg("-localhost");
        } else {
            cmd.arg("-listen").arg(&args.vnc_bind);
        }
        let envs = session_env_pairs(&state);
        spawn_logged("x11vnc", cmd, &envs).await?
    };
    write_pid("x11vnc", vnc_pid)?;

    let ws_pid = {
        let web_root = args.novnc_web_root.clone();
        let web_root_path = Path::new(&web_root);
        if !web_root_path.exists() {
            return Err(anyhow!(
                "noVNC web root not found at {} (expected vnc.html)",
                web_root
            ));
        }
        no_vnc_root_checks(web_root_path)?;
        let mut cmd = Command::new("websockify");
        cmd.arg("--web")
            .arg(&web_root)
            .arg(format!("{}:{}", args.novnc_bind, args.novnc_port))
            .arg(format!("{}:{}", args.vnc_bind, args.vnc_port));
        spawn_logged("websockify", cmd, &[]).await?
    };
    write_pid("websockify", ws_pid)?;

    let deadline = Instant::now() + Duration::from_secs(args.wait_ready_secs);
    while Instant::now() < deadline {
        let novnc_ready = tcp_listening(&args.novnc_bind, args.novnc_port);
        let vnc_ready = tcp_listening(&args.vnc_bind, args.vnc_port);
        if novnc_ready && vnc_ready && Path::new(&state.dbus_socket).exists() {
            write_state(&state)?;
            println!("qgui: display        {}", state.display);
            println!("qgui: resolution     {}x{}", state.res, state.depth);
            println!(
                "qgui: vnc            {}:{}  (loopback-only, proxy auth)",
                state.vnc_bind, state.vnc_port
            );
            println!(
                "qgui: novnc          {}:{}  (enabled)",
                state.novnc_bind, state.novnc_port
            );
            println!("qgui: dbus           {}", state.dbus_socket);
            println!("qgui: xdg_runtime    {}", state.xdg_runtime_dir);
            println!("qgui: ready");
            return Ok(state);
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
    }

    Err(anyhow!(
        "qgui up: timed out waiting for full GUI readiness on noVNC {}:{} and VNC {}:{}",
        args.novnc_bind,
        args.novnc_port,
        args.vnc_bind,
        args.vnc_port
    ))
}

async fn cmd_up(args: UpArgs) -> Result<()> {
    start_stack(&args).await?;

    #[cfg(unix)]
    let mut term = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
        .context("install SIGTERM handler")?;
    #[cfg(unix)]
    let mut interrupt = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::interrupt())
        .context("install SIGINT handler")?;

    loop {
        if let Some(name) = first_failed_component()? {
            let _ = cmd_down();
            return Err(anyhow!("qgui up: component '{}' exited", name));
        }

        #[cfg(unix)]
        tokio::select! {
            _ = term.recv() => {
                cmd_down()?;
                return Ok(());
            }
            _ = interrupt.recv() => {
                cmd_down()?;
                return Ok(());
            }
            _ = tokio::time::sleep(Duration::from_millis(500)) => {}
        }

        #[cfg(not(unix))]
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
}

fn cmd_down() -> Result<()> {
    ensure_dirs()?;
    let display = load_state()
        .ok()
        .map(|state| state.display)
        .unwrap_or_else(|| DEFAULT_DISPLAY.to_string());

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

    let _ = fs::remove_file(Path::new(RUN_DIR).join("dbus.sock"));
    let _ = fs::remove_file(state_path());
    if let Some(socket) = display_socket_path(&display) {
        if !socket.exists() {
            clear_display_artifacts(&display);
        }
    }

    println!("qgui: stopped");
    Ok(())
}

fn cmd_status() -> Result<()> {
    ensure_dirs()?;
    let state = load_state().ok();
    if let Some(state) = state.as_ref() {
        println!(
            "qgui: display={} resolution={} depth={}",
            state.display, state.res, state.depth
        );
        println!("qgui: dbus_socket={}", state.dbus_socket);
        println!("qgui: xdg_runtime_dir={}", state.xdg_runtime_dir);
        println!("qgui: novnc_web_root={}", state.novnc_web_root);
        println!(
            "qgui: vnc={} reachable={}",
            format!("{}:{}", state.vnc_bind, state.vnc_port),
            tcp_listening(&state.vnc_bind, state.vnc_port)
        );
        println!(
            "qgui: novnc={} reachable={}",
            format!("{}:{}", state.novnc_bind, state.novnc_port),
            tcp_listening(&state.novnc_bind, state.novnc_port)
        );
    } else {
        println!("qgui: session=missing");
    }

    for report in component_reports()? {
        match report.pid {
            Some(pid) => println!(
                "qgui: component={} pid={} status={}",
                report.name,
                pid,
                report.state.as_str()
            ),
            None => println!(
                "qgui: component={} status={}",
                report.name,
                report.state.as_str()
            ),
        }
    }
    Ok(())
}

fn cmd_logs(args: LogsArgs) -> Result<()> {
    ensure_dirs()?;
    let lp = log_path(&args.component);
    if !lp.exists() {
        return Err(anyhow!("log file not found: {}", lp.display()));
    }
    let bytes = fs::read(&lp).with_context(|| format!("read log file {}", lp.display()))?;
    let start = bytes.len().saturating_sub(args.tail_bytes);
    std::io::stdout().write_all(&bytes[start..]).ok();
    Ok(())
}

fn cmd_env(args: EnvArgs) -> Result<()> {
    let state = require_active_session_state()?;
    match args.format {
        EnvFormat::Shell => {
            for (key, value) in session_env_pairs(&state) {
                println!("export {}={}", key, shell_escape(value));
            }
        }
        EnvFormat::Json => {
            let json = serde_json::json!({
                "DISPLAY": state.display,
                "DBUS_SESSION_BUS_ADDRESS": state.dbus_addr,
                "XDG_RUNTIME_DIR": state.xdg_runtime_dir,
                "QT_X11_NO_MITSHM": "1"
            });
            println!(
                "{}",
                serde_json::to_string_pretty(&json).context("serialize qgui env")?
            );
        }
    }
    Ok(())
}

async fn cmd_run(args: RunArgs) -> Result<()> {
    let state = require_active_session_state()?;
    let mut iter = args.command.into_iter();
    let program = iter
        .next()
        .ok_or_else(|| anyhow!("missing command; usage: qgui run -- <command...>"))?;
    let mut cmd = Command::new(&program);
    cmd.args(iter);
    cmd.stdin(Stdio::inherit());
    cmd.stdout(Stdio::inherit());
    cmd.stderr(Stdio::inherit());
    for (key, value) in session_env_pairs(&state) {
        cmd.env(key, value);
    }

    let status = cmd
        .status()
        .await
        .with_context(|| format!("spawn GUI app {:?}", program))?;
    exit_with_status(status)
}

fn cmd_doctor() -> Result<()> {
    run_doctor(true)?;
    println!("qgui: doctor ok");
    Ok(())
}

fn shell_escape(value: &str) -> String {
    if value
        .bytes()
        .all(|byte| matches!(byte, b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'_' | b'/' | b':' | b'.' | b'-'))
    {
        value.to_string()
    } else {
        format!("'{}'", value.replace('\'', "'\"'\"'"))
    }
}

fn exit_with_status(status: ExitStatus) -> Result<()> {
    if status.success() {
        return Ok(());
    }

    #[cfg(unix)]
    {
        use std::os::unix::process::ExitStatusExt;
        if let Some(code) = status.code() {
            std::process::exit(code);
        }
        if let Some(signal) = status.signal() {
            std::process::exit(128 + signal);
        }
    }

    bail!("GUI app failed: {}", status);
}

#[cfg(test)]
mod tests {
    use super::{session_env_pairs, shell_escape, SessionState};

    fn test_state() -> SessionState {
        SessionState {
            display: ":1".to_string(),
            res: "1920x1080".to_string(),
            depth: 24,
            vnc_bind: "127.0.0.1".to_string(),
            vnc_port: 5901,
            novnc_bind: "0.0.0.0".to_string(),
            novnc_port: 6080,
            novnc_web_root: "/usr/share/novnc".to_string(),
            dbus_addr: "unix:path=/run/qgui/dbus.sock".to_string(),
            dbus_socket: "/run/qgui/dbus.sock".to_string(),
            xdg_runtime_dir: "/run/qgui/xdg-runtime".to_string(),
        }
    }

    #[test]
    fn session_env_contains_gui_contract() {
        let state = test_state();
        let envs = session_env_pairs(&state);
        assert_eq!(envs[0], ("DISPLAY", ":1"));
        assert_eq!(
            envs[1],
            ("DBUS_SESSION_BUS_ADDRESS", "unix:path=/run/qgui/dbus.sock")
        );
        assert_eq!(envs[2], ("XDG_RUNTIME_DIR", "/run/qgui/xdg-runtime"));
        assert_eq!(envs[3], ("QT_X11_NO_MITSHM", "1"));
    }

    #[test]
    fn shell_escape_quotes_when_needed() {
        assert_eq!(shell_escape("/run/qgui/dbus.sock"), "/run/qgui/dbus.sock");
        assert_eq!(shell_escape("hello world"), "'hello world'");
    }
}
