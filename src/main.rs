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
use rand::{distributions::Alphanumeric, Rng};
use tokio::io::AsyncWriteExt;
use tokio::process::Command;

mod gui_backend_contract;

use gui_backend_contract::GuiSessionState as SessionState;

const RUN_DIR: &str = "/run/qgui";
const DATA_DIR: &str = "/var/lib/qgui";
const DEFAULT_DISPLAY: &str = ":1";
const DEFAULT_RES: &str = "1440x900";
const DEFAULT_DEPTH: u16 = 24;
const DEFAULT_BACKEND_PORT: u16 = 6080;
const DEFAULT_BACKEND_USER: &str = "quilt";

#[derive(Debug, Parser)]
#[command(
    name = "qgui",
    version,
    about = "Quilt GUI session manager (KasmVNC + XFCE)"
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
    /// X display to use (e.g. :1)
    #[arg(long, default_value = DEFAULT_DISPLAY)]
    display: String,

    /// Screen resolution (e.g. 1440x900)
    #[arg(long, default_value = DEFAULT_RES)]
    res: String,

    /// Color depth (bits per pixel)
    #[arg(long, default_value_t = DEFAULT_DEPTH)]
    depth: u16,

    /// KasmVNC bind address
    #[arg(long, default_value = "0.0.0.0")]
    bind: String,

    /// KasmVNC browser port
    #[arg(long, default_value_t = DEFAULT_BACKEND_PORT)]
    port: u16,

    /// Wait for readiness for up to N seconds.
    #[arg(long, default_value_t = 15)]
    wait_ready_secs: u64,
}

#[derive(Debug, Parser)]
struct LogsArgs {
    /// Component to show logs for: kasmvnc, dbus
    #[arg(long, default_value = "kasmvnc")]
    component: String,

    /// Number of bytes from end of file.
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
    /// Output format.
    #[arg(long, value_enum, default_value_t = EnvFormat::Shell)]
    format: EnvFormat,
}

