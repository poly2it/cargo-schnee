//! The project source as a list of files instead of a copy on disk.
//!
//! `add_project_source_to_store` used to copy the git-filtered project into a
//! temporary directory and add that to the store on every edit. A
//! `SourceTree` records the same tree, file by file, with where each file's
//! bytes come from. Its NAR equals the NAR of the copy, so its store path is
//! the path the copy would have had, and the planner can cut per-crate slices
//! from it without the whole tree ever reaching the store.

use crate::nar::{nar_bytes, nar_string};
use anyhow::{Context, Result};
use std::collections::{BTreeMap, BTreeSet, HashSet};
use std::path::{Path, PathBuf};

/// Where the bytes of one file of a [`SourceTree`] come from.
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum FileSource {
    /// A file on disk, read through symlinks as `std::fs::copy` does.
    Disk(PathBuf),
    /// Bytes held in memory, such as a manifest with rewritten paths.
    Bytes { data: Vec<u8>, executable: bool },
}

/// A tree of regular files and directories, keyed by path relative to its
/// root.
#[derive(Debug, Clone, Default, PartialEq)]
pub(crate) struct SourceTree {
    files: BTreeMap<PathBuf, FileSource>,
    /// Directories that exist even when no file lies below them.
    dirs: BTreeSet<PathBuf>,
}

impl SourceTree {
    /// The tree that copying `files`, relative to `project_dir`, into an empty
    /// directory produces. Like the copy, it creates the parents of every
    /// entry, and it holds an entry only when that entry is a regular file
    /// after following symlinks.
    pub(crate) fn from_allowed_files(project_dir: &Path, files: &HashSet<PathBuf>) -> Self {
        let mut tree = Self::default();
        for rel in files {
            tree.add_parents(rel);
            let path = project_dir.join(rel);
            if path.is_file() {
                tree.files.insert(rel.clone(), FileSource::Disk(path));
            }
        }
        tree
    }

    /// The tree that copying `dir` without symlinks and without top-level or
    /// nested entries named in `excluded` produces, as `copy_dir_excluding`
    /// does. Every directory it visits stays, even an empty one.
    pub(crate) fn from_dir_excluding(dir: &Path, excluded: &[&str]) -> Result<Self> {
        let mut tree = Self::default();
        tree.walk(dir, Path::new(""), excluded)?;
        Ok(tree)
    }

    fn walk(&mut self, root: &Path, rel: &Path, excluded: &[&str]) -> Result<()> {
        let dir = root.join(rel);
        for entry in std::fs::read_dir(&dir)
            .with_context(|| format!("Failed to read dir {}", dir.display()))?
        {
            let entry = entry?;
            let name = entry.file_name();
            if excluded.iter().any(|e| *e == name.to_string_lossy()) {
                continue;
            }
            let kind = entry.metadata()?.file_type();
            let child = rel.join(&name);
            if kind.is_symlink() {
                continue;
            }
            if kind.is_dir() {
                self.dirs.insert(child.clone());
                self.walk(root, &child, excluded)?;
            } else {
                self.insert_disk(child, entry.path());
            }
        }
        Ok(())
    }

    /// Add the file at `path` on disk as `rel`, with its parents.
    pub(crate) fn insert_disk(&mut self, rel: PathBuf, path: PathBuf) {
        self.add_parents(&rel);
        self.files.insert(rel, FileSource::Disk(path));
    }

    /// Add `data` as `rel`, with its parents.
    pub(crate) fn insert_bytes(&mut self, rel: PathBuf, data: Vec<u8>, executable: bool) {
        self.add_parents(&rel);
        self.files
            .insert(rel, FileSource::Bytes { data, executable });
    }

    /// Add every entry of `other` below `prefix`.
    pub(crate) fn graft(&mut self, prefix: &Path, other: SourceTree) {
        self.add_parents(prefix);
        self.dirs.insert(prefix.to_path_buf());
        for dir in other.dirs {
            self.dirs.insert(prefix.join(dir));
        }
        for (rel, source) in other.files {
            self.files.insert(prefix.join(rel), source);
        }
    }

    fn add_parents(&mut self, rel: &Path) {
        let mut dir = rel.parent();
        while let Some(d) = dir {
            if d.as_os_str().is_empty() {
                break;
            }
            self.dirs.insert(d.to_path_buf());
            dir = d.parent();
        }
    }

