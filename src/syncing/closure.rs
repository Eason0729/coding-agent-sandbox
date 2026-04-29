use std::cmp::Reverse;
use std::path::{Path, PathBuf};

use dashmap::DashMap;
use serde::{Deserialize, Deserializer, Serialize, Serializer};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
struct ClosureRow {
    ancestor: PathBuf,
    descendant: PathBuf,
    distance: u32,
}

#[derive(Debug, Default)]
pub struct PathTree {
    by_ancestor: DashMap<PathBuf, Vec<(PathBuf, u32)>>,
    by_descendant: DashMap<PathBuf, Vec<(PathBuf, u32)>>,
    by_parent: DashMap<PathBuf, Vec<PathBuf>>,
}

impl PathTree {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn from_paths<I, P>(paths: I) -> Self
    where
        I: IntoIterator<Item = P>,
        P: AsRef<Path>,
    {
        let mut paths = paths
            .into_iter()
            .map(|path| path.as_ref().to_path_buf())
            .collect::<Vec<_>>();
        paths.sort_by_key(|path| Self::depth(path));

        let tree = Self::new();
        for path in paths {
            tree.insert(&path);
        }
        tree
    }

    pub fn contains(&self, path: &Path) -> bool {
        self.by_descendant.contains_key(path)
    }

    pub fn insert(&self, path: &Path) {
        let mut prefixes = path
            .ancestors()
            .filter(|ancestor| !ancestor.as_os_str().is_empty())
            .map(|ancestor| ancestor.to_path_buf())
            .collect::<Vec<_>>();
        prefixes.reverse();

        for prefix in prefixes {
            if self.contains(&prefix) {
                continue;
            }

            for (ancestor, distance) in Self::ancestors_with_distance(&prefix) {
                self.insert_row(ancestor, prefix.clone(), distance);
            }
        }
    }

    pub fn remove(&self, path: &Path) -> Vec<PathBuf> {
        if !self.contains(path) {
            return Vec::new();
        }

        let mut subtree = self.subtree_paths(path);
        subtree.sort_by_key(|p| Reverse(p.components().count()));

        for victim in &subtree {
            self.remove_one(victim);
        }

        subtree
    }

    pub fn children_of(&self, path: &Path) -> Vec<PathBuf> {
        self.by_parent
            .get(path)
            .map(|entries| entries.iter().cloned().collect())
            .unwrap_or_default()
    }

    pub fn descendants_of(&self, path: &Path) -> Vec<PathBuf> {
        self.by_ancestor
            .get(path)
            .map(|entries| {
                entries
                    .iter()
                    .filter(|(_, distance)| *distance > 0)
                    .map(|(descendant, _)| descendant.clone())
                    .collect()
            })
            .unwrap_or_default()
    }

    pub fn ancestors_of(&self, path: &Path) -> Vec<PathBuf> {
        self.by_descendant
            .get(path)
            .map(|entries| {
                entries
                    .iter()
                    .filter(|(_, distance)| *distance > 0)
                    .map(|(ancestor, _)| ancestor.clone())
                    .collect()
            })
            .unwrap_or_default()
    }

    pub fn subtree_paths(&self, path: &Path) -> Vec<PathBuf> {
        if !self.contains(path) {
            return Vec::new();
        }

        let mut paths = vec![path.to_path_buf()];
        paths.extend(self.descendants_of(path));
        paths
    }

    pub fn clear(&self) {
        self.by_ancestor.clear();
        self.by_descendant.clear();
        self.by_parent.clear();
    }

    pub fn rows(&self) -> Vec<(PathBuf, PathBuf, u32)> {
        self.by_ancestor
            .iter()
            .flat_map(|entry| {
                let ancestor = entry.key().clone();
                entry
                    .value()
                    .iter()
                    .map(move |(descendant, distance)| {
                        (ancestor.clone(), descendant.clone(), *distance)
                    })
                    .collect::<Vec<_>>()
            })
            .collect()
    }

    fn insert_row(&self, ancestor: PathBuf, descendant: PathBuf, distance: u32) {
        let ancestor_for_descendant = ancestor.clone();
        let descendant_for_parent = descendant.clone();

        self.by_ancestor
            .entry(ancestor)
            .or_insert_with(Vec::new)
            .push((descendant_for_parent.clone(), distance));

        self.by_descendant
            .entry(descendant)
            .or_insert_with(Vec::new)
            .push((ancestor_for_descendant.clone(), distance));

        if distance == 1 {
            self.by_parent
                .entry(ancestor_for_descendant)
                .or_insert_with(Vec::new)
                .push(descendant_for_parent);
        }
    }

