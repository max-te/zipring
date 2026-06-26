use rc_zip::parse::Entry;

pub(crate) enum FsTreeNode<E = Entry> {
    Dir {
        name: String,
        children: Vec<FsTreeNode<E>>,
        entry: Option<E>,
        is_root: bool,
        index_html_index: Option<usize>,
    },
    File {
        name: String,
        entry: E,
    },
}

impl<E> std::fmt::Debug for FsTreeNode<E> {
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

impl<E> FsTreeNode<E> {
    pub(crate) fn insert_at(&mut self, new_entry: E, path: String) {
        let FsTreeNode::Dir {
            children,
            index_html_index,
            ..
        } = self
        else {
            panic!("Cannot insert into FsTreeNode::File")
        };

        if let Some(separator) = path.chars().position(|c| c == '/') {
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
                    entry: None,
                    is_root: false,
                    index_html_index: None,
                });
                children
                    .last_mut()
                    .expect("should find the just inserted child")
            };

            if tail.len() > 1 {
                // Recurse
                let tail = tail.split_off(1);
                child.insert_at(new_entry, tail);
            } else {
                // We are at the insertion site of a directory.
                match child {
                    FsTreeNode::Dir {
                        entry: dir_entry, ..
                    } => {
                        let _ = dir_entry.insert(new_entry);
                    }
                    FsTreeNode::File { .. } => unreachable!(),
                }
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
            entry: None,
            is_root: true,
            index_html_index: None,
        }
    }

    pub(crate) fn name(&self) -> &str {
        match self {
            FsTreeNode::Dir { name, .. } | FsTreeNode::File { name, .. } => name,
        }
    }

    pub(crate) fn entry(&self) -> Option<&E> {
        match self {
            FsTreeNode::Dir { entry, .. } => entry.as_ref(),
            FsTreeNode::File { entry, .. } => Some(entry),
        }
    }

    pub(crate) fn find(&self, path: &str) -> Option<&Self> {
        if path.is_empty() || path == "/" {
            return Some(self);
        }

        let (head, tail) = match path.chars().position(|c| c == '/') {
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
            entry: None,
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

    #[test]
    fn test_root_creates_empty_dir() {
        let root = FsTreeNode::<i32>::root();
        assert_eq!(root.name(), "");
        assert!(root.entry().is_none());
        match &root {
            FsTreeNode::Dir { is_root, .. } => assert!(is_root),
            _ => panic!("root should be a Dir"),
        }
    }

    #[test]
    fn test_name_returns_name() {
        assert_eq!(dir("mydir").name(), "mydir");
        assert_eq!(file("f.txt", 42).name(), "f.txt");
    }

    #[test]
    fn test_entry_returns_entry() {
        assert_eq!(*file("f.txt", 99).entry().unwrap(), 99);
        assert!(dir("d").entry().is_none());
        assert!(FsTreeNode::<i32>::root().entry().is_none());
    }

    #[test]
    fn test_dir_entry_via_trailing_slash() {
        let mut root = FsTreeNode::<i32>::root();
        root.insert_at(42, "subdir/".to_string());
        let subdir = root.find("subdir").unwrap();
        assert!(matches!(subdir, FsTreeNode::Dir { .. }));
        assert_eq!(*subdir.entry().unwrap(), 42);
    }

    #[test]
    fn test_insert_file_at_root() {
        let mut root = FsTreeNode::<i32>::root();
        root.insert_at(42, "hello.txt".to_string());
        let found = root.find("hello.txt").unwrap();
        assert_eq!(found.name(), "hello.txt");
        assert_eq!(*found.entry().unwrap(), 42);
        assert!(matches!(found, FsTreeNode::File { .. }));
    }

    #[test]
    fn test_insert_multiple_files_at_root() {
        let mut root = FsTreeNode::<i32>::root();
        root.insert_at(1, "a.txt".to_string());
        root.insert_at(2, "b.txt".to_string());
        root.insert_at(3, "c.txt".to_string());
        assert_eq!(*root.find("a.txt").unwrap().entry().unwrap(), 1);
        assert_eq!(*root.find("b.txt").unwrap().entry().unwrap(), 2);
        assert_eq!(*root.find("c.txt").unwrap().entry().unwrap(), 3);
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
        assert_eq!(*root.find("a/b/c.txt").unwrap().entry().unwrap(), 99);
    }

    #[test]
    fn test_insert_multiple_files_in_same_dir() {
        let mut root = FsTreeNode::<i32>::root();
        root.insert_at(1, "dir/x.txt".to_string());
        root.insert_at(2, "dir/y.txt".to_string());
        assert_eq!(*root.find("dir/x.txt").unwrap().entry().unwrap(), 1);
        assert_eq!(*root.find("dir/y.txt").unwrap().entry().unwrap(), 2);
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
        assert_eq!(*root.find("/file.txt").unwrap().entry().unwrap(), 42);
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
        match &root {
            FsTreeNode::Dir {
                index_html_index, ..
            } => {
                assert_eq!(*index_html_index, Some(1));
            }
            _ => panic!("root should be Dir"),
        }
    }

    #[test]
    fn test_recursive_sort_orders_children() {
        let mut root = FsTreeNode::<i32>::root();
        root.insert_at(1, "z.txt".to_string());
        root.insert_at(2, "m.txt".to_string());
        root.insert_at(3, "a.txt".to_string());
        root.recursive_sort();
        match &root {
            FsTreeNode::Dir { children, .. } => {
                assert_eq!(children[0].name(), "a.txt");
                assert_eq!(children[1].name(), "m.txt");
                assert_eq!(children[2].name(), "z.txt");
            }
            _ => panic!("root should be Dir"),
        }
    }

    #[test]
    fn test_recursive_sort_updates_index_html_index() {
        let mut root = FsTreeNode::<i32>::root();
        root.insert_at(1, "z.txt".to_string());
        root.insert_at(2, "index.html".to_string());
        root.insert_at(3, "a.txt".to_string());
        root.recursive_sort();
        match &root {
            FsTreeNode::Dir {
                index_html_index,
                children,
                ..
            } => {
                assert_eq!(index_html_index, &Some(1));
                assert_eq!(children[0].name(), "a.txt");
                assert_eq!(children[1].name(), "index.html");
                assert_eq!(children[2].name(), "z.txt");
            }
            _ => panic!("root should be Dir"),
        }
    }

    #[test]
    fn test_recursive_sort_nested_dirs() {
        let mut root = FsTreeNode::<i32>::root();
        root.insert_at(1, "b/c.txt".to_string());
        root.insert_at(2, "a/d.txt".to_string());
        root.insert_at(3, "b/a.txt".to_string());
        root.recursive_sort();
        match &root {
            FsTreeNode::Dir { children, .. } => {
                assert_eq!(children[0].name(), "a");
                assert_eq!(children[1].name(), "b");
            }
            _ => panic!("root should be Dir"),
        }
        match root.find("b").unwrap() {
            FsTreeNode::Dir { children, .. } => {
                assert_eq!(children[0].name(), "a.txt");
                assert_eq!(children[1].name(), "c.txt");
            }
            _ => panic!("b should be Dir"),
        }
    }

    #[test]
    fn test_recursive_sort_no_index_html_does_not_update() {
        let mut root = FsTreeNode::<i32>::root();
        root.insert_at(1, "b.txt".to_string());
        root.insert_at(2, "a.txt".to_string());
        root.recursive_sort();
        match &root {
            FsTreeNode::Dir {
                index_html_index, ..
            } => {
                assert_eq!(*index_html_index, None);
            }
            _ => panic!("root should be Dir"),
        }
    }
}
