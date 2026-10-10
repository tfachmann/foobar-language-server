//! Shared workspace state: one immutable snapshot per known file, plus the include graph.
//!
//! Open buffers and files read from disk share the same map, so every cross-file lookup is
//! plain synchronous code that never touches a document lock.

use std::collections::{HashSet, VecDeque};
use std::ffi::OsStr;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{Arc, OnceLock};
use std::time::SystemTime;

use dashmap::DashMap;
use ropey::Rope;
use tower_lsp_server::ls_types::{Diagnostic, Uri};
use tree_sitter::Tree;

use crate::diagnostics::collect_parse_errors;
use crate::document::Document;
use crate::symbol::{DefineEntry, IncludeRef, SymbolIndex, TemplateDef};
use crate::syntax;
use crate::text::{path_to_uri, uri_to_path};

/// What other files may know about a file. Replaced as a whole after every parse.
pub struct FileSnapshot {
    /// The editor's URI for open buffers, one built from the path for disk files.
    pub uri: Uri,
    pub origin: Origin,
    pub symbols: SymbolIndex,
    pub parse_diags: Vec<Diagnostic>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Origin {
    /// An editor buffer. Authoritative: never replaced by the disk version while open.
    Open,
    Disk {
        mtime: Option<SystemTime>,
    },
}

impl FileSnapshot {
    pub fn new(uri: Uri, origin: Origin, tree: &Tree, rope: &Rope) -> Self {
        let mut parse_diags = Vec::new();
        collect_parse_errors(tree, rope, &mut parse_diags);
        Self {
            uri,
            origin,
            symbols: SymbolIndex::from_tree(tree, rope),
            parse_diags,
        }
    }
}

/// Outcome of resolving a raw include path.
#[derive(Clone, Debug)]
pub struct ResolvedInclude {
    /// Path joined relative to the including file's directory; may or may not exist.
    pub path: PathBuf,
    /// True if `path.exists()` at resolution time.
    pub exists: bool,
}

/// A file reached by following `include`s.
pub struct Reached {
    pub path: PathBuf,
    /// `None` if the file is missing or unreadable.
    pub file: Option<Arc<FileSnapshot>>,
}

#[derive(Default)]
pub struct Workspace {
    /// Every known file by [`file_key`]: open buffers and files read from disk.
    pub files: DashMap<PathBuf, Arc<FileSnapshot>>,
    /// Reverse include graph: file path -> open docs that (transitively) include it.
    pub dependents: DashMap<PathBuf, HashSet<Uri>>,
    /// Workspace root directories captured at LSP `initialize`.
    pub roots: OnceLock<Vec<PathBuf>>,
}

/// The single map key for a file: its canonical path.
///
/// A file that doesn't exist yet (new, unsaved) gets its canonical directory plus its name, so the
/// key stays the same once the file is saved.
pub fn file_key(path: &Path) -> PathBuf {
    if let Ok(canonical) = path.canonicalize() {
        return canonical;
    }
    match (
        path.parent().and_then(|d| d.canonicalize().ok()),
        path.file_name(),
    ) {
        (Some(dir), Some(name)) => dir.join(name),
        _ => path.to_path_buf(),
    }
}

/// [`file_key`] for a `file://` URI.
pub fn uri_key(uri: &Uri) -> Option<PathBuf> {
    uri_to_path(uri).map(|p| file_key(&p))
}

impl Workspace {
    pub fn set_roots(&self, roots: Vec<PathBuf>) {
        let _ = self.roots.set(roots.iter().map(|r| file_key(r)).collect());
    }

    /// Is `key` inside one of the workspace roots?
    fn in_roots(&self, key: &Path) -> bool {
        self.roots
            .get()
            .is_some_and(|roots| roots.iter().any(|r| key.starts_with(r)))
    }

    /// The snapshot for `path`. Clones the `Arc` so no DashMap guard outlives the call.
    pub fn file(&self, path: &Path) -> Option<Arc<FileSnapshot>> {
        self.files.get(path).map(|e| e.value().clone())
    }

    /// All snapshots, cloned out of the map so callers can load files while iterating.
    pub fn all_files(&self) -> Vec<Arc<FileSnapshot>> {
        self.files.iter().map(|e| e.value().clone()).collect()
    }

    /// Make an open buffer's latest parse visible to other files.
    ///
    /// Call while holding the document's write lock so snapshots land in edit order.
    pub fn publish(&self, doc: &Document) {
        if let Some(key) = &doc.key {
            self.files.insert(key.clone(), doc.snapshot.clone());
        }
    }

