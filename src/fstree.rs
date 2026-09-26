use rc_zip::parse::Entry;

#[derive(Clone)]
pub(crate) enum FsTreeNode<E: Clone = Entry> {
    Dir {
        name: String,
        children: Vec<FsTreeNode<E>>,
        is_root: bool,
        index_html_index: Option<usize>,
    },
    File {
        name: String,
        entry: E,
    },
}

/// What a request path is answered with.
#[derive(Debug)]
pub(crate) enum Target<'a, E: Clone = Entry> {
    /// A file, or the `index.html` standing in for its directory.
    Entry(&'a E),
    Listing {
        is_root: bool,
        children: &'a [FsTreeNode<E>],
    },
}

impl<E: Clone> std::fmt::Debug for FsTreeNode<E> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Dir { name, children, .. } => f.debug_map().key(name).value(children).finish(),
            Self::File { name, .. } => name.fmt(f),
        }
    }
}

impl FsTreeNode<Entry> {
    pub(crate) fn insert(&mut self, entry: Entry) {
        let path = entry.name.clone();
        self.insert_at(entry, path);
    }
}

impl<E: Clone> FsTreeNode<E> {
    pub(crate) fn insert_at(&mut self, new_entry: E, path: String) {
        let FsTreeNode::Dir {
            children,
            index_html_index,
            ..
        } = self
        else {
            panic!("Cannot insert into FsTreeNode::File")
        };

        if let Some(separator) = path.find('/') {
            let mut head = path;
            let mut tail = head.split_off(separator);

            let existing_child = children
                .iter_mut()
                .find(|ch| matches!(ch, FsTreeNode::Dir { name, .. } if head.eq(name)));

            let child = if let Some(child) = existing_child {
                child
            } else {
                children.push(FsTreeNode::Dir {
                    name: head.clone(),
                    children: vec![],
                    is_root: false,
                    index_html_index: None,
                });
                children
                    .last_mut()
                    .expect("should find the just inserted child")
            };

            // A trailing slash names the directory itself, which keeps no entry.
            if tail.len() > 1 {
                // Recurse
                let tail = tail.split_off(1);
                child.insert_at(new_entry, tail);
            }
        } else {
            // Insertion site of a file
            if path == "index.html" {
                *index_html_index = Some(children.len());
            }
            children.push(FsTreeNode::File {
                name: path.clone(),
                entry: new_entry,
            });
        }
    }

    pub(crate) fn root() -> Self {
        FsTreeNode::Dir {
            name: String::new(),
            children: Vec::new(),
            is_root: true,
            index_html_index: None,
        }
    }

    pub(crate) fn name(&self) -> &str {
        match self {
            FsTreeNode::Dir { name, .. } | FsTreeNode::File { name, .. } => name,
        }
    }

    pub(crate) fn find(&self, path: &str) -> Option<&Self> {
        if path.is_empty() || path == "/" {
            return Some(self);
        }

        let (head, tail) = match path.find('/') {
            Some(sep) => (&path[..sep], &path[sep + 1..]),
            None => (path, ""),
        };
        if head.is_empty() {
            return self.find(tail);
        }

        match self {
            FsTreeNode::Dir { children, .. } => children
                .iter()
                .find(|c| c.name() == head)
                .and_then(|c| c.find(tail)),
            FsTreeNode::File { .. } => None,
        }
    }

