use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

#[allow(dead_code)]
pub const SESSION_STATE_RELATIVE_PATH: &str = "run/qgui/session.json";

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GuiSessionState {
    pub display: String,
    pub res: String,
    pub depth: u16,
    pub backend_bind: String,
    pub backend_port: u16,
    pub dbus_addr: String,
    pub dbus_socket: String,
    pub xdg_runtime_dir: String,
    pub auth_username: String,
    pub auth_password: String,
}

#[allow(dead_code)]
pub fn session_state_path_in_rootfs(rootfs_path: &str) -> PathBuf {
    Path::new(rootfs_path).join(SESSION_STATE_RELATIVE_PATH)
}

#[allow(dead_code)]
pub fn load_session_state_from_rootfs(rootfs_path: &str) -> Result<GuiSessionState> {
    let path = session_state_path_in_rootfs(rootfs_path);
    let data = std::fs::read(&path).with_context(|| format!("read {}", path.display()))?;
    serde_json::from_slice(&data).with_context(|| format!("parse {}", path.display()))
}
