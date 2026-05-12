use std::{
    collections::BTreeSet,
    ffi::OsStr,
    fs,
    path::{Path, PathBuf},
};

use anyhow::{Context, Result};

use crate::ignore::IgnoreRules;

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum TreeEntryKind {
    Directory,
    File,
}

#[derive(Clone, Debug)]
pub struct VisibleEntry {
    pub path: PathBuf,
    pub name: String,
    pub depth: usize,
    pub kind: TreeEntryKind,
    pub expanded: bool,
}

#[derive(Clone, Debug)]
pub struct FileTree {
    root: PathBuf,
    ignore_rules: IgnoreRules,
    expanded: BTreeSet<PathBuf>,
    visible: Vec<VisibleEntry>,
    selected: usize,
}

#[derive(Clone, Debug)]
pub struct IgnoredEntry {
    pub path: PathBuf,
    pub kind: TreeEntryKind,
    pub pattern: String,
}

impl FileTree {
    pub fn new(root: PathBuf) -> Result<Self> {
        let root = root
            .canonicalize()
            .with_context(|| format!("failed to resolve {}", root.display()))?;
        let ignore_rules = IgnoreRules::load(&root)?;
        let mut expanded = BTreeSet::new();
        expanded.insert(root.clone());
        let mut tree = Self {
            root,
            ignore_rules,
            expanded,
            visible: Vec::new(),
            selected: 0,
        };
        tree.refresh()?;
        Ok(tree)
    }

    pub fn refresh(&mut self) -> Result<()> {
        let previous = self.selected_path().map(Path::to_path_buf);
        self.ignore_rules = IgnoreRules::load(&self.root)?;
        self.visible.clear();
        self.push_children(self.root.clone(), 0)?;

        if let Some(previous) = previous
            && let Some(index) = self.visible.iter().position(|entry| entry.path == previous)
        {
            self.selected = index;
            return Ok(());
        }

        self.selected = self.selected.min(self.visible.len().saturating_sub(1));
        Ok(())
    }

    pub fn visible(&self) -> &[VisibleEntry] {
        &self.visible
    }

    pub fn selected(&self) -> usize {
        self.selected
    }

    pub fn selected_entry(&self) -> Option<&VisibleEntry> {
        self.visible.get(self.selected)
    }

    pub fn selected_path(&self) -> Option<&Path> {
        self.selected_entry().map(|entry| entry.path.as_path())
    }

    pub fn move_up(&mut self) {
        self.selected = self.selected.saturating_sub(1);
    }

    pub fn move_down(&mut self) {
        if !self.visible.is_empty() {
            self.selected = (self.selected + 1).min(self.visible.len() - 1);
        }
    }

    pub fn toggle_selected(&mut self) -> Result<Option<PathBuf>> {
        let Some(entry) = self.selected_entry().cloned() else {
            return Ok(None);
        };

        match entry.kind {
            TreeEntryKind::File => Ok(Some(entry.path)),
            TreeEntryKind::Directory => {
                if entry.expanded {
                    self.expanded.remove(&entry.path);
                } else {
                    self.expanded.insert(entry.path);
                }
                self.refresh()?;
                Ok(None)
            }
        }
    }

    pub fn expand_selected(&mut self) -> Result<Option<PathBuf>> {
        let Some(entry) = self.selected_entry().cloned() else {
            return Ok(None);
        };

        match entry.kind {
            TreeEntryKind::Directory => {
                self.expanded.insert(entry.path);
                self.refresh()?;
                Ok(None)
            }
            TreeEntryKind::File => Ok(Some(entry.path)),
        }
    }

    pub fn collapse_selected(&mut self) -> Result<()> {
        let Some(entry) = self.selected_entry().cloned() else {
            return Ok(());
        };

        if entry.kind == TreeEntryKind::Directory && entry.expanded {
            self.expanded.remove(&entry.path);
            self.refresh()?;
            return Ok(());
        }

        if let Some(parent) = entry.path.parent()
            && parent != self.root
            && let Some(index) = self
                .visible
                .iter()
                .position(|visible| visible.path == parent)
        {
            self.selected = index;
        }

        Ok(())
    }

    pub fn ignore_selected(&mut self) -> Result<Option<IgnoredEntry>> {
        let Some(entry) = self.selected_entry().cloned() else {
            return Ok(None);
        };
        let is_dir = entry.kind == TreeEntryKind::Directory;
        let pattern = self.ignore_rules.add_path(&entry.path, is_dir)?;
        let ignored = IgnoredEntry {
            path: entry.path,
            kind: entry.kind,
            pattern,
        };
        self.refresh()?;
        Ok(Some(ignored))
    }

    pub fn files(&self) -> Vec<PathBuf> {
        let mut files = Vec::new();
        self.collect_files(&self.root, &mut files);
        files
    }