    fn remove_one(&self, path: &Path) {
        let path_buf = path.to_path_buf();

        if let Some((_, ancestors)) = self.by_descendant.remove(path) {
            for (ancestor, _) in ancestors {
                if let Some(mut entry) = self.by_ancestor.get_mut(&ancestor) {
                    entry.retain(|(descendant, _)| descendant != &path_buf);
                    let is_empty = entry.is_empty();
                    drop(entry);
                    if is_empty {
                        self.by_ancestor.remove(&ancestor);
                    }
                }

                if let Some(mut entry) = self.by_parent.get_mut(&ancestor) {
                    entry.retain(|descendant| descendant != &path_buf);
                    let is_empty = entry.is_empty();
                    drop(entry);
                    if is_empty {
                        self.by_parent.remove(&ancestor);
                    }
                }
            }
        }

        self.by_ancestor.remove(path);
        self.by_parent.remove(path);
    }

    fn ancestors_with_distance(path: &Path) -> Vec<(PathBuf, u32)> {
        let total = Self::depth(path);
        path.ancestors()
            .filter(|ancestor| !ancestor.as_os_str().is_empty())
            .map(|ancestor| {
                let distance = total.saturating_sub(Self::depth(ancestor));
                (ancestor.to_path_buf(), distance as u32)
            })
            .collect()
    }

    fn depth(path: &Path) -> usize {
        path.components()
            .filter(|component| matches!(component, std::path::Component::Normal(_)))
            .count()
    }

    fn from_rows(rows: Vec<ClosureRow>) -> Self {
        let tree = Self::new();
        for row in rows {
            tree.insert_row(row.ancestor, row.descendant, row.distance);
        }
        tree
    }
}

impl Serialize for PathTree {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        let rows = self
            .rows()
            .into_iter()
            .map(|(ancestor, descendant, distance)| ClosureRow {
                ancestor,
                descendant,
                distance,
            })
            .collect::<Vec<_>>();
        rows.serialize(serializer)
    }
}

impl<'de> Deserialize<'de> for PathTree {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let rows = Vec::<ClosureRow>::deserialize(deserializer)?;
        Ok(Self::from_rows(rows))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sorted(paths: &[PathBuf]) -> Vec<PathBuf> {
        let mut out = paths.to_vec();
        out.sort();
        out
    }

    #[test]
    fn insert_single_path() {
        let tree = PathTree::new();
        tree.insert(Path::new("/a"));

        assert_eq!(
            sorted(&tree.children_of(Path::new("/"))),
            vec![PathBuf::from("/a")]
        );
        assert_eq!(
            sorted(&tree.ancestors_of(Path::new("/a"))),
            vec![PathBuf::from("/")]
        );
    }

    #[test]
    fn insert_nested_path() {
        let tree = PathTree::new();
        tree.insert(Path::new("/a/b/c"));

        assert_eq!(
            sorted(&tree.children_of(Path::new("/a/b"))),
            vec![PathBuf::from("/a/b/c")]
        );
        assert_eq!(
            sorted(&tree.ancestors_of(Path::new("/a/b/c"))),
            vec![
                PathBuf::from("/"),
                PathBuf::from("/a"),
                PathBuf::from("/a/b")
            ]
        );
    }

    #[test]
    fn children_of_root() {
        let tree = PathTree::new();
        tree.insert(Path::new("/a"));
        tree.insert(Path::new("/b"));
        tree.insert(Path::new("/a/b/c"));

        assert_eq!(
            sorted(&tree.children_of(Path::new("/"))),
            vec![PathBuf::from("/a"), PathBuf::from("/b")]
        );
    }

    #[test]
    fn descendants_of() {
        let tree = PathTree::new();
        tree.insert(Path::new("/a/b/c"));
        tree.insert(Path::new("/a/d"));

        assert_eq!(
            sorted(&tree.descendants_of(Path::new("/a"))),
            vec![
                PathBuf::from("/a/b"),
                PathBuf::from("/a/b/c"),
                PathBuf::from("/a/d")
            ]
        );
    }

