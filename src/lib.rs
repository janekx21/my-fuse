use std::{
    collections::{BTreeMap, LinkedList},
    ffi::CStr,
    io::{self},
    path::Path,
    sync::{Arc, RwLock, Weak},
    time::{Duration, SystemTime},
};

use fuse_backend_rs::{
    abi::fuse_abi::{Attr, FsOptions, Opcode, OpenOptions, stat64},
    api::{
        filesystem::{DirEntry, Entry, FileSystem},
        server::{MetricsHook, Server},
    },
    transport::{FuseChannel, FuseSession},
};
use log::{debug, error, info, trace};

/// The datamodel for the my-fuse filesystem
struct MyFileSystem<'a> {
    /// This vector maps index to inode.
    /// The index 0 is therefore the root node of the filesystem
    nodes: RwLock<Vec<Option<Arc<RwLock<Node>>>>>,

    /// This queue contains the inodes that can be used again.
    /// The nodes vector should have a None value in these places.
    reusable_inode_queue: RwLock<LinkedList<Inode>>,

    /// This BTree mapps absolute file paths to nodes and is an index for fast path lookups.
    /// A possible key could be "/path/to/a/file.txt". The root "/" is relative to the filesystem mount point.
    path_index: BTreeMap<&'a str, Weak<Arc<Node>>>,
}

impl<'a> MyFileSystem<'a> {
    pub fn new() -> MyFileSystem<'a> {
        MyFileSystem {
            path_index: BTreeMap::new(),
            nodes: RwLock::new(Vec::new()),
            reusable_inode_queue: RwLock::new(LinkedList::new()),
        }
    }
}

/// This node is a node in the filesystem
#[derive(Debug)]
struct Node {
    inode: Inode,
    inner: InnerNode,
}

impl Node {
    fn new_folder(inode: Inode) -> Self {
        Self {
            inode,
            inner: InnerNode::Folder(Folder {
                entries: BTreeMap::new(),
            }),
        }
    }

    fn new_file(inode: Inode) -> Self {
        Self {
            inode,
            inner: InnerNode::File(File {
                data: Arc::new(RwLock::new(vec![])),
            }),
        }
    }

    fn get_entry(&self) -> Entry {
        let now = SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .unwrap()
            .as_secs();

        let attr = match &self.inner {
            InnerNode::File(file) => {
                let size = file.data.read().unwrap().len();
                Attr {
                    ino: self.inode,
                    mode: libc::S_IFREG | libc::S_IRWXU | libc::S_IRGRP | libc::S_IROTH,
                    uid: 1000,
                    gid: 100,
                    size: size as u64,
                    blksize: 1u32,
                    blocks: size as u64,
                    atime: now,
                    mtime: now,
                    ctime: now,
                    ..Default::default()
                }
            }
            InnerNode::Folder(folder) => Attr {
                ino: self.inode,
                mode: libc::S_IFDIR | libc::S_IRWXU | libc::S_IRGRP | libc::S_IROTH,
                uid: 1000,
                gid: 100,
                size: folder.entries.len() as u64,
                blksize: 1u32,
                blocks: folder.entries.len() as u64,
                atime: now,
                mtime: now,
                ctime: now,
                ..Default::default()
            },
        };

        Entry {
            inode: self.inode,
            generation: 0,
            attr: attr.into(),
            attr_flags: 0,
            attr_timeout: Duration::from_secs(1 << 32),
            entry_timeout: Duration::from_secs(1 << 32),
        }
    }
}

#[derive(Debug)]
enum InnerNode {
    File(File),
    Folder(Folder),
}

#[derive(Clone, Debug)]
struct File {
    pub data: Arc<RwLock<Vec<u8>>>,
}

#[derive(Debug)]
struct Folder {
    /// This BTree mapps a path segment to a child inode of this folder
    entries: BTreeMap<String, Inode>,
}

impl MyFileSystem<'_> {
    fn load(&self, inode: Inode) -> io::Result<Arc<RwLock<Node>>> {
        let nodes = self.nodes.read().unwrap();
        if let Some(node) = &nodes[inode as usize - 1] {
            let arc = node.clone();
            Ok(arc)
        } else {
            Err(io::Error::new(
                io::ErrorKind::NotFound,
                format!("Inode not found: {inode}"),
            ))
        }
    }

    fn next_inode(&self) -> Inode {
        if let Some(inode) = self.reusable_inode_queue.write().unwrap().pop_back() {
            inode
        } else {
            let mut nodes = self.nodes.write().unwrap();
            nodes.push(None);
            nodes.len() as Inode // This should return the last index + 1 (inode 0 is invalid). Now a None value
        }
    }
}

