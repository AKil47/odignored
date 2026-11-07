//! Stopping and restarting the OneDrive sync client.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use deelevate::{Command, Token};
use sysinfo::{ProcessRefreshKind, ProcessesToUpdate, System, UpdateKind};

const PROC: &str = "OneDrive.exe";

/// Proof that we stopped OneDrive, and the only way to start it again (consumed on use).
/// Remembers where OneDrive was running from, so the same install is relaunched.
pub struct Stopped {
    exe: Option<PathBuf>,
}

/// Kill every running OneDrive.exe immediately. `None` if it wasn't running.
pub fn stop() -> Option<Stopped> {
    let mut sys = System::new().ok()?;
    // Only the exe path is needed; the default refresh also samples CPU/disk, which is slow.
    sys.refresh_processes_specifics(
        ProcessesToUpdate::All,
        true,
        ProcessRefreshKind::nothing().with_exe(UpdateKind::OnlyIfNotSet),
    );
    let (mut killed, mut exe) = (false, None);
    for p in sys.processes_by_exact_name(PROC.as_ref()) {
        killed |= terminate(p.pid().as_u32());
        exe = exe.or_else(|| p.exe().map(Path::to_path_buf));
    }
    killed.then_some(Stopped { exe })
}

impl Stopped {
    /// Start OneDrive *de-elevated* and quietly (`/background`, so no Explorer window pops up).
    /// We run as admin and OneDrive refuses to run elevated, so launch it with the desktop shell's token.
    pub fn relaunch(self) -> Result<()> {
        let exe = self.exe.or_else(default_exe).context("cannot locate OneDrive.exe")?;
        let token = Token::with_shell_process()?;
        let mut cmd = Command::with_environment_for_token(&token)?;
        cmd.set_argv(vec![exe.into(), "/background".into()]);
        cmd.hide_window();
        cmd.spawn_with_token(&token).context("launching OneDrive")?;
        Ok(())
    }
}

/// Kill a process immediately. (sysinfo's `kill()` shells out to `taskkill.exe`, ~0.5s per process.)
pub fn terminate(pid: u32) -> bool {
    use windows_sys::Win32::Foundation::CloseHandle;
    use windows_sys::Win32::System::Threading::{OpenProcess, PROCESS_TERMINATE, TerminateProcess};
    // SAFETY: the handle is checked for null, used once, and closed.
    unsafe {
        let h = OpenProcess(PROCESS_TERMINATE, 0, pid);
        if h.is_null() {
            return false;
        }
        let ok = TerminateProcess(h, 1) != 0;
        CloseHandle(h);
        ok
    }
}

/// Sync roots of every account registered with the OneDrive client (personal and business).
pub fn account_folders() -> Vec<PathBuf> {
    use winreg::{RegKey, enums::HKEY_CURRENT_USER};
    let Ok(accounts) = RegKey::predef(HKEY_CURRENT_USER).open_subkey(r"Software\Microsoft\OneDrive\Accounts") else {
        return Vec::new();
    };
    accounts
        .enum_keys()
        .flatten()
        .filter_map(|k| accounts.open_subkey(k).ok()?.get_value::<String, _>("UserFolder").ok())
        .map(PathBuf::from)
        .collect()
}

fn default_exe() -> Option<PathBuf> {
    let per_user = std::env::var_os("LOCALAPPDATA").map(|l| Path::new(&l).join(r"Microsoft\OneDrive\OneDrive.exe"));
    let machine = PathBuf::from(r"C:\Program Files\Microsoft OneDrive\OneDrive.exe");
    per_user.into_iter().chain([machine]).find(|p| p.is_file())
}
