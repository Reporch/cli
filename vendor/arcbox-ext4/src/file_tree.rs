// In-memory file tree used by the Formatter to track the directory structure
// being built.  Each node holds an inode number, name, optional block ranges,
// and pointers (indices) into the flat `nodes` vector.

use std::path::{Path, PathBuf};

pub type InodeNumber = u32;

/// A block range [start, end) in units of filesystem blocks.
#[derive(Debug, Clone, Copy)]
pub struct BlockRange {
    pub start: u32,
    pub end: u32,
}

/// A node in the in-memory file tree.
pub struct FileTreeNode {
    pub inode: InodeNumber,
    pub name: String,
    /// Indices into `FileTree::nodes` for this node's children.
    pub children: Vec<usize>,
    /// Index into `FileTree::nodes` for the parent (None for root).
    pub parent: Option<usize>,
    /// Primary data block range allocated to this node.
    pub blocks: Option<BlockRange>,
    /// Additional block ranges (for files spanning multiple extents).
    pub additional_blocks: Vec<BlockRange>,
    /// If this entry is a hard link, the target inode number.
    pub link: Option<InodeNumber>,
}

/// In-memory file tree tracking directory structure during formatting.
pub struct FileTree {
    nodes: Vec<FileTreeNode>,
    root: usize,
}

impl FileTree {
    /// Create a new file tree with a single root node.
    pub fn new(root_inode: InodeNumber, name: &str) -> Self {
        let root = FileTreeNode {
            inode: root_inode,
            name: name.to_string(),
            children: Vec::new(),
            parent: None,
            blocks: None,
            additional_blocks: Vec::new(),
            link: None,
        };
        Self {
            nodes: vec![root],
            root: 0,
        }
    }

    /// Return the index of the root node.
    #[inline]
    pub fn root(&self) -> usize {
        self.root
    }

    /// Borrow a node by index.
    #[inline]
    pub fn node(&self, idx: usize) -> &FileTreeNode {
        &self.nodes[idx]
    }

    /// Mutably borrow a node by index.
    #[inline]
    pub fn node_mut(&mut self, idx: usize) -> &mut FileTreeNode {
        &mut self.nodes[idx]
    }

    /// Look up a node by path, walking the tree from the root.
    ///
    /// The path is split into components.  A leading "/" or empty first component
    /// is skipped so that both "/foo/bar" and "foo/bar" resolve identically.
    /// Returns `None` if any component is not found.
    pub fn lookup(&self, path: &Path) -> Option<usize> {
        let mut current = self.root;

        for component in path.components() {
            let name = component.as_os_str().to_str()?;

            // Skip the root directory prefix.
            if name == "/" || name.is_empty() {
                continue;
            }

            let node = &self.nodes[current];
            let found = node.children.iter().find(|&&child_idx| {
                self.nodes[child_idx].name == name
            });

            match found {
                Some(&child_idx) => current = child_idx,
                None => return None,
            }
        }

        Some(current)
    }

    /// Add a child node under `parent`, returning the new node's index.
    pub fn add_child(&mut self, parent: usize, mut node: FileTreeNode) -> usize {
        let idx = self.nodes.len();
        node.parent = Some(parent);
        self.nodes.push(node);
        self.nodes[parent].children.push(idx);
        idx
    }

    /// Remove the first child with the given `name` from `parent`'s children list.
    ///
    /// The node itself remains in the vector (indices are stable), but the
    /// parent no longer references it.
    pub fn remove_child(&mut self, parent: usize, name: &str) {
        let pos = self.nodes[parent]
            .children
            .iter()
            .position(|&child_idx| self.nodes[child_idx].name == name);

        if let Some(pos) = pos {
            self.nodes[parent].children.remove(pos);
        }
    }

    /// Reconstruct the full path of a node by walking the parent chain.
    pub fn node_path(&self, idx: usize) -> PathBuf {
        let mut parts = Vec::new();
        let mut current = idx;

        loop {
            parts.push(self.nodes[current].name.as_str());
            match self.nodes[current].parent {
                Some(parent) => current = parent,
                None => break,
            }
        }

        parts.reverse();

        let mut path = PathBuf::new();
        for part in &parts {
            path.push(part);
        }
        path
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_new_tree_has_root() {
        let tree = FileTree::new(2, "/");
        assert_eq!(tree.root(), 0);
        assert_eq!(tree.node(0).inode, 2);
        assert_eq!(tree.node(0).name, "/");
    }

    #[test]
    fn test_add_and_lookup() {
        let mut tree = FileTree::new(2, "/");
        let child = FileTreeNode {
            inode: 11,
            name: "etc".to_string(),
            children: Vec::new(),
            parent: None,
            blocks: None,
            additional_blocks: Vec::new(),
            link: None,
        };
        let etc_idx = tree.add_child(tree.root(), child);

        let grandchild = FileTreeNode {
            inode: 12,
            name: "passwd".to_string(),
            children: Vec::new(),
            parent: None,
            blocks: None,
            additional_blocks: Vec::new(),
            link: None,
        };
        tree.add_child(etc_idx, grandchild);

        // Lookup with leading slash.
        assert_eq!(tree.lookup(Path::new("/etc/passwd")), Some(2));

        // Lookup without leading slash.
        assert_eq!(tree.lookup(Path::new("etc/passwd")), Some(2));

        // Lookup directory itself.
        assert_eq!(tree.lookup(Path::new("/etc")), Some(1));

        // Lookup root.
        assert_eq!(tree.lookup(Path::new("/")), Some(0));

        // Missing path.
        assert_eq!(tree.lookup(Path::new("/etc/shadow")), None);
    }

    #[test]
    fn test_remove_child() {
        let mut tree = FileTree::new(2, "/");
        let child_a = FileTreeNode {
            inode: 11,
            name: "a".to_string(),
            children: Vec::new(),
            parent: None,
            blocks: None,
            additional_blocks: Vec::new(),
            link: None,
        };
        let child_b = FileTreeNode {
            inode: 12,
            name: "b".to_string(),
            children: Vec::new(),
            parent: None,
            blocks: None,
            additional_blocks: Vec::new(),
            link: None,
        };
        tree.add_child(tree.root(), child_a);
        tree.add_child(tree.root(), child_b);

        assert_eq!(tree.node(tree.root()).children.len(), 2);

        tree.remove_child(tree.root(), "a");
        assert_eq!(tree.node(tree.root()).children.len(), 1);
        assert_eq!(tree.node(tree.node(tree.root()).children[0]).name, "b");
    }

    #[test]
    fn test_node_path() {
        let mut tree = FileTree::new(2, "/");
        let etc = FileTreeNode {
            inode: 11,
            name: "etc".to_string(),
            children: Vec::new(),
            parent: None,
            blocks: None,
            additional_blocks: Vec::new(),
            link: None,
        };
        let etc_idx = tree.add_child(tree.root(), etc);

        let passwd = FileTreeNode {
            inode: 12,
            name: "passwd".to_string(),
            children: Vec::new(),
            parent: None,
            blocks: None,
            additional_blocks: Vec::new(),
            link: None,
        };
        let passwd_idx = tree.add_child(etc_idx, passwd);

        let path = tree.node_path(passwd_idx);
        assert_eq!(path, PathBuf::from("/etc/passwd"));
    }
}
