use crate::{
    core::{self, Chain},
    web,
};
use anyhow::{bail, ensure, Context, Result};
use std::{
    fs::{self, File, OpenOptions},
    io,
    net::TcpListener,
    os::{fd::AsRawFd, unix::fs::OpenOptionsExt, unix::process::CommandExt},
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
    sync::atomic::{AtomicBool, Ordering::Relaxed},
    thread,
    time::{Duration, Instant},
};

const LIFECYCLE_LOCK: &str = "services.lock";
const DASHBOARD_LOCK: &str = "dashboard.lock";
const CONTROL_FILE: &str = "dashboard.token";
const DASHBOARD_LOG: &str = "dashboard.log";
pub const CONTROL_HEADER: &str = "X-Silicon-Control";
pub const CONTROL_STATUS: &str = "/_silicon/status";
pub const CONTROL_STOP: &str = "/_silicon/stop";
const START_TIMEOUT: Duration = Duration::from_secs(30);
const STOP_TIMEOUT: Duration = Duration::from_secs(120);
const POLL_INTERVAL: Duration = Duration::from_millis(100);

fn lock(state: &Path, name: &str, flags: libc::c_int) -> io::Result<File> {
    let file = OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(false)
        .mode(0o600)
        .open(state.join(name))?;
    if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | flags) } != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(file)
}

fn lifecycle_lock(state: &Path) -> Result<File> {
    lock(state, LIFECYCLE_LOCK, 0).context("Could not lock the service lifecycle")
}

fn dashboard_locked(state: &Path) -> Result<bool> {
    if !state.is_dir() {
        return Ok(false);
    }
    match lock(state, DASHBOARD_LOCK, libc::LOCK_NB) {
        Ok(_) => Ok(false),
        Err(error) if error.kind() == io::ErrorKind::WouldBlock => Ok(true),
        Err(error) => Err(error).context("Could not check the dashboard lock"),
    }
}

pub struct DashboardControl {
    file: PathBuf,
    token: String,
}

impl DashboardControl {
    pub fn publish(&self) -> Result<()> {
        core::write_private(&self.file, self.token.as_bytes())
    }

    pub fn accepts(&self, token: Option<&str>) -> bool {
        token == Some(self.token.as_str())
    }
}

impl Drop for DashboardControl {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.file);
    }
}

pub fn dashboard(state: PathBuf, shutdown: &AtomicBool, background: bool) -> Result<()> {
    core::prepare(&state)?;
    // Only the child launched by `start` already has a parent holding this lock.
    let lifecycle = (!background).then(|| lifecycle_lock(&state)).transpose()?;
    let _dashboard = lock(&state, DASHBOARD_LOCK, libc::LOCK_NB).map_err(|error| {
        if error.kind() == io::ErrorKind::WouldBlock {
            anyhow::anyhow!("Dashboard already running for {}", state.display())
        } else {
            anyhow::Error::new(error).context("Could not lock the dashboard")
        }
    })?;
    drop(lifecycle);

    let file = state.join(CONTROL_FILE);
    match fs::remove_file(&file) {
        Ok(()) => (),
        Err(error) if error.kind() == io::ErrorKind::NotFound => (),
        Err(error) => return Err(error).context("Could not clear the dashboard control file"),
    }
    let control = DashboardControl {
        file,
        token: core::random_hex(32)?,
    };
    web::run(state, shutdown, &control)
}

fn request_dashboard(state: &Path, route: &str) -> Result<()> {
    let token = fs::read_to_string(state.join(CONTROL_FILE))
        .context("Dashboard control credentials are unavailable")?;
    let response = minreq::post(format!("{}{route}", web::URL))
        .with_header(CONTROL_HEADER, token)
        .with_timeout(1)
        .with_max_redirects(0)
        .send()
        .context("Dashboard control request failed")?;
    ensure!(
        response.status_code == 200 && response.as_bytes() == br#"{"ok":true}"#,
        "Dashboard control request was rejected (HTTP {})",
        response.status_code
    );
    Ok(())
}