const MAX_FILE_SIZE: usize = 4294967296; // 4GiB / 4.29 GB
const BLOCK_SIZE: usize = 4096;

type Inode = u64;
type Handle = u64;

impl FileSystem for MyFileSystem<'_> {
    type Inode = Inode;
    type Handle = Handle;

    fn init(&self, capable: FsOptions) -> std::io::Result<FsOptions> {
        let _ = capable; // unused
        let root_node = Node::new_folder(1);
        let mut nodes = self.nodes.write().unwrap();
        nodes.push(Some(Arc::new(RwLock::new(root_node))));
        info!("Filesystem Init");
        Ok(FsOptions::ASYNC_READ
            | FsOptions::BIG_WRITES
            | FsOptions::ASYNC_DIO
            | FsOptions::PARALLEL_DIROPS
            | FsOptions::ZERO_MESSAGE_OPEN
            | FsOptions::ZERO_MESSAGE_OPENDIR)
    }

    fn lookup(
        &self,
        ctx: &fuse_backend_rs::api::filesystem::Context,
        parent: Self::Inode,
        name: &CStr,
    ) -> io::Result<Entry> {
        let _ = ctx;
        debug!("Lookup parent={parent} name={}", name.to_str().unwrap());

        self.load(parent).and_then(|e| {
            let node = &*e.read().unwrap();
            match &node.inner {
                InnerNode::Folder(folder) => {
                    if let Some(inode) = folder.entries.get(name.to_str().unwrap()) {
                        let rw_lock = self.load(*inode)?;
                        let entry = rw_lock.read().unwrap();
                        Ok(entry.get_entry())
                    } else {
                        Err(io::Error::new(
                            io::ErrorKind::NotFound,
                            format!("File or folder not found: {parent} {name:?}"),
                        ))
                    }
                }
                _ => Err(io::Error::new(
                    io::ErrorKind::NotFound,
                    format!("Parent not found for lookup: {parent}"),
                )),
            }
        })
    }

    fn getattr(
        &self,
        ctx: &fuse_backend_rs::api::filesystem::Context,
        inode: Self::Inode,
        handle: Option<Self::Handle>,
    ) -> io::Result<(stat64, Duration)> {
        let _ = ctx;
        debug!("Getting attributes for inode {inode} and handle {handle:?}");

        self.load(inode)
            .map(|e| e.read().unwrap().get_entry())
            .map(|e| (e.attr, Duration::from_secs(1 << 32)))
    }

    fn setattr(
        &self,
        ctx: &fuse_backend_rs::api::filesystem::Context,
        inode: Self::Inode,
        attr: stat64,
        handle: Option<Self::Handle>,
        valid: fuse_backend_rs::abi::fuse_abi::SetattrValid,
    ) -> io::Result<(stat64, Duration)> {
        let _ = valid;
        let _ = handle;
        let _ = ctx;
        debug!("setattr {attr:#?}");
        // The attributes are readonly so lets return just the attributes
        self.load(inode)
            .map(|e| {
                let node = e.write().unwrap();
                match &node.inner {
                    InnerNode::File(file) => {
                        // Truncate the file
                        let mut data = file.data.write().unwrap();
                        let target_size = attr.st_size as usize;
                        data.resize(target_size, 0);
                        data.shrink_to_fit();
                    }
                    InnerNode::Folder(_) => {}
                }
                node.get_entry()
            })
            .map(|e| (e.attr, Duration::from_secs(1 << 32)))
    }

    /////////////////////////////
    // Directory Operations
    /////////////////////////////

    fn mkdir(
        &self,
        ctx: &fuse_backend_rs::api::filesystem::Context,
        parent: Self::Inode,
        name: &CStr,
        mode: u32,
        umask: u32,
    ) -> io::Result<Entry> {
        debug!("mkdir {parent} {name:?}");
        let parent = self.load(parent)?;
        let mut parent = parent.write().unwrap();
        match &mut parent.inner {
            InnerNode::File(_) => Err(io::Error::new(
                io::ErrorKind::NotADirectory,
                format!("Can not create folder inside file {parent:?}"),
            )),
            InnerNode::Folder(folder) => {
                let inode = self.next_inode();
                let new_folder = Node::new_folder(inode);
                debug!("created node {new_folder:#?}");
                let entry = new_folder.get_entry();
                let mut nodes = self.nodes.write().unwrap();
                nodes[inode as usize - 1] = Some(Arc::new(RwLock::new(new_folder)));
                folder
                    .entries
                    .insert(name.to_str().unwrap().to_string(), inode);

                Ok(entry)
            }
        }
    }

    fn rmdir(
        &self,
        ctx: &fuse_backend_rs::api::filesystem::Context,
        parent: Self::Inode,
        name: &CStr,
    ) -> io::Result<()> {
        debug!("rmdir parent={parent} name={name:?}");
        let parent = self.load(parent)?;
        let mut parent = parent.write().unwrap();
        match &mut parent.inner {
            InnerNode::File(_) => Err(io::Error::new(
                io::ErrorKind::NotADirectory,
                format!("Can not remove folder inside file {parent:?}"),
            )),
            InnerNode::Folder(folder) => {
                if let Some(inode) = folder.entries.remove(name.to_str().unwrap()) {
                    drop(parent);
                    let mut nodes = self.nodes.write().unwrap();
                    nodes[inode as usize - 1] = None;
                    let mut queue = self.reusable_inode_queue.write().unwrap();
                    queue.push_back(inode);
                    debug!("Reusable inode queue {queue:?}");
                    Ok(())
                } else {
                    Err(io::Error::new(
                        io::ErrorKind::NotFound,
                        format!("Folder not found inside {parent:?}"),
                    ))
                }
            }
        }
    }

    fn readdir(
        &self,
        ctx: &fuse_backend_rs::api::filesystem::Context,
        inode: Self::Inode,
        handle: Self::Handle,
        size: u32,
        offset: u64,
        add_entry: &mut dyn FnMut(fuse_backend_rs::api::filesystem::DirEntry) -> io::Result<usize>,
    ) -> io::Result<()> {
        let _ = handle; // unused
        let _ = size;
        debug!("Reading directory {} with offset {offset}", inode);

        let node = self.load(inode)?;

        let node1 = &*node.read().unwrap();
        match &node1.inner {
            InnerNode::Folder(folder) => {
                for (i, (name, child_inode)) in folder
                    .entries
                    .iter()
                    .skip(offset as usize)
                    .take(size as usize)
                    .enumerate()
                {
                    let child_node = self.load(*child_inode)?;
                    let entry_type = match &child_node.read().unwrap().inner {
                        InnerNode::File(_) => libc::DT_REG,
                        InnerNode::Folder(_) => libc::DT_DIR,
                    };
                    add_entry(DirEntry {
                        ino: *child_inode,
                        offset: i as u64 + 1,
                        type_: entry_type as u32,
                        name: name.as_bytes(),
                    })?;
                }
                Ok(())
            }
            _ => Err(io::Error::new(
                io::ErrorKind::NotFound,
                format!("Readdir inode not found: {inode}"),
            )),
        }
    }

    /////////////////////////
    // File Operations
    /////////////////////////

    fn mknod(
        &self,
        ctx: &fuse_backend_rs::api::filesystem::Context,
        inode: Self::Inode,
        name: &CStr,
        mode: u32,
        rdev: u32,
        umask: u32,
    ) -> io::Result<Entry> {
        debug!("mknod {inode} {name:?}");
        let parent = self.load(inode)?;
        let mut parent = parent.write().unwrap();
        match &mut parent.inner {
            InnerNode::File(_) => Err(io::Error::new(
                io::ErrorKind::NotADirectory,
                format!("Can not create file inside file {parent:?}"),
            )),
            InnerNode::Folder(folder) => {
                let new_inode = self.next_inode();
                folder
                    .entries
                    .insert(name.to_str().unwrap().to_string(), new_inode);

                drop(parent);

                let new_file = Node::new_file(new_inode);
                debug!("created file {new_file:#?}");
                let entry = new_file.get_entry();
                let mut nodes = self.nodes.write().unwrap();
                nodes[new_inode as usize - 1] = Some(Arc::new(RwLock::new(new_file)));

                Ok(entry)
            }
        }
    }

    fn unlink(
        &self,
        ctx: &fuse_backend_rs::api::filesystem::Context,
        parent: Self::Inode,
        name: &CStr,
    ) -> io::Result<()> {
        debug!("unlink parent={parent} name={name:?}");
        let parent = self.load(parent)?;
        let mut parent = parent.write().unwrap();
        match &mut parent.inner {
            InnerNode::File(_) => Err(io::Error::new(
                io::ErrorKind::NotADirectory,
                format!("Can not remove file inside file {parent:?}"),
            )),
            InnerNode::Folder(folder) => {
                if let Some(inode) = folder.entries.remove(name.to_str().unwrap()) {
                    drop(parent);

                    let mut nodes = self.nodes.write().unwrap();

                    nodes[inode as usize - 1] = None;
                    let mut queue = self.reusable_inode_queue.write().unwrap();
                    queue.push_back(inode);
                    debug!("Reusable inode queue {queue:?}");
                    Ok(())
                } else {
                    Err(io::Error::new(
                        io::ErrorKind::NotFound,
                        format!("Folder not found inside {parent:?}"),
                    ))
                }
            }
        }
    }

    fn rename(
        &self,
        ctx: &fuse_backend_rs::api::filesystem::Context,
        olddir: Self::Inode,
        oldname: &CStr,
        newdir: Self::Inode,
        newname: &CStr,
        flags: u32,
    ) -> io::Result<()> {
        let old_dir_node = self.load(olddir)?;
        let old_dir_node = &mut old_dir_node.write().unwrap();

        if olddir == newdir {
            // Would deadlock because the folders are the same
            match &mut old_dir_node.inner {
                InnerNode::Folder(folder) => {
                    if let Some(inode) = folder.entries.remove(oldname.to_str().unwrap()) {
                        folder
                            .entries
                            .insert(newname.to_str().unwrap().to_string(), inode);
                        Ok(())
                    } else {
                        Err(io::Error::new(
                            io::ErrorKind::NotFound,
                            format!("File or folder not found: {olddir} {oldname:?}"),
                        ))
                    }
                }
                _ => Err(io::Error::new(
                    io::ErrorKind::NotADirectory,
                    format!("Parent is not a direcoty: {olddir}"),
                )),
            }
        } else {
            let new_dir_node = self.load(newdir)?;
            let new_dir_node = &mut new_dir_node.write().unwrap();
            match (&mut old_dir_node.inner, &mut new_dir_node.inner) {
                (InnerNode::Folder(old_folder), InnerNode::Folder(new_folder)) => {
                    if let Some(inode) = old_folder.entries.remove(oldname.to_str().unwrap()) {
                        new_folder
                            .entries
                            .insert(newname.to_str().unwrap().to_string(), inode);
                        Ok(())
                    } else {
                        Err(io::Error::new(
                            io::ErrorKind::NotFound,
                            format!("File or folder not found: {olddir} {oldname:?}"),
                        ))
                    }
                }
                _ => Err(io::Error::new(
                    io::ErrorKind::NotADirectory,
                    format!("Parent is not a direcoty: {olddir}"),
                )),
            }
        }
    }

    fn open(
        &self,
        ctx: &fuse_backend_rs::api::filesystem::Context,
        inode: Self::Inode,
        flags: u32,
        fuse_flags: u32,
    ) -> io::Result<(
        Option<Self::Handle>,
        fuse_backend_rs::abi::fuse_abi::OpenOptions,
        Option<u32>,
    )> {
        let _ = fuse_flags;
        let _ = flags;
        let _ = ctx;
        self.load(inode)?;
        Ok((None, OpenOptions::empty(), None))
    }

    fn read(
        &self,
        ctx: &fuse_backend_rs::api::filesystem::Context,
        inode: Self::Inode,
        handle: Self::Handle,
        w: &mut dyn fuse_backend_rs::api::filesystem::ZeroCopyWriter,
        size: u32,
        offset: u64,
        lock_owner: Option<u64>,
        flags: u32,
    ) -> io::Result<usize> {
        // unused
        let _ = flags;
        let _ = lock_owner;
        let _ = handle;
        let _ = ctx;
        debug!("Read {inode} with size {size} and offset {offset}");
        let node = self.load(inode)?;
        let node1 = &*node.read().unwrap();
        match &node1.inner {
            InnerNode::File(file) => {
                let offset = offset as usize;
                let size = size as usize;
                let data = file.data.read().unwrap();
                let mut range = offset..(offset + size);
                // if range.start >= MAX_FILE_SIZE {
                //     range.start = MAX_FILE_SIZE;
                // }
                // if range.end >= MAX_FILE_SIZE {
                //     range.end = MAX_FILE_SIZE;
                // }
                if range.start >= data.len() {
                    range.start = data.len();
                }
                if range.end >= data.len() {
                    range.end = data.len();
                }
                w.write_all(&data[range.clone()]).unwrap();
                let written = range.count();

                debug!("Reading with size {written}");

                Ok(written)
            }
            _ => Err(io::Error::new(
                io::ErrorKind::NotFound,
                format!("File could not be found: {inode}"),
            )),
        }
    }

    fn write(
        &self,
        ctx: &fuse_backend_rs::api::filesystem::Context,
        inode: Self::Inode,
        handle: Self::Handle,
        r: &mut dyn fuse_backend_rs::api::filesystem::ZeroCopyReader,
        size: u32,
        offset: u64,
        lock_owner: Option<u64>,
        delayed_write: bool,
        flags: u32,
        fuse_flags: u32,
    ) -> io::Result<usize> {
        let _ = delayed_write;
        let _ = lock_owner;
        let _ = ctx;
        debug!(
            "Write inode {inode} handle {handle} size {size} offset {offset} flags {flags} fuse_flags {fuse_flags} "
        );
        let node = self.load(inode)?;
        let node1 = &*node.read().unwrap();
        match &node1.inner {
            InnerNode::File(file) => {
                let mut data = file.data.write().unwrap();

                let mut buf = Vec::with_capacity(BLOCK_SIZE);
                let buf_size = r.read_to_end(&mut buf).unwrap();

                if buf_size != size as usize {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidInput,
                        format!(
                            "The buffer size({buf_size}) and size paramter({size}) are not the same"
                        ),
                    ));
                }

                let mut range = offset as usize..(offset as usize + buf_size);

                if range.start >= data.len() {
                    range.start = data.len();
                }
                if range.end >= data.len() {
                    range.end = data.len();
                }

                if range.start >= MAX_FILE_SIZE {
                    range.start = MAX_FILE_SIZE;
                }
                if range.end >= MAX_FILE_SIZE {
                    range.end = MAX_FILE_SIZE;
                }

                data.splice(range, buf);

                debug!("Writing to file {buf_size}");
                Ok(buf_size)
            }
            _ => Err(io::Error::new(
                io::ErrorKind::NotFound,
                format!("File could not be found {inode}"),
            )),
        }
    }

    fn flush(
        &self,
        ctx: &fuse_backend_rs::api::filesystem::Context,
        inode: Self::Inode,
        handle: Self::Handle,
        lock_owner: u64,
    ) -> io::Result<()> {
        let _ = lock_owner;
        let _ = handle;
        let _ = ctx;
        debug!("Flush {inode}");
        Ok(())
    }

    fn release(
        &self,
        ctx: &fuse_backend_rs::api::filesystem::Context,
        inode: Self::Inode,
        flags: u32,
        handle: Self::Handle,
        flush: bool,
        flock_release: bool,
        lock_owner: Option<u64>,
    ) -> io::Result<()> {
        let _ = lock_owner;
        let _ = flock_release;
        let _ = flush;
        let _ = handle;
        let _ = flags;
        let _ = ctx;
        debug!("Release {inode}");
        Ok(())
    }

    fn releasedir(
        &self,
        ctx: &fuse_backend_rs::api::filesystem::Context,
        inode: Self::Inode,
        flags: u32,
        handle: Self::Handle,
    ) -> io::Result<()> {
        Ok(())
    }
}