    pub(crate) fn resolve(&self, path: &str) -> Option<Target<'_, E>> {
        let target = match self.find(path)? {
            FsTreeNode::File { entry, .. } => Target::Entry(entry),
            FsTreeNode::Dir {
                children,
                index_html_index: Some(idx),
                ..
            } if let FsTreeNode::File { entry, .. } = &children[*idx] => Target::Entry(entry),
            FsTreeNode::Dir {
                is_root, children, ..
            } => Target::Listing {
                is_root: *is_root,
                children,
            },
        };
        Some(target)
    }

    pub(crate) fn recursive_sort(&mut self) {
        if let FsTreeNode::Dir {
            children,
            index_html_index,
            ..
        } = self
        {
            children.sort_by(|a, b| Ord::cmp(&a.name(), &b.name()));
            children.iter_mut().for_each(FsTreeNode::recursive_sort);
            if index_html_index.is_some() {
                *index_html_index = children.iter().position(|c| c.name() == "index.html");
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dir(name: &str) -> FsTreeNode<i32> {
        FsTreeNode::Dir {
            name: name.to_string(),
            children: vec![],
            is_root: false,
            index_html_index: None,
        }
    }

    fn file(name: &str, val: i32) -> FsTreeNode<i32> {
        FsTreeNode::File {
            name: name.to_string(),
            entry: val,
        }
    }

    fn resolved_entry(root: &FsTreeNode<i32>, path: &str) -> i32 {
        match root.resolve(path) {
            Some(Target::Entry(entry)) => *entry,
            other => panic!("{path} should resolve to an entry, not {other:?}"),
        }
    }

    /// Entry names are only ASCII by convention; a zip may hold any UTF-8, and
    /// `rc_zip` transcodes cp437 and shift-jis names into it.
    #[test]
    fn test_insert_and_find_non_ascii_path() {
        let mut root = FsTreeNode::<i32>::root();
        root.insert_at(1, "写真/東京タワー.txt".to_string());
        root.insert_at(2, "日本語のファイル.txt".to_string());

        assert_eq!(resolved_entry(&root, "写真/東京タワー.txt"), 1);
        assert_eq!(resolved_entry(&root, "日本語のファイル.txt"), 2);
        assert_eq!(root.find("写真").unwrap().name(), "写真");
    }

    /// A requested path is any UTF-8 the client cares to send, and must never
    /// be split mid-character.
    #[test]
    fn test_find_non_ascii_path_that_is_absent() {
        let root = FsTreeNode::<i32>::root();
        assert!(root.find("é/x").is_none());
        assert!(root.find("東京/タワー/x.txt").is_none());
    }

    #[test]
    fn test_root_creates_empty_dir() {
        let root = FsTreeNode::<i32>::root();
        assert_eq!(root.name(), "");
        let FsTreeNode::Dir { is_root, .. } = root else {
            panic!("root should be a Dir");
        };
        assert!(is_root);
    }

    #[test]
    fn test_name_returns_name() {
        assert_eq!(dir("mydir").name(), "mydir");
        assert_eq!(file("f.txt", 42).name(), "f.txt");
    }

    #[test]
    fn test_dir_via_trailing_slash() {
        let mut root = FsTreeNode::<i32>::root();
        root.insert_at(42, "subdir/".to_string());
        assert!(matches!(
            root.find("subdir").unwrap(),
            FsTreeNode::Dir { .. }
        ));
    }

    #[test]
    fn test_insert_file_at_root() {
        let mut root = FsTreeNode::<i32>::root();
        root.insert_at(42, "hello.txt".to_string());
        let found = root.find("hello.txt").unwrap();
        assert_eq!(found.name(), "hello.txt");
        assert_eq!(resolved_entry(&root, "hello.txt"), 42);
        assert!(matches!(found, FsTreeNode::File { .. }));
    }

    #[test]
    fn test_insert_multiple_files_at_root() {
        let mut root = FsTreeNode::<i32>::root();
        root.insert_at(1, "a.txt".to_string());
        root.insert_at(2, "b.txt".to_string());
        root.insert_at(3, "c.txt".to_string());
        assert_eq!(resolved_entry(&root, "a.txt"), 1);
        assert_eq!(resolved_entry(&root, "b.txt"), 2);
        assert_eq!(resolved_entry(&root, "c.txt"), 3);
        match root {
            FsTreeNode::Dir { children, .. } => assert_eq!(children.len(), 3),
            FsTreeNode::File { .. } => panic!("root should be Dir"),
        }
    }

    #[test]
    fn test_insert_nested_file_creates_intermediate_dirs() {
        let mut root = FsTreeNode::<i32>::root();
        root.insert_at(99, "a/b/c.txt".to_string());
        assert!(matches!(root.find("a").unwrap(), FsTreeNode::Dir { .. }));
        assert!(matches!(root.find("a/b").unwrap(), FsTreeNode::Dir { .. }));
        assert!(root.find("a/b/c.txt").is_some());
        assert_eq!(resolved_entry(&root, "a/b/c.txt"), 99);
    }

    #[test]
    fn test_insert_multiple_files_in_same_dir() {
        let mut root = FsTreeNode::<i32>::root();
        root.insert_at(1, "dir/x.txt".to_string());
        root.insert_at(2, "dir/y.txt".to_string());
        assert_eq!(resolved_entry(&root, "dir/x.txt"), 1);
        assert_eq!(resolved_entry(&root, "dir/y.txt"), 2);
    }

    #[test]
    fn test_find_empty_path_returns_self() {
        let root = FsTreeNode::<i32>::root();
        assert!(root.find("").is_some());
        assert!(root.find("/").is_some());
    }

    #[test]
    fn test_find_nonexistent_returns_none() {
        let mut root = FsTreeNode::<i32>::root();
        root.insert_at(1, "a/b.txt".to_string());
        assert!(root.find("nonexistent").is_none());
        assert!(root.find("a/nonexistent").is_none());
        assert!(root.find("a/b/c").is_none());
        assert!(root.find("x/y/z").is_none());
    }

    #[test]
    fn test_find_with_leading_slash() {
        let mut root = FsTreeNode::<i32>::root();
        root.insert_at(42, "file.txt".to_string());
        assert_eq!(resolved_entry(&root, "/file.txt"), 42);
    }

    #[test]
    fn test_find_on_file_returns_none_for_subpath() {
        let mut root = FsTreeNode::<i32>::root();
        root.insert_at(1, "file.txt".to_string());
        assert!(root.find("file.txt/nested").is_none());
    }

    #[test]
    fn test_index_html_tracked_on_insert() {
        let mut root = FsTreeNode::<i32>::root();
        root.insert_at(1, "a.txt".to_string());
        root.insert_at(2, "index.html".to_string());
        root.insert_at(3, "b.txt".to_string());
        let FsTreeNode::Dir {
            index_html_index, ..
        } = root
        else {
            panic!("root should be a Dir");
        };
        assert_eq!(index_html_index, Some(1));
    }

    #[test]
    fn test_recursive_sort_orders_children() {
        let mut root = FsTreeNode::<i32>::root();
        root.insert_at(1, "z.txt".to_string());
        root.insert_at(2, "m.txt".to_string());
        root.insert_at(3, "a.txt".to_string());
        root.recursive_sort();
        let FsTreeNode::Dir { children, .. } = root else {
            panic!("root should be a Dir");
        };
        assert_eq!(children[0].name(), "a.txt");
        assert_eq!(children[1].name(), "m.txt");
        assert_eq!(children[2].name(), "z.txt");
    }

    #[test]
    fn test_recursive_sort_updates_index_html_index() {
        let mut root = FsTreeNode::<i32>::root();
        root.insert_at(1, "z.txt".to_string());
        root.insert_at(2, "index.html".to_string());
        root.insert_at(3, "a.txt".to_string());
        root.recursive_sort();
        let FsTreeNode::Dir {
            children,
            index_html_index,
            ..
        } = root
        else {
            panic!("root should be a Dir");
        };
        assert_eq!(index_html_index, Some(1));
        assert_eq!(children[0].name(), "a.txt");
        assert_eq!(children[1].name(), "index.html");
        assert_eq!(children[2].name(), "z.txt");
    }

    #[test]
    fn test_recursive_sort_nested_dirs() {
        let mut root = FsTreeNode::<i32>::root();
        root.insert_at(1, "b/c.txt".to_string());
        root.insert_at(2, "a/d.txt".to_string());
        root.insert_at(3, "b/a.txt".to_string());
        root.recursive_sort();
        let FsTreeNode::Dir { ref children, .. } = root else {
            panic!("root should be a Dir");
        };
        assert_eq!(children[0].name(), "a");
        assert_eq!(children[1].name(), "b");
        let FsTreeNode::Dir { children, .. } = root.find("b").unwrap() else {
            panic!("root should be a Dir");
        };
        assert_eq!(children[0].name(), "a.txt");
        assert_eq!(children[1].name(), "c.txt");
    }

    #[test]
    fn test_recursive_sort_no_index_html_does_not_update() {
        let mut root = FsTreeNode::<i32>::root();
        root.insert_at(1, "b.txt".to_string());
        root.insert_at(2, "a.txt".to_string());
        root.recursive_sort();
        let FsTreeNode::Dir {
            index_html_index, ..
        } = root
        else {
            panic!("root should be a Dir");
        };
        assert_eq!(index_html_index, None);
    }

    #[test]
    fn test_resolve_file_to_its_entry() {
        let mut root = FsTreeNode::<i32>::root();
        root.insert_at(7, "a/b.txt".to_string());
        assert!(matches!(root.resolve("a/b.txt"), Some(Target::Entry(7))));
    }

    #[test]
    fn test_resolve_dir_with_index_html_to_that_entry() {
        let mut root = FsTreeNode::<i32>::root();
        root.insert_at(1, "a/z.txt".to_string());
        root.insert_at(2, "a/index.html".to_string());
        root.recursive_sort();
        assert!(matches!(root.resolve("a"), Some(Target::Entry(2))));
        assert!(matches!(root.resolve("a/"), Some(Target::Entry(2))));
    }

    #[test]
    fn test_resolve_dir_without_index_html_to_a_listing() {
        let mut root = FsTreeNode::<i32>::root();
        root.insert_at(1, "a/b.txt".to_string());
        let Some(Target::Listing { is_root, children }) = root.resolve("a") else {
            panic!("a should be listed");
        };
        assert!(!is_root);
        assert_eq!(children.len(), 1);
        assert!(matches!(
            root.resolve("/"),
            Some(Target::Listing { is_root: true, .. })
        ));
    }

    /// A directory sharing the name can shadow `index.html` after sorting; it
    /// must not stand in for its parent.
    #[test]
    fn test_resolve_dir_with_index_html_dir_to_a_listing() {
        let mut root = FsTreeNode::<i32>::root();
        root.insert_at(1, "index.html/x.txt".to_string());
        root.insert_at(2, "index.html".to_string());
        root.recursive_sort();
        assert!(matches!(root.resolve(""), Some(Target::Listing { .. })));
    }

    #[test]
    fn test_resolve_missing_path_to_none() {
        let root = FsTreeNode::<i32>::root();
        assert!(root.resolve("nope").is_none());
    }
}