    /// The editor closed `doc`: drop it from the include graph and fall back to the disk version.
    pub fn close(&self, doc: &Document) {
        let Some(key) = &doc.key else {
            return;
        };
        // Only remove our own snapshot: a quick reopen or a second alias may own the entry by now.
        if self
            .files
            .remove_if(key, |_, f| Arc::ptr_eq(f, &doc.snapshot))
            .is_none()
        {
            return;
        }
        self.forget_doc(&doc.uri);
        // Files outside the roots were never part of the pool; don't add them now.
        if self.in_roots(key) {
            self.load(key);
        }
    }

    /// Recursively walk every stored root, loading each `*.foobar` file.
    pub fn scan_workspace(&self) {
        let Some(roots) = self.roots.get() else {
            return;
        };
        for path in walk_foobar_files(roots) {
            self.load(&file_key(&path));
        }
    }

    /// The snapshot for `path`, (re)read from disk unless it is open or the cached copy is current.
    pub fn load(&self, path: &Path) -> Option<Arc<FileSnapshot>> {
        let mtime = fs::metadata(path).and_then(|m| m.modified()).ok();
        if let Some(cached) = self.file(path)
            && (cached.origin == Origin::Open || cached.origin == Origin::Disk { mtime })
        {
            return Some(cached);
        }
        // Read and parse outside the map; the file may get opened meanwhile.
        let Some(fresh) = load_disk_file(path, mtime).map(Arc::new) else {
            // Deleted or unreadable: forget the cached copy, but never an open buffer.
            self.files.remove_if(path, |_, f| f.origin != Origin::Open);
            return None;
        };
        let entry = self
            .files
            .entry(path.to_path_buf())
            .and_modify(|e| {
                if e.origin != Origin::Open {
                    *e = fresh.clone();
                }
            })
            .or_insert(fresh);
        Some(entry.value().clone())
    }

    /// Resolve a raw include path string against a base `file://` URI.
    ///
    /// Returns `None` for non-file base URIs.
    pub fn resolve(&self, base: &Uri, raw: &str) -> Option<ResolvedInclude> {
        let base_path = uri_to_path(base)?;
        let path = file_key(&base_path.parent()?.join(raw.trim()));
        let exists = path.exists();
        Some(ResolvedInclude { path, exists })
    }

    /// Breadth-first walk over everything transitively included from `base`.
    ///
    /// Each path is visited once, so include cycles terminate.
    pub fn reachable(&self, base: &Uri, includes: &[IncludeRef]) -> Vec<Reached> {
        let mut out: Vec<Reached> = Vec::new();
        let mut visited: HashSet<PathBuf> = HashSet::new();
        let mut queue: VecDeque<(Uri, Vec<IncludeRef>)> = VecDeque::new();
        queue.push_back((base.clone(), includes.to_vec()));

        while let Some((base, includes)) = queue.pop_front() {
            for inc in includes {
                let Some(resolved) = self.resolve(&base, &inc.raw_path) else {
                    continue;
                };
                if !visited.insert(resolved.path.clone()) {
                    continue;
                }
                // Also for missing files: evicts a deleted file's stale snapshot.
                let file = self.load(&resolved.path);
                if let Some(f) = &file {
                    queue.push_back((f.uri.clone(), f.symbols.includes.clone()));
                }
                out.push(Reached {
                    path: resolved.path,
                    file,
                });
            }
        }
        out
    }

    /// Load all transitive includes of an open file and record it as their dependent.
    pub fn refresh_includes_for(&self, snapshot: &FileSnapshot) {
        let reached = self.reachable(&snapshot.uri, &snapshot.symbols.includes);
        self.forget_doc(&snapshot.uri);
        for r in reached {
            self.dependents
                .entry(r.path)
                .or_default()
                .insert(snapshot.uri.clone());
        }
    }

    /// Drop `uri` from dependents entirely (e.g. on `did_close`).
    pub fn forget_doc(&self, uri: &Uri) {
        for mut entry in self.dependents.iter_mut() {
            entry.value_mut().remove(uri);
        }
        self.dependents.retain(|_, set| !set.is_empty());
    }

    /// Return the set of open-doc URIs that transitively include `path`.
    pub fn dependents_of(&self, path: &Path) -> Vec<Uri> {
        self.dependents
            .get(path)
            .map(|s| s.iter().cloned().collect())
            .unwrap_or_default()
    }

    /// Search `from` first, then everything it transitively includes.
    ///
    /// `pick` extracts the wanted symbol from one file's index.
    pub fn lookup<T>(
        &self,
        from: &Arc<FileSnapshot>,
        pick: impl Fn(&SymbolIndex) -> Option<T>,
    ) -> Option<(Arc<FileSnapshot>, T)> {
        if let Some(hit) = pick(&from.symbols) {
            return Some((from.clone(), hit));
        }
        self.reachable(&from.uri, &from.symbols.includes)
            .into_iter()
            .find_map(|r| {
                let file = r.file?;
                let hit = pick(&file.symbols)?;
                Some((file, hit))
            })
    }