/// This struct is just used for logging all requests
struct LoggingMetricsHook {}

impl MetricsHook for LoggingMetricsHook {
    fn collect(&self, ih: &fuse_backend_rs::abi::fuse_abi::InHeader) {
        trace!("Begin request {:?}", Opcode::from(ih.opcode));
    }

    fn release(&self, oh: Option<&fuse_backend_rs::abi::fuse_abi::OutHeader>) {
        if let Some(e) = oh {
            info!("{e:?}");
        } else {
            trace!("End request");
        }
    }
}

pub struct ServerSession<'a> {
    server: Server<MyFileSystem<'a>>,
    pub session: Arc<RwLock<FuseSession>>,
    channel: FuseChannel,
}

impl ServerSession<'_> {
    pub fn new(mount_point: &str) -> Self {
        let filesystem = MyFileSystem::new();
        let server = Server::new(filesystem);
        let session = Arc::new(RwLock::new(
            FuseSession::new(Path::new(mount_point), "my-fuse", "", false).unwrap(),
        ));

        let channel = {
            let mut session = session.write().unwrap();
            session.set_allow_other(false);
            session.mount().unwrap();
            session.new_channel().unwrap()
        };

        Self {
            server,
            session,
            channel,
        }
    }

    pub fn start(&mut self) {
        let metrics_hook = LoggingMetricsHook {};

        info!("Running fuse");
        loop {
            match self.channel.get_request() {
                Ok(Some((reader, writer))) => {
                    self.server
                        .handle_message(reader, writer.into(), None, Some(&metrics_hook))
                        .unwrap_or_else(|e| {
                            error!("{e:?}");
                            0
                        });
                }
                Ok(None) => {
                    info!("Cant handle message");
                }
                Err(e) => {
                    error!("Request Error: {e}");
                    break;
                }
            }
        }
    }
}