    fn push_children(&mut self, directory: PathBuf, depth: usize) -> Result<()> {
        let mut entries = fs::read_dir(&directory)
            .with_context(|| format!("failed to read {}", directory.display()))?
            .collect::<Result<Vec<_>, _>>()?;

        entries.retain(|entry| !self.is_hidden(entry.path().as_path()));
        entries.sort_by(|left, right| {
            let left_path = left.path();
            let right_path = right.path();
            let left_is_dir = left_path.is_dir();
            let right_is_dir = right_path.is_dir();
            right_is_dir
                .cmp(&left_is_dir)
                .then_with(|| left.file_name().cmp(&right.file_name()))
        });

        for entry in entries {
            let path = entry.path();
            let file_type = entry.file_type()?;
            let is_dir = file_type.is_dir();
            let expanded = is_dir && self.expanded.contains(&path);
            self.visible.push(VisibleEntry {
                name: entry.file_name().to_string_lossy().to_string(),
                path: path.clone(),
                depth,
                kind: if is_dir {
                    TreeEntryKind::Directory
                } else {
                    TreeEntryKind::File
                },
                expanded,
            });

            if expanded {
                self.push_children(path, depth + 1)?;
            }
        }

        Ok(())
    }

    fn collect_files(&self, directory: &Path, files: &mut Vec<PathBuf>) {
        let Ok(entries) = fs::read_dir(directory) else {
            return;
        };

        for entry in entries.flatten() {
            let path = entry.path();
            if self.is_hidden(&path) {
                continue;
            }

            if path.is_dir() {
                self.collect_files(&path, files);
            } else if path.is_file() {
                files.push(path);
            }
        }
    }

    fn is_hidden(&self, path: &Path) -> bool {
        is_builtin_ignored(path) || self.ignore_rules.is_ignored(path, path.is_dir())
    }
}

fn is_builtin_ignored(path: &Path) -> bool {
    matches!(
        path.file_name().and_then(OsStr::to_str),
        Some(".git" | ".debth" | "target")
    )
}

#[cfg(test)]
mod tests {
    use std::time::{SystemTime, UNIX_EPOCH};

    use super::*;

    #[test]
    fn selected_directory_can_expand_and_collapse() -> Result<()> {
        let root = unique_temp_dir();
        let src = root.join("src");
        fs::create_dir_all(&src)?;
        fs::write(src.join("main.rs"), "fn main() {}\n")?;

        let mut tree = FileTree::new(root.clone())?;
        assert_eq!(tree.visible().len(), 1);
        assert_eq!(tree.visible()[0].name, "src");
        assert!(!tree.visible()[0].expanded);

        tree.toggle_selected()?;
        assert_eq!(tree.visible().len(), 2);
        assert!(tree.visible()[0].expanded);
        assert_eq!(tree.visible()[1].name, "main.rs");

        tree.toggle_selected()?;
        assert_eq!(tree.visible().len(), 1);
        assert!(!tree.visible()[0].expanded);

        fs::remove_dir_all(root)?;
        Ok(())
    }

    #[test]
    fn ignored_selected_file_disappears_from_visible_tree() -> Result<()> {
        let root = unique_temp_dir();
        fs::create_dir_all(&root)?;
        fs::write(root.join("keep.rs"), "fn keep() {}\n")?;
        fs::write(root.join("skip.rs"), "fn skip() {}\n")?;

        let mut tree = FileTree::new(root.clone())?;
        while tree
            .selected_entry()
            .is_some_and(|entry| entry.name != "skip.rs")
        {
            tree.move_down();
        }
        let ignored = tree.ignore_selected()?.expect("selected entry");

        assert_eq!(ignored.pattern, "/skip.rs");
        assert!(tree.visible().iter().all(|entry| entry.name != "skip.rs"));
        assert!(tree.visible().iter().any(|entry| entry.name == "keep.rs"));

        fs::remove_dir_all(root)?;
        Ok(())
    }

    #[test]
    fn ignored_selected_directory_hides_children_from_stats_files() -> Result<()> {
        let root = unique_temp_dir();
        let keep = root.join("keep");
        let skip = root.join("skip");
        fs::create_dir_all(&keep)?;
        fs::create_dir_all(&skip)?;
        fs::write(keep.join("main.rs"), "fn keep() {}\n")?;
        fs::write(skip.join("main.rs"), "fn skip() {}\n")?;
        let keep = keep.canonicalize()?;
        let skip = skip.canonicalize()?;

        let mut tree = FileTree::new(root.clone())?;
        while tree
            .selected_entry()
            .is_some_and(|entry| entry.name != "skip")
        {
            tree.move_down();
        }
        let ignored = tree.ignore_selected()?.expect("selected entry");

        assert_eq!(ignored.pattern, "/skip/");
        assert!(tree.files().iter().all(|path| !path.starts_with(&skip)));
        assert!(tree.files().iter().any(|path| path.starts_with(&keep)));

        fs::remove_dir_all(root)?;
        Ok(())
    }

    fn unique_temp_dir() -> PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|duration| duration.as_nanos())
            .unwrap_or_default();
        std::env::temp_dir().join(format!("debth-tree-test-{}-{nanos}", std::process::id()))
    }
}