    /// Every file whose symbols `from` can use, nearest first: itself, its includes, the pool.
    pub fn visible(&self, from: &Arc<FileSnapshot>) -> Vec<Arc<FileSnapshot>> {
        let includes = self
            .reachable(&from.uri, &from.symbols.includes)
            .into_iter()
            .filter_map(|r| r.file);
        std::iter::once(from.clone())
            .chain(includes)
            .chain(self.all_files())
            .collect()
    }

    /// Search every known file, whether it is included or not.
    pub fn lookup_anywhere<T>(
        &self,
        pick: impl Fn(&SymbolIndex) -> Option<T>,
    ) -> Option<(Arc<FileSnapshot>, T)> {
        self.all_files().into_iter().find_map(|file| {
            let hit = pick(&file.symbols)?;
            Some((file, hit))
        })
    }

    /// The define for `name`, from the file itself or its transitive includes.
    pub fn lookup_define(
        &self,
        from: &Arc<FileSnapshot>,
        name: &str,
    ) -> Option<(Arc<FileSnapshot>, DefineEntry)> {
        self.lookup(from, |s| s.defines.get(name).cloned())
    }

    /// Does `name` resolve via [`Self::lookup_define`] or anywhere else in the workspace?
    pub fn has_define(&self, from: &Arc<FileSnapshot>, name: &str) -> bool {
        let pick = |s: &SymbolIndex| s.defines.get(name).map(|_| ());
        self.lookup(from, pick).is_some() || self.lookup_anywhere(pick).is_some()
    }

    /// The template `name`, from the file itself, its includes, or anywhere in the workspace.
    pub fn lookup_template(
        &self,
        from: &Arc<FileSnapshot>,
        name: &str,
    ) -> Option<(Arc<FileSnapshot>, TemplateDef)> {
        let pick = |s: &SymbolIndex| s.templates.get(name).cloned();
        self.lookup(from, pick)
            .or_else(|| self.lookup_anywhere(pick))
    }

    /// Workspace-wide count of `%(name)` references.
    pub fn count_refs(&self, name: &str) -> usize {
        self.all_files()
            .iter()
            .flat_map(|f| &f.symbols.var_refs)
            .filter(|(k, _)| k == name)
            .count()
    }