impl Drop for ServerSession<'_> {
    fn drop(&mut self) {
        info!("Unmounting");
        {
            let mut session = self.session.write().unwrap();
            session.umount().unwrap();
        }
    }
}

pub mod test_util {
    use crate::ServerSession;
    use fuse_backend_rs::transport::FuseSession;

    use log::info;
    use std::{
        path::Path,
        sync::{Arc, RwLock},
        thread::{self, JoinHandle},
    };
    use tempdir::TempDir;

    pub struct TestFixture {
        session: Arc<RwLock<FuseSession>>,
        thread: JoinHandle<()>,
        tmp_dir: TempDir,
    }

    impl Default for TestFixture {
        fn default() -> Self {
            Self::new()
        }
    }

    impl TestFixture {
        pub fn new() -> Self {
            let tmp_dir = TempDir::new("my-fuse").unwrap();
            let tmp_dir_path = tmp_dir.path().to_str().unwrap().to_string();

            let mut server_session = ServerSession::new(tmp_dir_path.as_str());

            let session = server_session.session.clone();

            let thread = thread::spawn(move || {
                server_session.start();
            });

            Self {
                session,
                thread,
                tmp_dir,
            }
        }

        pub fn path(&self) -> &Path {
            self.tmp_dir.path()
        }
    }

