//! The watch loop: detect new ignorable entries, update policy, bounce OneDrive with a cooldown.

use std::path::{Path, PathBuf};
use std::sync::mpsc;
use std::time::{Duration, Instant};

use anyhow::Result;
use notify::RecursiveMode;
use notify_debouncer_mini::{DebouncedEvent, new_debouncer};

use crate::onedrive::{self, Stopped};
use crate::policy;
use crate::rules::{Blocks, Entry, Matcher, Scan, is_ignore_file};

/// OneDrive is never kept down longer than this many cooldowns, even under constant churn.
const MAX_DOWNTIME_FACTOR: u32 = 5;
/// Filesystem events are coalesced over this window (git checkout, builds, saves).
const BATCH_WINDOW: Duration = Duration::from_millis(100);

pub struct Daemon {
    matchers: Vec<Matcher>,
    blocks: Blocks,
    kept: Blocks,
    cooldown: Duration,
    /// Set while we have OneDrive stopped.
    down: Option<Down>,
}

struct Down {
    since: Instant,
    relaunch_at: Instant,
    stopped: Stopped,
}

/// Messages for the daemon thread.
pub enum Msg {
    Fs(Vec<DebouncedEvent>),
    Quit,
}

/// Spawn the daemon on its own thread; send `Msg::Quit` to stop it (OneDrive is relaunched if we had it down).
/// `on_exit` runs on that thread once the daemon has stopped, for any reason.
pub fn start(
    roots: Vec<PathBuf>,
    cooldown: Duration,
    on_exit: impl FnOnce() + Send + 'static,
) -> (mpsc::Sender<Msg>, std::thread::JoinHandle<Result<()>>) {
    let (tx, rx) = mpsc::channel();
    let fs_tx = tx.clone();
    let handle = std::thread::spawn(move || {
        let res = run(roots, cooldown, fs_tx, rx);
        if let Err(e) = &res {
            tracing::error!("daemon stopped: {e:#}");
        }
        on_exit();
        res
    });
    (tx, handle)
}

fn run(roots: Vec<PathBuf>, cooldown: Duration, tx: mpsc::Sender<Msg>, rx: mpsc::Receiver<Msg>) -> Result<()> {
    let mut debouncer = new_debouncer(BATCH_WINDOW, move |res: notify_debouncer_mini::DebounceEventResult| match res {
        Ok(events) => drop(tx.send(Msg::Fs(events))),
        Err(e) => tracing::warn!("watch error: {e}"),
    })?;
    for r in &roots {
        debouncer.watcher().watch(r, RecursiveMode::Recursive)?;
        tracing::info!(root = %r.display(), "watching");
    }

    let mut d = Daemon {
        matchers: roots.into_iter().map(Matcher::new).collect(),
        blocks: policy::read()?,
        kept: Blocks::default(),
        cooldown,
        down: None,
    };
    d.rescan(true)?;

    loop {
        let timeout = d.down.as_ref().map_or(Duration::from_secs(3600), |x| x.relaunch_at.saturating_duration_since(Instant::now()));
        match rx.recv_timeout(timeout) {
            Ok(Msg::Fs(events)) => d.handle(events)?,
            Ok(Msg::Quit) => return d.shutdown(),
            Err(mpsc::RecvTimeoutError::Timeout) => d.maybe_relaunch(),
            Err(mpsc::RecvTimeoutError::Disconnected) => return Ok(()),
        }
    }
}

impl Daemon {
    fn handle(&mut self, events: Vec<DebouncedEvent>) -> Result<()> {
        let (mut ignore_files_changed, mut maybe_stale) = (false, false);
        let mut found = Blocks::default();
        for path in events.iter().map(|e| &e.path) {
            let name = path.file_name().map(|n| n.to_string_lossy().to_lowercase()).unwrap_or_default();
            if is_ignore_file(&name) {
                ignore_files_changed = true;
            } else if let Ok(md) = std::fs::symlink_metadata(path) {
                if let Some(e) = self.classify(path, md.is_dir()) {
                    found.insert(e);
                }
            } else if [false, true].into_iter().any(|is_dir| self.blocks.contains(&Entry { name: name.clone(), is_dir })) {
                maybe_stale = true; // a blocked name went away; its entry may be stale now
            }
        }
        if ignore_files_changed {
            self.rescan(true)
        } else if maybe_stale && found.is_subset(&self.blocks) {
            self.rescan(false)
        } else if !found.is_subset(&self.blocks) {
            let mut next = self.blocks.clone();
            next.extend(found);
            self.apply(next, false)
        } else {
            Ok(())
        }
    }

    fn classify(&mut self, path: &Path, is_dir: bool) -> Option<Entry> {
        let m = self.matchers.iter_mut().find(|m| path.starts_with(m.root()))?;
        m.top_ignored(path, is_dir).filter(|e| !self.kept.contains(e))
    }

    /// Full recompute; the only path that can also *remove* stale entries.
    fn rescan(&mut self, force_restart: bool) -> Result<()> {
        let mut all = Scan::default();
        for m in &mut self.matchers {
            m.invalidate();
            all.extend(m.scan());
        }
        let conflicts = all.conflicts();
        if !conflicts.is_empty() {
            tracing::info!(names = ?conflicts.iter().map(ToString::to_string).collect::<Vec<_>>(), "not blocking: also used by non-ignored entries");
        }
        let blocks = all.blocks();
        self.kept = all.kept;
        self.apply(blocks, force_restart)
    }

    /// Write the new lists. OneDrive is bounced only if entries were *added* (or `force_restart`):
    /// on pure removals it keeps its already-loaded list until its next restart, so bouncing gains nothing.
    fn apply(&mut self, next: Blocks, force_restart: bool) -> Result<()> {
        if next == self.blocks {
            return Ok(());
        }
        let adds = !next.is_subset(&self.blocks);
        tracing::info!(entries = next.len(), adds, "updating OneDrive exclusion policy");
        policy::write(&next)?;
        self.blocks = next;
        if adds || force_restart {
            let stopped = onedrive::stop();
            let prev = self.down.take();
            let now = Instant::now();
            let since = prev.as_ref().map_or(now, |d| d.since);
            if let Some(stopped) = stopped.or(prev.map(|d| d.stopped)) {
                let relaunch_at = (now + self.cooldown).min(since + self.cooldown * MAX_DOWNTIME_FACTOR);
                self.down = Some(Down { since, relaunch_at, stopped });
                tracing::info!(cooldown = ?self.cooldown, "OneDrive stopped; relaunch deferred");
            }
        }
        Ok(())
    }

    fn shutdown(&mut self) -> Result<()> {
        self.down.take().map_or(Ok(()), |d| d.stopped.relaunch())
    }

    fn maybe_relaunch(&mut self) {
        if let Some(d) = self.down.take_if(|d| d.relaunch_at <= Instant::now()) {
            match d.stopped.relaunch() {
                Ok(()) => tracing::info!("OneDrive relaunched"),
                Err(e) => tracing::error!("{e:#}"),
            }
        }
    }
}
