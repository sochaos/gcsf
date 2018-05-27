use super::{File, FileId};
use DriveFacade;
use drive3;
use fuse::{FileAttr, FileType, Filesystem, ReplyAttr, ReplyCreate, ReplyData, ReplyDirectory,
           ReplyEmpty, ReplyEntry, ReplyStatfs, ReplyWrite, Request};
use id_tree::InsertBehavior::*;
use id_tree::MoveBehavior::*;
use id_tree::RemoveBehavior::*;
use id_tree::{Node, NodeId, NodeIdError, Tree, TreeBuilder};
use std::collections::HashMap;
use std::collections::LinkedList;
use std::fmt;
use time::Timespec;

pub type Inode = u64;
pub type DriveId = String;

pub struct FileManager {
    tree: Tree<Inode>,
    pub files: HashMap<Inode, File>,
    pub node_ids: HashMap<Inode, NodeId>,
    pub drive_ids: HashMap<DriveId, Inode>,
    pub df: DriveFacade,
}

/// Deals with everything that involves local file managing. In turn, uses a DriveFacade in order
/// to ensure consistency between the local and remote (drive) state.
impl FileManager {
    pub fn with_drive_facade(df: DriveFacade) -> Self {
        let mut manager = FileManager {
            tree: TreeBuilder::new().with_node_capacity(500).build(),
            files: HashMap::new(),
            node_ids: HashMap::new(),
            drive_ids: HashMap::new(),
            df,
        };

        manager.populate();
        manager
    }

    // Recursively adds all files and directories shown in "My Drive".
    fn populate(&mut self) {
        let root = self.new_root_file();
        self.add_file(root, None);

        let mut queue: LinkedList<DriveId> = LinkedList::new();
        queue.push_back(self.df.root_id());

        while !queue.is_empty() {
            let parent_id = queue.pop_front().unwrap();
            for drive_file in self.df.get_all_files(Some(&parent_id)) {
                let mut file = File::from_drive_file(self.next_available_inode(), drive_file);

                if file.kind() == FileType::Directory {
                    queue.push_back(file.drive_id().unwrap());
                }

                // TODO: this makes everything slow; find a better solution
                // if file.is_drive_document() {
                //     let size = drive_facade
                //         .get_file_size(file.drive_id().as_ref().unwrap(), file.mime_type());
                //     file.attr.size = size;
                // }

                if self.contains(FileId::DriveId(parent_id.clone())) {
                    self.add_file(file, Some(FileId::DriveId(parent_id.clone())));
                } else {
                    self.add_file(file, None);
                }
            }
        }
    }

    fn new_root_file(&mut self) -> File {
        let mut drive_file = drive3::File::default();
        drive_file.id = Some(self.df.root_id());

        File {
            name: String::from("."),
            attr: FileAttr {
                ino: self.next_available_inode(),
                size: 0,
                blocks: 123,
                atime: Timespec { sec: 0, nsec: 0 },
                mtime: Timespec { sec: 0, nsec: 0 },
                ctime: Timespec { sec: 0, nsec: 0 },
                crtime: Timespec { sec: 0, nsec: 0 },
                kind: FileType::Directory,
                perm: 0o755,
                nlink: 2,
                uid: 0,
                gid: 0,
                rdev: 0,
                flags: 0,
            },
            drive_file: Some(drive_file),
        }
    }

    fn new_shared_with_me_file(&mut self) -> File {
        File {
            name: String::from("Shared with me"),
            attr: FileAttr {
                ino: self.next_available_inode(),
                size: 0,
                blocks: 123,
                atime: Timespec { sec: 0, nsec: 0 },
                mtime: Timespec { sec: 0, nsec: 0 },
                ctime: Timespec { sec: 0, nsec: 0 },
                crtime: Timespec { sec: 0, nsec: 0 },
                kind: FileType::Directory,
                perm: 0o755,
                nlink: 2,
                uid: 0,
                gid: 0,
                rdev: 0,
                flags: 0,
            },
            drive_file: None,
        }
    }

    pub fn next_available_inode(&self) -> Inode {
        (1..)
            .filter(|inode| !self.contains(FileId::Inode(*inode)))
            .take(1)
            .next()
            .unwrap()
    }

    pub fn contains(&self, file_id: FileId) -> bool {
        match file_id {
            FileId::Inode(inode) => self.node_ids.contains_key(&inode),
            FileId::DriveId(drive_id) => self.drive_ids.contains_key(&drive_id),
            FileId::NodeId(node_id) => self.tree.get(&node_id).is_ok(),
            FileId::ParentAndName { parent, name } => {
                self.get_file(FileId::ParentAndName { parent, name })
                    .is_some()
            }
        }
    }