#[derive(Debug, Parser)]
struct RunArgs {
    /// Command to run inside the active GUI session.
    #[arg(required = true, trailing_var_arg = true)]
    command: Vec<OsString>,
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
    fs::create_dir_all(vnc_dir()).context("create ~/.vnc")?;
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

fn vnc_dir() -> PathBuf {
    Path::new("/root/.vnc").to_path_buf()
}

fn kasmvnc_config_path() -> PathBuf {
    Path::new(RUN_DIR).join("kasmvnc.yaml")
}

fn kasmvnc_passwd_path() -> PathBuf {
    Path::new("/root/.kasmpasswd").to_path_buf()
}

fn xstartup_path() -> PathBuf {
    Path::new(RUN_DIR).join("xstartup")
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
    let connect_addr = match addr {
        "0.0.0.0" => "127.0.0.1",
        "::" | "[::]" => "::1",
        _ => addr,
    };

    (connect_addr, port)
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
    ["kasmvnc"].into_iter().map(component_report).collect()
}

fn component_report(name: &'static str) -> Result<ComponentReport> {
    let pid = read_pid(name)?;
    let state = match pid {
        Some(pid) if process_alive(pid) => HealthState::Running,
        Some(_) => HealthState::Dead,
        None => HealthState::Missing,
    };
    Ok(ComponentReport { name, pid, state })
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
    if let Some(report) = reports
        .into_iter()
        .find(|report| report.state != HealthState::Running)
    {
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
    if !tcp_listening(&state.backend_bind, state.backend_port) {
        bail!(
            "qgui session is not usable: KasmVNC is not listening on {}:{}",
            state.backend_bind,
            state.backend_port
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
        "kasmvncserver",
        "kasmvncpasswd",
        "xfce4-session",
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
    ensure_runtime_dir(&vnc_dir())?;
    let web_root = Path::new("/usr/share/kasmvnc/www/index.html");
    if !web_root.exists() {
        bail!("missing KasmVNC web asset: {}", web_root.display());
    }

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
        if state.auth_username.is_empty() || state.auth_password.is_empty() {
            bail!("qgui session is missing backend auth state");
        }
    }

    Ok(())
}

fn generate_backend_password() -> String {
    rand::thread_rng()
        .sample_iter(&Alphanumeric)
        .take(32)
        .map(char::from)
        .collect()
}

fn write_kasmvnc_config(args: &UpArgs) -> Result<PathBuf> {
    let config_path = kasmvnc_config_path();
    let config = format!(
        "desktop:\n  resolution:\n    width: {width}\n    height: {height}\n  allow_resize: true\n  pixel_depth: {depth}\nnetwork:\n  protocol: http\n  interface: {bind}\n  websocket_port: {port}\n  ssl:\n    require_ssl: false\n  udp:\n    public_ip: 127.0.0.1\ncommand_line:\n  prompt: false\n",
        width = args
            .res
            .split_once('x')
            .ok_or_else(|| anyhow!("resolution must be WIDTHxHEIGHT"))?
            .0,
        height = args
            .res
            .split_once('x')
            .ok_or_else(|| anyhow!("resolution must be WIDTHxHEIGHT"))?
            .1,
        depth = args.depth,
        bind = args.bind,
        port = args.port,
    );
    fs::write(&config_path, config).with_context(|| format!("write {}", config_path.display()))?;
    Ok(config_path)
}

fn write_xstartup(state: &SessionState) -> Result<PathBuf> {
    let path = xstartup_path();
    let contents = format!(
        "#!/bin/sh\nexport DISPLAY={display}\nexport DBUS_SESSION_BUS_ADDRESS={dbus}\nexport XDG_RUNTIME_DIR={xdg}\nexport QT_X11_NO_MITSHM=1\nexec xfce4-session\n",
        display = shell_escape(&state.display),
        dbus = shell_escape(&state.dbus_addr),
        xdg = shell_escape(&state.xdg_runtime_dir),
    );
    fs::write(&path, contents).with_context(|| format!("write {}", path.display()))?;
    let mut perms = fs::metadata(&path)
        .with_context(|| format!("stat {}", path.display()))?
        .permissions();
    perms.set_mode(0o755);
    fs::set_permissions(&path, perms).with_context(|| format!("chmod {}", path.display()))?;
    Ok(path)
}

async fn configure_backend_auth(state: &SessionState) -> Result<()> {
    let passwd_path = kasmvnc_passwd_path();
    let _ = fs::remove_file(&passwd_path);

    let mut cmd = Command::new("kasmvncpasswd");
    cmd.arg(&passwd_path)
        .arg("-u")
        .arg(&state.auth_username)
        .arg("-w")
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::piped());
    let mut child = cmd.spawn().context("spawn kasmvncpasswd")?;
    {
        let mut stdin = child
            .stdin
            .take()
            .ok_or_else(|| anyhow!("kasmvncpasswd stdin unavailable"))?;
        let payload = format!("{}\n{}\n", state.auth_password, state.auth_password);
        stdin
            .write_all(payload.as_bytes())
            .await
            .context("write kasmvncpasswd input")?;
    }
    let output = child
        .wait_with_output()
        .await
        .context("wait for kasmvncpasswd")?;
    if !output.status.success() {
        bail!(
            "kasmvncpasswd failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        );
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
    ensure_runtime_dir(&vnc_dir())?;

    {
        let mut cmd = Command::new("dbus-daemon");
        cmd.arg("--session")
            .arg("--fork")
            .arg("--address")
            .arg(&dbus_addr)
            .arg("--print-address");
        let _ = spawn_logged("dbus", cmd, &[]).await?;
    }

    let state = SessionState {
        display: display.clone(),
        res: args.res.clone(),
        depth: args.depth,
        backend_bind: args.bind.clone(),
        backend_port: args.port,
        dbus_addr: dbus_addr.clone(),
        dbus_socket: dbus_sock.display().to_string(),
        xdg_runtime_dir: xdg_runtime_dir.display().to_string(),
        auth_username: DEFAULT_BACKEND_USER.to_string(),
        auth_password: generate_backend_password(),
    };

    configure_backend_auth(&state).await?;
    let config_path = write_kasmvnc_config(args)?;
    let xstartup = write_xstartup(&state)?;

    let kasmvnc_pid = {
        let mut cmd = Command::new("kasmvncserver");
        cmd.arg(&display)
            .arg("-fg")
            .arg("-geometry")
            .arg(&args.res)
            .arg("-depth")
            .arg(args.depth.to_string())
            .arg("-xstartup")
            .arg(&xstartup)
            .arg("-interface")
            .arg(&args.bind)
            .arg("-websocketPort")
            .arg(args.port.to_string())
            .arg("-config")
            .arg(&config_path);
        let envs = session_env_pairs(&state);
        spawn_logged("kasmvnc", cmd, &envs).await?
    };
    write_pid("kasmvnc", kasmvnc_pid)?;

    wait_for_display_ready(&display, Duration::from_secs(10)).await?;

    let deadline = Instant::now() + Duration::from_secs(args.wait_ready_secs);
    while Instant::now() < deadline {
        let backend_ready = tcp_listening(&args.bind, args.port);
        if backend_ready && Path::new(&state.dbus_socket).exists() {
            write_state(&state)?;
            println!("qgui: display        {}", state.display);
            println!("qgui: resolution     {} depth={}", state.res, state.depth);
            println!(
                "qgui: kasmvnc        {}:{}  (browser backend)",
                state.backend_bind, state.backend_port
            );
            println!("qgui: dbus           {}", state.dbus_socket);
            println!("qgui: xdg_runtime    {}", state.xdg_runtime_dir);
            println!("qgui: ready");
            return Ok(state);
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
    }

    Err(anyhow!(
        "qgui up: timed out waiting for KasmVNC on {}:{}",
        args.bind,
        args.port
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

fn kill_kasmvnc_display(display: &str) {
    let _ = std::process::Command::new("kasmvncserver")
        .arg("-kill")
        .arg(display)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status();
}

fn cmd_down() -> Result<()> {
    ensure_dirs()?;
    let display = load_state()
        .ok()
        .map(|state| state.display)
        .unwrap_or_else(|| DEFAULT_DISPLAY.to_string());

    kill_kasmvnc_display(&display);

    for name in ["kasmvnc"] {
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
    let _ = fs::remove_file(kasmvnc_config_path());
    let _ = fs::remove_file(kasmvnc_passwd_path());
    let _ = fs::remove_file(xstartup_path());
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
        println!(
            "qgui: kasmvnc={} reachable={}",
            format!("{}:{}", state.backend_bind, state.backend_port),
            tcp_listening(&state.backend_bind, state.backend_port)
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
    if value.bytes().all(|byte| {
        matches!(
            byte,
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'_' | b'/' | b':' | b'.' | b'-'
        )
    }) {
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
    use super::{session_env_pairs, shell_escape, Cli, SessionState};
    use clap::CommandFactory;

    fn test_state() -> SessionState {
        SessionState {
            display: ":1".to_string(),
            res: "1440x900".to_string(),
            depth: 24,
            backend_bind: "0.0.0.0".to_string(),
            backend_port: 6080,
            dbus_addr: "unix:path=/run/qgui/dbus.sock".to_string(),
            dbus_socket: "/run/qgui/dbus.sock".to_string(),
            xdg_runtime_dir: "/run/qgui/xdg-runtime".to_string(),
            auth_username: "quilt".to_string(),
            auth_password: "secret".to_string(),
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

    #[test]
    fn cli_exposes_managed_session_subcommands() {
        let command = Cli::command();
        let subcommands: Vec<String> = command
            .get_subcommands()
            .map(|subcommand| subcommand.get_name().to_string())
            .collect();

        assert!(subcommands.iter().any(|name| name == "up"));
        assert!(subcommands.iter().any(|name| name == "env"));
        assert!(subcommands.iter().any(|name| name == "run"));
        assert!(subcommands.iter().any(|name| name == "doctor"));
    }
}
