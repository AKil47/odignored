//! Ignore-rule evaluation: `.gitignore` + `.onedriveignore`, top-down, pruning ignored dirs.
//!
//! Precedence: every `.onedriveignore` (deepest first) beats every `.gitignore` (deepest first),
//! so a root `.onedriveignore` can re-include (`!foo`) or add to whatever git ignores.

use std::collections::{BTreeSet, HashMap};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use ignore::Match;
use ignore::gitignore::{Gitignore, GitignoreBuilder};

pub const OD_IGNORE: &str = ".onedriveignore";
pub const GIT_IGNORE: &str = ".gitignore";

/// Directories we never descend into while scanning (huge, and not ignored by default).
const NO_DESCEND: &[&str] = &[".git"];

pub fn is_ignore_file(name: &str) -> bool {
    name == OD_IGNORE || name == GIT_IGNORE
}

/// A top-level ignored entry, identified by its (lowercased) base name.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct Entry {
    pub name: String,
    pub is_dir: bool,
}

impl std::fmt::Display for Entry {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{} {}", if self.is_dir { "dir " } else { "file" }, self.name)
    }
}

/// The names handed to OneDrive's `EnableODIgnore*ListFromGPO` policies.
pub type Blocks = BTreeSet<Entry>;

struct Layer {
    od: Gitignore,
    git: Gitignore,
}

fn load(dir: &Path, file: &str) -> Gitignore {
    let path = dir.join(file);
    if !path.is_file() {
        return Gitignore::empty();
    }
    let mut b = GitignoreBuilder::new(dir);
    if let Some(err) = b.add(&path) {
        tracing::warn!(%err, path = %path.display(), "bad ignore pattern (skipped)");
    }
    b.build().unwrap_or_else(|_| Gitignore::empty())
}

fn ignored(layers: &[Arc<Layer>], path: &Path, is_dir: bool) -> bool {
    let od = layers.iter().rev().map(|l| &l.od);
    let git = layers.iter().rev().map(|l| &l.git);
    for gi in od.chain(git) {
        match gi.matched(path, is_dir) {
            Match::Ignore(_) => return true,
            Match::Whitelist(_) => return false,
            Match::None => {}
        }
    }
    false
}

/// Result of a full scan. OneDrive's policy matches by name only, so a name that is ignored in one place
/// but *kept* (not ignored) somewhere else must not be blocked, or the kept copies would stop syncing.
#[derive(Debug, Default)]
pub struct Scan {
    pub ignored: Blocks,
    pub kept: Blocks,
}

impl Scan {
    pub fn extend(&mut self, other: Scan) {
        self.ignored.extend(other.ignored);
        self.kept.extend(other.kept);
    }

    /// Names that are ignored somewhere but also kept elsewhere (reported, never blocked).
    pub fn conflicts(&self) -> Blocks {
        &self.ignored & &self.kept
    }

    pub fn blocks(&self) -> Blocks {
        &self.ignored - &self.kept
    }
}

/// Evaluates ignore rules under one OneDrive root. Parsed rule files are cached per directory.
pub struct Matcher {
    root: PathBuf,
    cache: HashMap<PathBuf, Arc<Layer>>,
}

impl Matcher {
    pub fn new(root: PathBuf) -> Self {
        Self { root, cache: HashMap::new() }
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Drop cached rule files (call when any `.gitignore`/`.onedriveignore` changed).
    pub fn invalidate(&mut self) {
        self.cache.clear();
    }

    fn layer(&mut self, dir: &Path) -> Arc<Layer> {
        self.cache
            .entry(dir.to_path_buf())
            .or_insert_with(|| Arc::new(Layer { od: load(dir, OD_IGNORE), git: load(dir, GIT_IGNORE) }))
            .clone()
    }

    /// The outermost ignored entry on the way to `path` (which may itself be it).
    pub fn top_ignored(&mut self, path: &Path, is_dir: bool) -> Option<Entry> {
        let rel = path.strip_prefix(&self.root).ok()?;
        let comps: Vec<_> = rel.components().collect();
        let mut cur = self.root.clone();
        let mut layers = vec![self.layer(&cur)];
        for (i, comp) in comps.iter().enumerate() {
            let last = i + 1 == comps.len();
            let child_is_dir = !last || is_dir;
            cur.push(comp);
            if ignored(&layers, &cur, child_is_dir) {
                let name = comp.as_os_str().to_string_lossy().to_lowercase();
                return Some(Entry { name, is_dir: child_is_dir });
            }
            if !last {
                layers.push(self.layer(&cur));
            }
        }
        None
    }

    /// Walk the whole root, pruning at the first ignored entry on every branch.
    pub fn scan(&mut self) -> Scan {
        let mut out = Scan::default();
        let root = self.root.clone();
        let mut layers = vec![self.layer(&root)];
        self.walk(&root, &mut layers, &mut out);
        out
    }

    fn walk(&mut self, dir: &Path, layers: &mut Vec<Arc<Layer>>, out: &mut Scan) {
        let Ok(rd) = std::fs::read_dir(dir) else { return };
        for de in rd.flatten() {
            let Ok(ft) = de.file_type() else { continue };
            if ft.is_symlink() {
                continue;
            }
            let path = de.path();
            let is_dir = ft.is_dir();
            if ignored(layers, &path, is_dir) {
                let name = de.file_name().to_string_lossy().to_lowercase();
                out.ignored.insert(Entry { name, is_dir });
            } else {
                out.kept.insert(Entry { name: de.file_name().to_string_lossy().to_lowercase(), is_dir });
                if is_dir && !NO_DESCEND.iter().any(|n| de.file_name().eq_ignore_ascii_case(n)) {
                    layers.push(self.layer(&path));
                    self.walk(&path, layers, out);
                    layers.pop();
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    #[test]
    fn onedriveignore_overrides_gitignore_and_prunes() {
        let d = std::env::temp_dir().join(format!("odi-test-{}", std::process::id()));
        let _ = fs::remove_dir_all(&d);
        fs::create_dir_all(d.join("node_modules/pkg")).unwrap();
        fs::create_dir_all(d.join("src")).unwrap();
        for f in ["node_modules/pkg/a.js", "src/a.log", "src/keep.log", "src/b.tmp2"] {
            fs::write(d.join(f), "").unwrap();
        }
        fs::write(d.join(".gitignore"), "node_modules/
*.log
").unwrap();
        fs::write(d.join(".onedriveignore"), "!keep.log
*.tmp2
").unwrap();

        let mut m = Matcher::new(d.clone());
        let b: Vec<_> = m.scan().blocks().iter().map(ToString::to_string).collect();
        assert_eq!(b, ["file a.log", "file b.tmp2", "dir  node_modules"]);

        let e = m.top_ignored(&d.join("node_modules/pkg/a.js"), false).unwrap();
        assert_eq!(e, Entry { name: "node_modules".into(), is_dir: true });
        assert!(m.top_ignored(&d.join("src/keep.log"), false).is_none());
        fs::remove_dir_all(&d).unwrap();
    }
}