    /// Paths of every file, relative to the root.
    pub(crate) fn files(&self) -> impl Iterator<Item = &Path> {
        self.files.keys().map(PathBuf::as_path)
    }

    /// Whether the tree holds a file or directory at `rel`.
    pub(crate) fn contains(&self, rel: &Path) -> bool {
        rel.as_os_str().is_empty() || self.files.contains_key(rel) || self.dirs.contains(rel)
    }

    /// The bytes of the file at `rel`.
    pub(crate) fn read(&self, rel: &Path) -> Result<Vec<u8>> {
        match self.files.get(rel) {
            Some(FileSource::Disk(path)) => {
                std::fs::read(path).with_context(|| format!("Failed to read {}", path.display()))
            }
            Some(FileSource::Bytes { data, .. }) => Ok(data.clone()),
            None => anyhow::bail!("{} is not in the source tree", rel.display()),
        }
    }

    /// Whether the file at `rel` is executable, by its owner's execute bit.
    pub(crate) fn is_executable(&self, rel: &Path) -> Result<bool> {
        match self.files.get(rel) {
            Some(FileSource::Disk(path)) => {
                use std::os::unix::fs::PermissionsExt;
                let meta = std::fs::metadata(path)
                    .with_context(|| format!("Failed to stat {}", path.display()))?;
                Ok(meta.permissions().mode() & 0o100 != 0)
            }
            Some(FileSource::Bytes { executable, .. }) => Ok(*executable),
            None => anyhow::bail!("{} is not in the source tree", rel.display()),
        }
    }

    /// The part of the tree below `rel`, rooted at `rel`. Directories without
    /// files stay, as in a copy of that directory.
    pub(crate) fn subtree(&self, rel: &Path) -> SourceTree {
        let strip = |p: &Path| p.strip_prefix(rel).ok().map(Path::to_path_buf);
        SourceTree {
            files: self
                .files
                .iter()
                .filter_map(|(p, s)| strip(p).map(|r| (r, s.clone())))
                .filter(|(r, _)| !r.as_os_str().is_empty())
                .collect(),
            dirs: self
                .dirs
                .iter()
                .filter_map(|p| strip(p))
                .filter(|r| !r.as_os_str().is_empty())
                .collect(),
        }
    }

    /// The files in `keep`, with only the directories on the way to them.
    pub(crate) fn select(&self, keep: &HashSet<PathBuf>) -> SourceTree {
        let mut tree = SourceTree::default();
        for (rel, source) in &self.files {
            if keep.contains(rel) {
                tree.add_parents(rel);
                tree.files.insert(rel.clone(), source.clone());
            }
        }
        tree
    }

    /// Serialise the tree to a NAR, as `nix-store --add` would serialise a
    /// copy of it on disk.
    pub(crate) fn nar(&self) -> Result<Vec<u8>> {
        let mut buf = Vec::with_capacity(1024 * 1024);
        nar_string(&mut buf, "nix-archive-1");
        self.write_dir(&mut buf, Path::new(""))?;
        Ok(buf)
    }

    fn write_dir(&self, buf: &mut Vec<u8>, dir: &Path) -> Result<()> {
        nar_string(buf, "(");
        nar_string(buf, "type");
        nar_string(buf, "directory");
        for name in self.children(dir) {
            let child = dir.join(&name);
            nar_string(buf, "entry");
            nar_string(buf, "(");
            nar_string(buf, "name");
            nar_string(buf, &name);
            nar_string(buf, "node");
            match self.files.get(&child) {
                Some(source) => write_file(buf, source)?,
                None => self.write_dir(buf, &child)?,
            }
            nar_string(buf, ")");
        }
        nar_string(buf, ")");
        Ok(())
    }

    /// Names of the entries directly in `dir`, in NAR order.
    fn children(&self, dir: &Path) -> BTreeSet<String> {
        let direct = |p: &PathBuf| {
            (p.parent() == Some(dir))
                .then(|| p.file_name().map(|n| n.to_string_lossy().into_owned()))
                .flatten()
        };
        // `String` orders by bytes, which is the order NAR requires.
        self.files
            .keys()
            .filter_map(direct)
            .chain(self.dirs.iter().filter_map(direct))
            .collect()
    }