fn spawn_dashboard(state: &Path) -> Result<Child> {
    let log = OpenOptions::new()
        .append(true)
        .create(true)
        .mode(0o600)
        .open(state.join(DASHBOARD_LOG))?;
    let mut command = Command::new(std::env::current_exe()?);
    command
        .arg("dashboard-background")
        .stdin(Stdio::null())
        .stdout(log.try_clone()?)
        .stderr(log);
    // Detach from the caller's terminal before executing the dashboard.
    unsafe {
        command.pre_exec(|| {
            if libc::setsid() == -1 {
                return Err(io::Error::last_os_error());
            }
            Ok(())
        });
    }
    command.spawn().context("Could not launch the dashboard")
}

fn wait_for_dashboard(
    state: &Path,
    mut child: Option<&mut Child>,
    shutdown: &AtomicBool,
) -> Result<()> {
    let deadline = Instant::now() + START_TIMEOUT;
    loop {
        ensure!(!shutdown.load(Relaxed), "Start interrupted");
        if let Some(child) = child.as_mut() {
            if let Some(status) = child.try_wait()? {
                bail!("Dashboard exited with {status}. Run bitcoin-silicon logs for details.");
            }
        }
        if dashboard_locked(state)? && request_dashboard(state, CONTROL_STATUS).is_ok() {
            return Ok(());
        }
        ensure!(
            Instant::now() < deadline,
            "Dashboard did not become ready. Run bitcoin-silicon logs for details."
        );
        thread::sleep(POLL_INTERVAL);
    }
}

fn stop_child(child: &mut Child) -> Result<()> {
    if child.try_wait()?.is_some() {
        return Ok(());
    }
    // The unreaped Child owns this PID, even if the process exits before the signal.
    if unsafe { libc::kill(child.id() as libc::pid_t, libc::SIGTERM) } != 0 {
        let error = io::Error::last_os_error();
        if error.raw_os_error() != Some(libc::ESRCH) {
            return Err(error).context("Could not signal the new dashboard");
        }
    }
    let deadline = Instant::now() + STOP_TIMEOUT;
    while child.try_wait()?.is_none() {
        if Instant::now() >= deadline {
            child.kill()?;
            child.wait()?;
            break;
        }
        thread::sleep(POLL_INTERVAL);
    }
    Ok(())
}

pub fn start(state: &Path, shutdown: &AtomicBool) -> Result<()> {
    core::prepare(state)?;
    let _lifecycle = lifecycle_lock(state)?;
    let mut child = if dashboard_locked(state)? {
        None
    } else {
        TcpListener::bind(("127.0.0.1", web::PORT))
            .with_context(|| format!("Dashboard port {} is unavailable", web::PORT))?;
        Some(spawn_dashboard(state)?)
    };
    let result = wait_for_dashboard(state, child.as_mut(), shutdown)
        .and_then(|()| core::node_start(state, Chain::Mainnet));
    if let Err(error) = result {
        if let Some(child) = &mut child {
            if let Err(cleanup) = stop_child(child) {
                return Err(error.context(format!("Dashboard cleanup failed: {cleanup:#}")));
            }
        }
        return Err(error);
    }
    println!("Dashboard · {}", web::URL);
    Ok(())
}

fn stop_dashboard(state: &Path) -> Result<()> {
    let deadline = Instant::now() + STOP_TIMEOUT;
    let mut acknowledged = false;
    while dashboard_locked(state)? {
        if !acknowledged {
            let request = request_dashboard(state, CONTROL_STOP);
            // A successful response means the server is draining the mining session.
            acknowledged = request.is_ok();
            if Instant::now() >= deadline {
                request.context("Could not stop the dashboard")?;
            }
        }
        ensure!(
            Instant::now() < deadline,
            "Dashboard is still shutting down"
        );
        thread::sleep(POLL_INTERVAL);
    }
    Ok(())
}

pub fn stop(state: &Path) -> Result<()> {
    if !state.is_dir() {
        return Ok(());
    }
    let _lifecycle = lifecycle_lock(state)?;
    stop_dashboard(state)?;
    core::node_stop(state, Chain::Mainnet)
}

pub fn status(state: &Path) -> Result<()> {
    if dashboard_locked(state)? {
        match request_dashboard(state, CONTROL_STATUS) {
            Ok(()) => println!("dashboard: running · {}", web::URL),
            Err(error) => println!("dashboard: unavailable ({error:#})"),
        }
    } else {
        println!("dashboard: stopped");
    }
    core::node_status(state, Chain::Mainnet)
}

pub fn logs(state: &Path) -> Result<()> {
    core::show_log(&state.join(DASHBOARD_LOG))
}