    pub fn get_node_id(&self, file_id: FileId) -> Option<NodeId> {
        match file_id {
            FileId::Inode(inode) => self.node_ids.get(&inode).cloned(),
            FileId::DriveId(drive_id) => self.get_node_id(FileId::Inode(self.get_inode(
                FileId::DriveId(drive_id),
            ).unwrap())),
            FileId::NodeId(node_id) => Some(node_id),
            FileId::ParentAndName { parent, name } => {
                let inode = self.get_inode(FileId::ParentAndName { parent, name })?;
                self.get_node_id(FileId::Inode(inode))
            }
        }
    }

    pub fn get_drive_id(&self, id: FileId) -> Option<DriveId> {
        self.get_file(id)?.drive_id()
    }

    pub fn get_inode(&self, id: FileId) -> Option<Inode> {
        // debug!("get_inode({:?})", &id);
        match id {
            FileId::Inode(inode) => Some(inode),
            FileId::DriveId(drive_id) => self.drive_ids.get(&drive_id).cloned(),
            FileId::NodeId(node_id) => self.tree
                .get(&node_id)
                .map(|node| node.data())
                .ok()
                .cloned(),
            FileId::ParentAndName { parent, name } => self.get_children(FileId::Inode(parent))?
                .into_iter()
                .find(|child| child.name == name)
                .map(|child| child.inode()),
        }
    }

    pub fn get_children(&self, id: FileId) -> Option<Vec<&File>> {
        // debug!("get_children({:?})", &id);
        let node_id = self.get_node_id(id)?;
        let children: Vec<&File> = self.tree
            .children(&node_id)
            .unwrap()
            .map(|child| self.get_file(FileId::Inode(*child.data())))
            .filter(Option::is_some)
            .map(Option::unwrap)
            .collect();

        Some(children)
    }

    pub fn get_file(&self, id: FileId) -> Option<&File> {
        // debug!("get_file({:?})", &id);
        let inode = self.get_inode(id)?;
        self.files.get(&inode)
    }

    pub fn get_mut_file(&mut self, id: FileId) -> Option<&mut File> {
        let inode = self.get_inode(id)?;
        self.files.get_mut(&inode)
    }

    /// Creates a file on Drive and adds it to the local file tree.
    pub fn create_file(&mut self, mut file: File, parent: Option<FileId>) {
        let drive_id = self.df.create(file.drive_file.as_ref().unwrap());
        file.set_drive_id(drive_id);
        self.add_file(file, parent);
    }

    /// Adds a file to the local file tree. Does not communicate with Drive.
    pub fn add_file(&mut self, file: File, parent: Option<FileId>) {
        let node_id = match parent {
            Some(inode) => {
                info!("add file to parent inode = {:?}", inode);
                let parent_id = self.get_node_id(inode).unwrap();
                self.tree
                    .insert(Node::new(file.inode()), UnderNode(&parent_id))
                    .unwrap()
            }
            None => {
                info!("Adding file as root! This should only happen once.");
                self.tree.insert(Node::new(file.inode()), AsRoot).unwrap()
            }
        };

        self.node_ids.insert(file.inode(), node_id);
        file.drive_id()
            .and_then(|drive_id| self.drive_ids.insert(drive_id, file.inode()));
        self.files.insert(file.inode(), file);
    }

    pub fn write(&mut self, id: FileId, offset: usize, data: &[u8]) {
        let drive_id = self.get_drive_id(id).unwrap();
        self.df.write(drive_id, offset, data);
    }
}

impl fmt::Debug for FileManager {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(f, "FileManager(\n")?;

        if self.tree.root_node_id().is_none() {
            return write!(f, ")\n");
        }

        let mut stack: Vec<(u32, &NodeId)> = vec![(0, self.tree.root_node_id().unwrap())];

        while !stack.is_empty() {
            let (level, node_id) = stack.pop().unwrap();

            for _ in 0..level {
                write!(f, "\t")?;
            }

            let file = self.get_file(FileId::NodeId(node_id.clone())).unwrap();
            write!(f, "{:3} => {}\n", file.inode(), file.name)?;

            self.tree.children_ids(node_id).unwrap().for_each(|id| {
                stack.push((level + 1, id));
            });
        }

        write!(f, ")\n")
    }
}