    #[test]
    fn children_of_is_direct_only() {
        let tree = PathTree::new();
        tree.insert(Path::new("/a/b/c"));
        tree.insert(Path::new("/a/d"));

        assert_eq!(
            sorted(&tree.children_of(Path::new("/a"))),
            vec![PathBuf::from("/a/b"), PathBuf::from("/a/d")]
        );
        assert_eq!(
            sorted(&tree.children_of(Path::new("/a/b"))),
            vec![PathBuf::from("/a/b/c")]
        );
        assert!(tree.children_of(Path::new("/a/b/c")).is_empty());
    }

    #[test]
    fn remove_cascade() {
        let tree = PathTree::new();
        tree.insert(Path::new("/a/b/c"));

        let removed = tree.remove(Path::new("/a"));
        assert_eq!(
            sorted(&removed),
            vec![
                PathBuf::from("/a"),
                PathBuf::from("/a/b"),
                PathBuf::from("/a/b/c")
            ]
        );

        assert!(!tree.contains(Path::new("/a")));
        assert!(!tree.contains(Path::new("/a/b")));
        assert!(!tree.contains(Path::new("/a/b/c")));
    }

    #[test]
    fn remove_leaf() {
        let tree = PathTree::new();
        tree.insert(Path::new("/a/b/c"));
        tree.insert(Path::new("/a/d"));

        let removed = tree.remove(Path::new("/a/b/c"));
        assert_eq!(sorted(&removed), vec![PathBuf::from("/a/b/c")]);

        assert!(tree.contains(Path::new("/a")));
        assert!(tree.contains(Path::new("/a/d")));
        assert!(!tree.contains(Path::new("/a/b/c")));
    }

    #[test]
    fn subtree_paths() {
        let tree = PathTree::new();
        tree.insert(Path::new("/a/b/c"));
        tree.insert(Path::new("/a/d"));
        tree.insert(Path::new("/e"));

        assert_eq!(
            sorted(&tree.subtree_paths(Path::new("/a"))),
            vec![
                PathBuf::from("/a"),
                PathBuf::from("/a/b"),
                PathBuf::from("/a/b/c"),
                PathBuf::from("/a/d"),
            ]
        );
    }

    #[test]
    fn serialization_round_trip() {
        let tree = PathTree::new();
        tree.insert(Path::new("/a/b/c"));
        tree.insert(Path::new("/a/d"));

        let data = postcard::to_allocvec(&tree).unwrap();
        let restored: PathTree = postcard::from_bytes(&data).unwrap();

        assert_eq!(
            sorted(&restored.children_of(Path::new("/a"))),
            vec![PathBuf::from("/a/b"), PathBuf::from("/a/d")]
        );
        assert_eq!(
            sorted(&restored.ancestors_of(Path::new("/a/b/c"))),
            vec![
                PathBuf::from("/"),
                PathBuf::from("/a"),
                PathBuf::from("/a/b")
            ]
        );
    }

    #[test]
    fn empty_lookups() {
        let tree = PathTree::new();

        assert!(tree.children_of(Path::new("/nonexistent")).is_empty());
        assert!(tree.descendants_of(Path::new("/nonexistent")).is_empty());
        assert!(tree.ancestors_of(Path::new("/nonexistent")).is_empty());
        assert!(!tree.contains(Path::new("/nonexistent")));
    }

    #[test]
    fn multiple_roots() {
        let tree = PathTree::new();
        tree.insert(Path::new("/a"));
        tree.insert(Path::new("/b"));
        tree.insert(Path::new("/c"));

        assert_eq!(
            sorted(&tree.children_of(Path::new("/"))),
            vec![
                PathBuf::from("/a"),
                PathBuf::from("/b"),
                PathBuf::from("/c")
            ]
        );
    }

    #[test]
    fn remove_non_existent() {
        let tree = PathTree::new();
        tree.insert(Path::new("/a"));

        assert!(tree.remove(Path::new("/nonexistent")).is_empty());
        assert!(tree.contains(Path::new("/a")));
    }

    #[test]
    fn remove_updates_direct_children_index() {
        let tree = PathTree::new();
        tree.insert(Path::new("/a/b/c"));

        assert_eq!(
            tree.children_of(Path::new("/a/b")),
            vec![PathBuf::from("/a/b/c")]
        );

        tree.remove(Path::new("/a/b/c"));

        assert!(tree.children_of(Path::new("/a/b")).is_empty());
    }
}
