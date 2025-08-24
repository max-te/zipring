use rc_zip::parse::Entry;

pub(crate) enum FsTreeNode {
    Dir {
        name: String,
        children: Vec<FsTreeNode>,
        entry: Option<Entry>,
        is_root: bool,
    },
    File {
        name: String,
        entry: Entry,
    },
}

impl std::fmt::Debug for FsTreeNode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Dir { name, children, .. } => f.debug_map().key(name).value(children).finish(),
            Self::File { name, .. } => name.fmt(f),
        }
    }
}

impl FsTreeNode {
    pub(crate) fn insert_at(&mut self, entry: Entry, path: String) {
        let FsTreeNode::Dir { children, .. } = self else {
            panic!("Cannot insert into FsTreeNode::File")
        };

        if let Some(separator) = path.chars().position(|c| c == '/') {
            let mut head = path;
            let mut tail = head.split_off(separator);

            let existing_child = children.iter_mut().find(|ch| match ch {
                FsTreeNode::Dir { name, .. } => head.eq(name),
                _ => false,
            });

            let child = match existing_child {
                Some(c) => c,
                None => {
                    let new_child = FsTreeNode::Dir {
                        name: head.to_owned(),
                        children: vec![],
                        entry: None,
                        is_root: false,
                    };
                    children.push(new_child);
                    children.last_mut().unwrap()
                }
            };

            if tail.len() > 1 {
                // Recurse
                let tail = tail.split_off(1);
                child.insert_at(entry, tail);
            } else {
                // We are at the insertion site of a directory.
                match child {
                    FsTreeNode::Dir {
                        entry: entry_slot, ..
                    } => {
                        let _ = entry_slot.insert(entry);
                    }
                    FsTreeNode::File { .. } => unreachable!(),
                }
            }
        } else {
            // Insertion site of a file
            let new_child = FsTreeNode::File {
                name: path.to_owned(),
                entry,
            };
            children.push(new_child);
        }
    }

    pub(crate) fn insert(&mut self, entry: Entry) {
        let path = entry.name.to_owned();
        self.insert_at(entry, path)
    }

    pub(crate) fn root() -> Self {
        FsTreeNode::Dir {
            name: String::new(),
            children: Vec::new(),
            entry: None,
            is_root: true,
        }
    }

    pub(crate) fn name(&self) -> &str {
        match self {
            FsTreeNode::Dir { name, .. } => name,
            FsTreeNode::File { name, .. } => name,
        }
    }

    pub(crate) fn entry(&self) -> Option<&Entry> {
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
        if let FsTreeNode::Dir { children, .. } = self {
            children.sort_by(|a, b| PartialOrd::partial_cmp(&a.name(), &b.name()).unwrap());
            children.iter_mut().for_each(FsTreeNode::recursive_sort);
        }
    }
}