    /// Write the tree below `dest`, as a copy of the source would look.
    pub(crate) fn materialise(&self, dest: &Path) -> Result<()> {
        std::fs::create_dir_all(dest)?;
        for dir in &self.dirs {
            std::fs::create_dir_all(dest.join(dir))?;
        }
        for (rel, source) in &self.files {
            let target = dest.join(rel);
            if let Some(parent) = target.parent() {
                std::fs::create_dir_all(parent)?;
            }
            match source {
                FileSource::Disk(path) => {
                    std::fs::copy(path, &target)
                        .with_context(|| format!("Failed to copy {}", path.display()))?;
                }
                FileSource::Bytes { data, executable } => {
                    use std::os::unix::fs::PermissionsExt;
                    std::fs::write(&target, data)?;
                    let mode = if *executable { 0o755 } else { 0o644 };
                    std::fs::set_permissions(&target, std::fs::Permissions::from_mode(mode))?;
                }
            }
        }
        Ok(())
    }
}

fn write_file(buf: &mut Vec<u8>, source: &FileSource) -> Result<()> {
    let (data, executable) = match source {
        FileSource::Disk(path) => {
            use std::os::unix::fs::PermissionsExt;
            let meta = std::fs::metadata(path)
                .with_context(|| format!("Failed to stat {}", path.display()))?;
            let data = std::fs::read(path)
                .with_context(|| format!("Failed to read {}", path.display()))?;
            (data, meta.permissions().mode() & 0o100 != 0)
        }
        FileSource::Bytes { data, executable } => (data.clone(), *executable),
    };
    nar_string(buf, "(");
    nar_string(buf, "type");
    nar_string(buf, "regular");
    if executable {
        nar_string(buf, "executable");
        nar_string(buf, "");
    }
    nar_string(buf, "contents");
    nar_bytes(buf, &data);
    nar_string(buf, ")");
    Ok(())
}

/// The project source as the planner sees it: a [`SourceTree`], the store
/// path the tree has as `project-src`, and the skeleton that the unit graph
/// is planned from. The tree reaches the store only through
/// [`ProjectSource::ensure_in_store`], which the planner calls only when a
/// unit still names the whole tree after per-crate slicing.
pub(crate) struct ProjectSource {
    pub(crate) tree: SourceTree,
    /// The store path of the tree, whether or not the store holds it.
    pub(crate) store_path: String,
    nar: Vec<u8>,
    /// The store path of the manifest-only skeleton, when there is one.
    pub(crate) skeleton: Option<String>,
    in_store: std::cell::Cell<bool>,
}

impl ProjectSource {
    pub(crate) fn new(
        tree: SourceTree,
        store_path: String,
        nar: Vec<u8>,
        skeleton: Option<String>,
    ) -> Self {
        Self {
            tree,
            store_path,
            nar,
            skeleton,
            in_store: std::cell::Cell::new(false),
        }
    }

    /// The project source that the store already holds at `path`, as the
    /// planner derivation receives it.
    pub(crate) fn in_store_at(path: &Path) -> Result<Self> {
        let tree = SourceTree::from_dir_excluding(path, &[])?;
        let source = Self::new(tree, path.to_string_lossy().into_owned(), Vec::new(), None);
        source.in_store.set(true);
        Ok(source)
    }

    /// Whether the store holds the whole tree.
    pub(crate) fn in_store(&self) -> bool {
        self.in_store.get()
    }

    /// Make the store hold the whole tree at `store_path`, over the daemon,
    /// or with `nix-store --add` on a copy when the daemon is unavailable.
    pub(crate) fn ensure_in_store(&self) -> Result<()> {
        if self.in_store.get() {
            return Ok(());
        }
        let _span = tracing::info_span!("store_add_project").entered();
        let added = crate::plan_nix::add_source_nar("project-src", &self.nar).or_else(|e| {
            tracing::info!("Daemon add of project source failed: {}, using a copy", e);
            let scratch =
                tempfile::tempdir().context("Failed to create temp dir for source copy")?;
            let dest = scratch.path().join("project-src");
            self.tree.materialise(&dest)?;
            crate::add_to_nix_store(&dest.to_string_lossy())
        })?;
        anyhow::ensure!(
            added == self.store_path,
            "The project source landed at {added}, expected {}",
            self.store_path
        );
        self.in_store.set(true);
        Ok(())
    }
}