    /// Number of defines in a known file. `None` if the file isn't loaded.
    pub fn defines_count_at(&self, path: &Path) -> Option<usize> {
        self.file(path).map(|f| f.symbols.defines.len())
    }
}

/// Read + parse + index a file from disk. `None` on any IO/parse failure.
fn load_disk_file(path: &Path, mtime: Option<SystemTime>) -> Option<FileSnapshot> {
    let text = fs::read_to_string(path).ok()?;
    Some(FileSnapshot::new(
        path_to_uri(path)?,
        Origin::Disk { mtime },
        &syntax::parse(&text),
        &Rope::from_str(&text),
    ))
}

/// Iterative DFS over `roots`, returning every `*.foobar` file found.
pub fn walk_foobar_files(roots: &[PathBuf]) -> Vec<PathBuf> {
    const SKIP_DIRS: &[&str] = &["node_modules", "target"];

    let mut out: Vec<PathBuf> = Vec::new();
    let mut visited: HashSet<PathBuf> = HashSet::new();
    let mut stack: Vec<PathBuf> = Vec::new();

    for root in roots {
        let canon = root.canonicalize().unwrap_or_else(|_| root.clone());
        if visited.insert(canon.clone()) {
            stack.push(canon);
        }
    }

    while let Some(dir) = stack.pop() {
        let Ok(entries) = fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            let Some(name) = path.file_name().and_then(OsStr::to_str) else {
                continue;
            };
            if name.starts_with('.') {
                continue;
            }
            let Ok(ty) = entry.file_type() else {
                continue;
            };
            if ty.is_dir() {
                if SKIP_DIRS.contains(&name) {
                    continue;
                }
                let canon = path.canonicalize().unwrap_or_else(|_| path.clone());
                if visited.insert(canon.clone()) {
                    stack.push(canon);
                }
            } else if ty.is_file() && path.extension() == Some(OsStr::new("foobar")) {
                out.push(path);
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::document::Document;
    use tempfile::TempDir;

    #[test]
    fn open_buffer_wins_until_closed() {
        let ws = Workspace::default();
        let dir = TempDir::new().unwrap();
        let inc_path = dir.path().join("inc.foobar");
        let main_path = dir.path().join("main.foobar");
        fs::write(&inc_path, "defines\nname: \"disk\"\n").unwrap();
        fs::write(&main_path, "include inc.foobar\n").unwrap();
        let main = Document::from_text(path_to_uri(&main_path).unwrap(), "include inc.foobar\n");
        let name = |ws: &Workspace| {
            let (file, entry) = ws.lookup_define(&main.snapshot, "name").unwrap();
            (entry.value_text, file.origin == Origin::Open)
        };

        // inc.foobar is open with unsaved edits. A workspace scan must not replace them.
        let inc_uri = path_to_uri(&inc_path).unwrap();
        let inc = Document::from_text(inc_uri.clone(), "defines\nname: \"buffer\"\n");
        ws.publish(&inc);
        ws.set_roots(vec![dir.path().to_path_buf()]);
        ws.scan_workspace();
        assert_eq!(name(&ws), ("buffer".to_string(), true));

        // Quick close + reopen: closing the old document must not drop the reopened buffer.
        let reopened = Document::from_text(inc_uri, "defines\nname: \"reopened\"\n");
        ws.publish(&reopened);
        ws.close(&inc);
        assert_eq!(name(&ws), ("reopened".to_string(), true));

        // Closing the buffer falls back to the file on disk.
        ws.close(&reopened);
        assert_eq!(name(&ws), ("disk".to_string(), false));
    }

    #[test]
    fn closed_and_deleted_files_leave_the_pool() {
        let ws = Workspace::default();
        let root = TempDir::new().unwrap();
        let outside = TempDir::new().unwrap();
        ws.set_roots(vec![root.path().to_path_buf()]);

        // A file outside the roots is forgotten when its buffer closes.
        let scratch = outside.path().join("scratch.foobar");
        fs::write(&scratch, "defines\nfoo: \"1\"\n").unwrap();
        let doc = Document::from_text(path_to_uri(&scratch).unwrap(), "defines\nfoo: \"1\"\n");
        ws.publish(&doc);
        ws.close(&doc);
        assert!(ws.files.is_empty());

        // A deleted include is evicted the next time the include graph is walked.
        let inc = root.path().join("inc.foobar");
        fs::write(&inc, "defines\nx: \"1\"\n").unwrap();
        let main = Document::from_text(
            path_to_uri(&root.path().join("main.foobar")).unwrap(),
            "include inc.foobar\n",
        );
        ws.refresh_includes_for(&main.snapshot);
        assert_eq!(ws.files.len(), 1);
        fs::remove_file(&inc).unwrap();
        ws.refresh_includes_for(&main.snapshot);
        assert!(ws.files.is_empty());
    }

    #[test]
    fn dependents_follow_edits() {
        let ws = Workspace::default();
        let dir = TempDir::new().unwrap();
        let inc_path = dir.path().join("inc.foobar");
        fs::write(&inc_path, "defines\nx: \"1\"\n").unwrap();
        let inc_key = file_key(&inc_path);
        let main_uri = path_to_uri(&dir.path().join("main.foobar")).unwrap();

        let v1 = Document::from_text(main_uri.clone(), "include inc.foobar\n");
        ws.refresh_includes_for(&v1.snapshot);
        assert_eq!(ws.dependents_of(&inc_key), vec![main_uri.clone()]);

        // The include is removed: inc.foobar no longer has main as a dependent.
        let v2 = Document::from_text(main_uri, "defines\ny: \"2\"\n");
        ws.refresh_includes_for(&v2.snapshot);
        assert!(ws.dependents_of(&inc_key).is_empty());
    }

    #[test]
    fn has_define_falls_back_to_workspace_pool() {
        let ws = Workspace::default();
        let dir = TempDir::new().unwrap();
        // b.foobar uses %(foo) without including a.foobar, which defines it.
        fs::write(dir.path().join("a.foobar"), "defines\nfoo: \"hello\"\n").unwrap();
        let b = Document::from_text(
            path_to_uri(&dir.path().join("b.foobar")).unwrap(),
            "defines\nbar: \"%(foo) %(ghost)\"\n",
        );

        ws.set_roots(vec![dir.path().to_path_buf()]);
        ws.scan_workspace();

        assert!(ws.has_define(&b.snapshot, "foo"));
        assert!(!ws.has_define(&b.snapshot, "ghost"));
    }

    #[test]
    fn scan_workspace_skips_hidden_and_build_dirs() {
        let ws = Workspace::default();
        let dir = TempDir::new().unwrap();
        for sub in [".git", "node_modules", "target", "nested"] {
            fs::create_dir_all(dir.path().join(sub)).unwrap();
            fs::write(dir.path().join(sub).join("x.foobar"), "").unwrap();
        }

        ws.set_roots(vec![dir.path().to_path_buf()]);
        ws.scan_workspace();

        assert_eq!(ws.files.len(), 1);
        assert!(
            ws.files
                .contains_key(&file_key(&dir.path().join("nested/x.foobar")))
        );
    }
}