    impl Drop for TestFixture {
        fn drop(&mut self) {
            info!("Drop test fixture");
            {
                let mut session = self.session.write().unwrap();
                session.umount().unwrap();
            }
            // TODO Lets join the thread
        }
    }
}

#[cfg(test)]
pub mod tests {
    use crate::{ServerSession, test_util::TestFixture};
    use fuse_backend_rs::transport::FuseSession;

    use itertools::Itertools;
    use log::info;
    use std::{
        fs,
        path::Path,
        sync::{Arc, RwLock},
        thread::{self, JoinHandle},
    };
    use tempdir::TempDir;

    #[test_log::test]
    fn mount_filesystem() {
        // Arrange
        let fixture = TestFixture::new();

        // Act

        let dir_content = fs::read_dir(fixture.path()).unwrap();

        // Assert
        assert_eq!(dir_content.count(), 0);
    }

    #[test_log::test]
    fn write_file() {
        // Arrange
        let fixture = TestFixture::new();

        // Act

        fs::write(fixture.path().join("test"), "test").unwrap();

        // Assert

        let dir_content = fs::read_dir(fixture.path()).unwrap();
        assert_eq!(dir_content.count(), 1);
    }

    #[test_log::test]
    fn read_file() {
        // Arrange
        let fixture = TestFixture::new();
        fs::write(fixture.path().join("test"), "test").unwrap();

        // Act

        let data = fs::read(fixture.path().join("test")).unwrap();

        // Assert

        let dir_content = fs::read_dir(fixture.path()).unwrap();
        assert_eq!(dir_content.count(), 1);

        let content = String::from_utf8(data).unwrap();

        assert_eq!(content.as_str(), "test");
    }

    #[test_log::test]
    fn mkdir() {
        // Arrange
        let fixture = TestFixture::new();

        // Act

        fs::create_dir(fixture.path().join("test")).unwrap();

        // Assert

        let dir_content = fs::read_dir(fixture.path())
            .unwrap()
            .flat_map(|x| x.ok())
            .collect_vec();
        assert_eq!(dir_content.len(), 1);

        let dir = dir_content.first().unwrap();
        let filename = dir.file_name();
        let name = filename.to_str().unwrap();

        assert_eq!(name, "test");
    }

    #[test_log::test]
    fn rmdir() {
        // Arrange
        let fixture = TestFixture::new();
        fs::create_dir(fixture.path().join("test")).unwrap();

        // Act

        fs::remove_dir(fixture.path().join("test")).unwrap();

        // Assert

        let dir_content = fs::read_dir(fixture.path())
            .unwrap()
            .flat_map(|x| x.ok())
            .collect_vec();
        assert_eq!(dir_content.len(), 0);
    }
}
