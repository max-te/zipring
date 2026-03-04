use rc_zip::parse::Entry;

pub(crate) enum FsTreeNode {
    Dir {
        name: String,
        children: Vec<FsTreeNode>,
        entry: Option<Entry>,
        is_root: bool,
        index_html_index: Option<usize>,
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
    pub(crate) fn insert_at(&mut self, new_entry: Entry, path: String) {
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

    pub(crate) fn insert(&mut self, entry: Entry) {
        let path = entry.name.clone();
        self.insert_at(entry, path);
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
