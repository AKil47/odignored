mod daemon;
mod onedrive;
mod policy;
mod rules;
mod tray;

use std::os::windows::process::CommandExt;
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::time::Duration;

use anyhow::{Context, Result, bail};
use deelevate::{PrivilegeLevel, Token};
use clap::{Args, Parser, Subcommand};
use sysinfo::{ProcessesToUpdate, System};

const DETACHED_PROCESS: u32 = 0x0000_0008;
const CREATE_NO_WINDOW: u32 = 0x0800_0000;

#[derive(Parser)]
#[command(version, about = "Give OneDrive .gitignore-like powers")]
struct Cli {
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Watch in the foreground (must be elevated to write the policy)
    Run(RunArgs),
    /// Start `run` as a detached background process
    Daemon(RunArgs),
    /// Stop a running background daemon
    Stop,
    /// Dry run: print what would be blocked
    Scan {
        #[arg(short, long)]
        root: Vec<PathBuf>,
        #[arg(short = 'x', long)]
        exclude: Vec<PathBuf>,
    },
}

#[derive(Args)]
struct RunArgs {
    /// OneDrive folder(s) to watch [default: every account registered with OneDrive]
    #[arg(short, long)]
    root: Vec<PathBuf>,
    /// Folder(s) to leave alone (e.g. a stale account's folder)
    #[arg(short = 'x', long)]
    exclude: Vec<PathBuf>,
    /// Quiet period after the last newly blocked entry before OneDrive is relaunched
    #[arg(short, long, default_value = "30s", value_parser = humantime::parse_duration)]
    cooldown: Duration,
    /// Don't show a system tray icon (run headless)
    #[arg(long)]
    no_tray: bool,
    #[arg(long, hide = true)]
    log_file: Option<PathBuf>,
}

fn roots(explicit: Vec<PathBuf>, exclude: &[PathBuf]) -> Result<Vec<PathBuf>> {
    let mut roots = explicit;
    if roots.is_empty() {
        roots = onedrive::account_folders();
    }
    roots.retain(|r| r.is_dir() && !exclude.iter().any(|x| x.as_os_str().eq_ignore_ascii_case(r.as_os_str())));
    roots.sort();
    roots.dedup();
    if roots.is_empty() {
        bail!("no OneDrive folder found; pass --root");
    }
    Ok(roots)
}

fn init_logging(file: Option<&PathBuf>) -> Result<()> {
    let b = tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()));
    match file {
        Some(p) => {
            if let Some(dir) = p.parent() {
                std::fs::create_dir_all(dir)?;
            }
            let f = std::fs::OpenOptions::new().create(true).append(true).open(p)?;
            b.with_ansi(false).with_writer(std::sync::Mutex::new(f)).init();
        }
        None => b.init(),
    }
    Ok(())
}

/// Re-run this exe through a UAC prompt unless already elevated. Returns true if the caller should exit.
fn elevate() -> Result<bool> {
    let token = Token::with_current_process()?;
    if token.privilege_level()? != PrivilegeLevel::NotPrivileged {
        return Ok(false);
    }
    let mut cmd = deelevate::Command::with_environment_for_token(&token)?;
    cmd.set_command_from_current_process()?;
    cmd.hide_window();
    cmd.shell_execute("runas").context("elevation was declined or failed")?;
    Ok(true)
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    // Everything except a dry-run scan needs admin (HKLM writes, killing the elevated daemon).
    if !matches!(cli.cmd, Cmd::Scan { .. }) && elevate()? {
        println!("relaunched elevated");
        return Ok(());
    }
    match cli.cmd {
        Cmd::Run(a) => {
            init_logging(a.log_file.as_ref())?;
            let roots = roots(a.root, &a.exclude)?;
            if a.no_tray {
                daemon::start(roots, a.cooldown, || {}).1.join().map_err(|_| anyhow::anyhow!("daemon panicked"))?
            } else {
                tray::run(roots, a.cooldown, a.log_file)
            }
        }
        Cmd::Daemon(a) => {
            let roots = roots(a.root, &a.exclude)?;
            let log = a.log_file.unwrap_or_else(|| {
                let base = std::env::var_os("LOCALAPPDATA").map(PathBuf::from).unwrap_or_default();
                base.join("odignored").join("odignored.log")
            });
            let mut cmd = Command::new(std::env::current_exe()?);
            cmd.arg("run").arg("--cooldown").arg(humantime::format_duration(a.cooldown).to_string());
            for r in &roots {
                cmd.arg("--root").arg(r);
            }
            let child = cmd
                .arg("--log-file")
                .arg(&log)
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .creation_flags(DETACHED_PROCESS | CREATE_NO_WINDOW)
                .spawn()?;
            println!("odignored running (pid {}), logging to {}", child.id(), log.display());
            Ok(())
        }
        Cmd::Stop => {
            let mut sys = System::new()?;
            sys.refresh_processes(ProcessesToUpdate::All, true);
            let me = sysinfo::get_current_pid().ok();
            let n = sys
                .processes_by_exact_name("odignored.exe".as_ref())
                .filter(|p| Some(p.pid()) != me)
                .filter(|p| onedrive::terminate(p.pid().as_u32()))
                .count();
            println!("stopped {n} process(es)");
            Ok(())
        }
        Cmd::Scan { root, exclude } => {
            for r in roots(root, &exclude)? {
                let scan = rules::Matcher::new(r.clone()).scan();
                let (blocks, skipped) = (scan.blocks(), scan.conflicts());
                println!("{}: would block {} name(s)", r.display(), blocks.len());
                blocks.iter().for_each(|e| println!("  {e}"));
                skipped.iter().for_each(|e| println!("  skip {e}  (also exists unignored)"));
            }
            Ok(())
        }
    }
}
