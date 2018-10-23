// Copyright 2018 Google Inc.
//
// Licensed under the Apache License, Version 2.0 (the "License"); you may not
// use this file except in compliance with the License.  You may obtain a copy
// of the License at:
//
//     http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS, WITHOUT
// WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.  See the
// License for the specific language governing permissions and limitations
// under the License.

#![cfg_attr(feature = "cargo-clippy", allow(redundant_field_names))]

#[macro_use] extern crate failure;
extern crate fuse;
#[macro_use] extern crate log;
extern crate nix;
#[cfg(test)] extern crate tempdir;
extern crate time;

use failure::{Error, ResultExt};
use std::collections::HashMap;
use std::ffi::OsStr;
use std::fs;
use std::os::unix::ffi::OsStrExt;
use std::path::{Component, Path, PathBuf};
use std::result::Result;
use std::sync::{Arc, Mutex};
use std::sync::atomic::{AtomicUsize, Ordering};
use time::Timespec;

mod nodes;
#[cfg(test)] mod testutils;

// TODO(jmmv): Make configurable via a flag and store inside SandboxFS.
pub const TTL: Timespec = Timespec { sec: 60, nsec: 0 };

/// An error indicating that a mapping specification (coming from the command line or from a
/// reconfiguration operation) is invalid.
#[derive(Debug, Eq, Fail, PartialEq)]
pub enum MappingError {
    /// A path was required to be absolute but wasn't.
    #[fail(display = "path {:?} is not absolute", path)]
    PathNotAbsolute {
        path: PathBuf,
    },

    /// A path contains non-normalized components (like "..").
    #[fail(display = "path {:?} is not normalized", path)]
    PathNotNormalized {
        path: PathBuf,
    },
}

/// Mapping describes how an individual path within the sandbox is connected to an external path
/// in the underlying file system.
#[derive(Debug, Eq, PartialEq)]
pub struct Mapping {
    path: PathBuf,
    underlying_path: PathBuf,
    writable: bool,
}
impl Mapping {
    /// Creates a new mapping from the individual components.
    ///
    /// `path` is the inside the sandbox's mount point where the `underlying_path` is exposed.
    /// Both must be absolute paths.  `path` must also not contain dot-dot components, though it
    /// may contain dot components and repeated path separators.
    pub fn new(path: PathBuf, underlying_path: PathBuf, writable: bool)
        -> Result<Mapping, MappingError> {
        if !path.is_absolute() {
            return Err(MappingError::PathNotAbsolute { path });
        }
        let is_normalized = {
            let mut components = path.components();
            assert_eq!(components.next(), Some(Component::RootDir), "Path expected to be absolute");
            let is_normal: fn(&Component) -> bool = |c| match c {
                Component::CurDir => panic!("Dot components ought to have been skipped"),
                Component::Normal(_) => true,
                Component::ParentDir | Component::Prefix(_) => false,
                Component::RootDir => panic!("Root directory should have already been handled"),
            };
            components.skip_while(is_normal).next().is_none()
        };
        if !is_normalized {
            return Err(MappingError::PathNotNormalized{ path });
        }

        if !underlying_path.is_absolute() {
            return Err(MappingError::PathNotAbsolute { path: underlying_path });
        }

        Ok(Mapping { path, underlying_path, writable })
    }

    /// Returns true if this is a mapping for the root directory.
    fn is_root(&self) -> bool {
        self.path.parent().is_none()
    }
}

/// Monotonically-increasing generator of identifiers.
pub struct IdGenerator {
    last_id: AtomicUsize,
}

impl IdGenerator {
    /// Constructs a new generator that starts at the given value.
    fn new(start_value: u64) -> Self {
        IdGenerator { last_id: AtomicUsize::new(start_value as usize) }
    }

    /// Obtains a new identifier.
    pub fn next(&self) -> u64 {
        let id = self.last_id.fetch_add(1, Ordering::AcqRel);
        // TODO(https://github.com/rust-lang/rust/issues/51577): Drop :: prefix.
        if id >= ::std::u64::MAX as usize {
            panic!("Ran out of identifiers");
        }
        id as u64
    }
}

/// Cache of sandboxfs nodes indexed by their underlying path.
///
/// This cache is critical to offer good performance during reconfigurations: if the identity of an
/// underlying file changes across reconfigurations, the kernel will think it's a different file
/// (even if it may not be) and will therefore not be able to take advantage of any caches.  You
/// would think that avoiding kernel cache invalidations during the reconfiguration itself (e.g. if
/// file `A` was mapped and is still mapped now, don't invalidate it) would be sufficient to avoid
/// this problem, but it's not: `A` could be mapped, then unmapped, and then remapped again in three
/// different reconfigurations, and we'd still not want to lose track of it.
///
/// Nodes should be inserted in this cache at creation time and removed from it when explicitly
/// deleted by the user (because there is a chance they'll be recreated, and at that point we truly
/// want to reload the data from disk).
///
/// TODO(jmmv): There currently is no cache expiration, which means that memory usage can grow
/// unboundedly.  A preliminary attempt at expiring cache entries on a node's forget handler sounded
/// promising (because then cache expiration would be delegated to the kernel)... but, on Linux, the
/// kernel seems to be calling this very eagerly, rendering our cache useless.  I did not track down
/// what exactly triggered the forget notifications though.
#[derive(Default)]
pub struct Cache {
    entries: Mutex<HashMap<PathBuf, Arc<nodes::Node>>>,
}

impl Cache {
    /// Gets a mapped node from the cache or creates a new one if not yet cached.
    ///
    /// The returned node represents the given underlying path uniquely.  If creation is needed, the
    /// created node uses the given type and writable settings.
    pub fn get_or_create(&self, ids: &IdGenerator, underlying_path: &Path, attr: &fs::Metadata,
        writable: bool) -> Arc<nodes::Node> {
        if attr.is_dir() {
            // Directories cannot be cached because they contain entries that are created only
            // in memory based on the mappings configuration.
            //
            // TODO(jmmv): Actually, they *could* be cached, but it's hard.  Investigate doing so
            // after quantifying how much it may benefit performance.
            return nodes::Dir::new_mapped(ids.next(), underlying_path, attr, writable);
        }

        let mut entries = self.entries.lock().unwrap();

        if let Some(node) = entries.get(underlying_path) {
            if node.writable() == writable {
                // We have a match from the cache!  Return it immediately.
                //
                // It is tempting to ensure that the type of the cached node matches the type we
                // want to return based on the metadata we have now in `attr`... but doing so does
                // not really prevent problems: the type of the underlying file can change at any
                // point in time.  We could check this here and the type could change immediately
                // afterwards behind our backs, so don't bother.
                return node.clone();
            }

            // We had a match... but node writability has changed; recreate the node.
            //
            // You may wonder why we care about this and not the file type as described above: the
            // reason is that the writability property is a setting of the mappings, not a property
            // of the underlying files, and thus it's a setting that we fully control and must keep
            // correct across reconfigurations or across different mappings of the same files.
            info!("Missed node caching opportunity because writability has changed for {:?}",
                underlying_path)
        }

        let node: Arc<nodes::Node> = if attr.is_dir() {
            panic!("Directory entries cannot be cached and are handled above");
        } else if attr.file_type().is_symlink() {
            nodes::Symlink::new_mapped(ids.next(), underlying_path, attr, writable)
        } else {
            nodes::File::new_mapped(ids.next(), underlying_path, attr, writable)
        };
        entries.insert(underlying_path.to_path_buf(), node.clone());
        node
    }
}

/// FUSE file system implementation of sandboxfs.
struct SandboxFS {
    /// Monotonically-increasing generator of identifiers for this file system instance.
    ids: IdGenerator,

    /// Mapping of inode numbers to in-memory nodes that tracks all files known by sandboxfs.
    nodes: Arc<Mutex<HashMap<u64, Arc<nodes::Node>>>>,

    /// Mapping of handle numbers to file open handles.
    handles: Arc<Mutex<HashMap<u64, Arc<nodes::Handle>>>>,

    /// Cache of sandboxfs nodes indexed by their underlying path.
    cache: Arc<Cache>,
}

/// Creates the initial node hierarchy based on a collection of `mappings`.
fn create_root(mappings: &[Mapping], ids: &IdGenerator, cache: &Cache)
    -> Result<Arc<nodes::Node>, Error> {
    let now = time::get_time();

    let (root, rest) = if mappings.is_empty() {
        (nodes::Dir::new_empty(ids.next(), None, now), mappings)
    } else {
        let first = &mappings[0];
        if first.is_root() {
            let fs_attr = fs::symlink_metadata(&first.underlying_path)
                .context(format!("Failed to map root: stat failed for {:?}",
                                 &first.underlying_path))?;
            ensure!(fs_attr.is_dir(), "Failed to map root: {:?} is not a directory",
                    &first.underlying_path);
            (nodes::Dir::new_mapped(ids.next(), &first.underlying_path, &fs_attr, first.writable),
                &mappings[1..])
        } else {
            (nodes::Dir::new_empty(ids.next(), None, now), mappings)
        }
    };

    for mapping in rest {
        let components = mapping.path.components().collect::<Vec<_>>();
        debug_assert_eq!(Component::RootDir, components[0],
            "Paths in mappings are always absolute");
        root.map(&components[1..], &mapping.underlying_path, mapping.writable, &ids, &cache)
            .context(format!("Failed to map {:?}", &mapping.path))?;
    }

    Ok(root)
}

impl SandboxFS {
    /// Creates a new `SandboxFS` instance.
    fn new(mappings: &[Mapping]) -> Result<SandboxFS, Error> {
        let ids = IdGenerator::new(fuse::FUSE_ROOT_ID);
        let cache = Cache::default();

        let mut nodes = HashMap::new();
        let root = create_root(mappings, &ids, &cache)?;
        assert_eq!(fuse::FUSE_ROOT_ID, root.inode());
        nodes.insert(root.inode(), root);

        Ok(SandboxFS {
            ids: ids,
            nodes: Arc::from(Mutex::from(nodes)),
            handles: Arc::from(Mutex::from(HashMap::new())),
            cache: Arc::from(cache),
        })
    }

    /// Gets a node given its `inode`.
    ///
    /// We assume that the inode number is valid and that we have a known node for it; otherwise,
    /// we crash.  The rationale for this is that this function is always called on inode numbers
    /// requested by the kernel, and we can trust that the kernel will only ever ask us for inode
    /// numbers we have previously told it about.
    fn find_node(&mut self, inode: u64) -> Arc<nodes::Node> {
        let nodes = self.nodes.lock().unwrap();
        nodes.get(&inode).expect("Kernel requested unknown inode").clone()
    }

    /// Gets a handle given its identifier.
    ///
    /// We assume that the identifier is valid and that we have a known handle for it; otherwise,
    /// we crash.  The rationale for this is that this function is always called on handles
    /// requested by the kernel, and we can trust that the kernel will only ever ask us for handles
    /// numbers we have previously told it about.
    fn find_handle(&mut self, fh: u64) -> Arc<nodes::Handle> {
        let handles = self.handles.lock().unwrap();
        handles.get(&fh).expect("Kernel requested unknown handle").clone()
    }
}

impl fuse::Filesystem for SandboxFS {
    fn create(&mut self, _req: &fuse::Request, _parent: u64, _name: &OsStr, _mode: u32, _flags: u32,
        _reply: fuse::ReplyCreate) {
        panic!("Required RW operation not yet implemented");
    }

    fn getattr(&mut self, _req: &fuse::Request, inode: u64, reply: fuse::ReplyAttr) {
        let node = self.find_node(inode);
        match node.getattr() {
            Ok(attr) => reply.attr(&TTL, &attr),
            Err(e) => reply.error(e.errno_as_i32()),
        }
    }

    fn link(&mut self, _req: &fuse::Request, _inode: u64, _newparent: u64, _newname: &OsStr,
        _reply: fuse::ReplyEntry) {
        panic!("Required RW operation not yet implemented");
    }

    fn lookup(&mut self, _req: &fuse::Request, parent: u64, name: &OsStr, reply: fuse::ReplyEntry) {
        let dir_node = self.find_node(parent);
        match dir_node.lookup(name, &self.ids, &self.cache) {
            Ok((node, attr)) => {
                {
                    let mut nodes = self.nodes.lock().unwrap();
                    if !nodes.contains_key(&node.inode()) {
                        nodes.insert(node.inode(), node);
                    }
                }
                reply.entry(&TTL, &attr, 0);
            },
            Err(e) => reply.error(e.errno_as_i32()),
        }
    }

    fn mkdir(&mut self, _req: &fuse::Request, _parent: u64, _name: &OsStr, _mode: u32,
        _reply: fuse::ReplyEntry) {
        panic!("Required RW operation not yet implemented");
    }

    fn mknod(&mut self, _req: &fuse::Request, _parent: u64, _name: &OsStr, _mode: u32, _rdev: u32,
        _reply: fuse::ReplyEntry) {
        panic!("Required RW operation not yet implemented");
    }

    fn open(&mut self, _req: &fuse::Request, inode: u64, flags: u32, reply: fuse::ReplyOpen) {
        let node = self.find_node(inode);

        match node.open(flags) {
            Ok(handle) => {
                let fh = self.ids.next();
                {
                    let mut handles = self.handles.lock().unwrap();
                    handles.insert(fh, handle);
                }
                reply.opened(fh, flags);
            },
            Err(e) => reply.error(e.errno_as_i32()),
        }
    }

    fn read(&mut self, _req: &fuse::Request, _inode: u64, fh: u64, offset: i64, size: u32,
        reply: fuse::ReplyData) {
        let handle = self.find_handle(fh);

        match handle.read(offset, size) {
            Ok(data) => reply.data(&data),
            Err(e) => reply.error(e.errno_as_i32()),
        }
    }

    fn readdir(&mut self, _req: &fuse::Request, inode: u64, _handle: u64, offset: i64,
               mut reply: fuse::ReplyDirectory) {
        if offset == 0 {
            let node = self.find_node(inode);
            match node.readdir(&self.ids, &self.cache, &mut reply) {
                Ok(()) => reply.ok(),
                Err(e) => reply.error(e.errno_as_i32()),
            }
        } else {
            assert!(offset > 0, "Do not know what to do with a negative offset");
            // Our node.readdir() implementation reads the whole directory in one go.  Therefore,
            // if we get an offset different than zero, it's because the kernel has already
            // completed the first read and is asking us for extra entries -- of which there will
            // be none.
            reply.ok();
        }
    }

    fn readlink(&mut self, _req: &fuse::Request, inode: u64, reply: fuse::ReplyData) {
        let node = self.find_node(inode);
        match node.readlink() {
            Ok(target) => reply.data(target.as_os_str().as_bytes()),
            Err(e) => reply.error(e.errno_as_i32()),
        }
    }

    fn release(&mut self, _req: &fuse::Request, _inode: u64, fh: u64, _flags: u32, _lock_owner: u64,
        _flush: bool, reply: fuse::ReplyEmpty) {
        {
            let mut handles = self.handles.lock().unwrap();
            handles.remove(&fh).expect("Kernel tried to release unknown handle");
        }
        reply.ok();
    }

    fn rename(&mut self, _req: &fuse::Request, _parent: u64, _name: &OsStr, _newparent: u64,
        _newname: &OsStr, _reply: fuse::ReplyEmpty) {
        panic!("Required RW operation not yet implemented");
    }

    fn rmdir(&mut self, _req: &fuse::Request, _parent: u64, _name: &OsStr,
        _reply: fuse::ReplyEmpty) {
        panic!("Required RW operation not yet implemented");
    }

    fn setattr(&mut self, _req: &fuse::Request, _inode: u64, _mode: Option<u32>, _uid: Option<u32>,
        _gid: Option<u32>, _size: Option<u64>, _atime: Option<Timespec>, _mtime: Option<Timespec>,
        _fh: Option<u64>, _crtime: Option<Timespec>, _chgtime: Option<Timespec>,
        _bkuptime: Option<Timespec>, _flags: Option<u32>, _reply: fuse::ReplyAttr) {
        panic!("Required RW operation not yet implemented");
    }

    fn symlink(&mut self, _req: &fuse::Request, _parent: u64, _name: &OsStr, _link: &Path,
        _reply: fuse::ReplyEntry) {
        panic!("Required RW operation not yet implemented");
    }

    fn unlink(&mut self, _req: &fuse::Request, _parent: u64, _name: &OsStr,
        _reply: fuse::ReplyEmpty) {
        panic!("Required RW operation not yet implemented");
    }
}

/// Mounts a new sandboxfs instance on the given `mount_point` and maps all `mappings` within it.
pub fn mount(mount_point: &Path, mappings: &[Mapping]) -> Result<(), Error> {
    // TODO(jmmv): Support passing in arbitrary FUSE options from the command line, like "-o ro",
    // or expose a good subset of them.
    let options = ["-o", "fsname=sandboxfs"]
        .iter()
        .map(|o| o.as_ref())
        .collect::<Vec<&OsStr>>();
    let fs = SandboxFS::new(mappings)?;
    info!("Mounting file system onto {:?}", mount_point);
    fuse::mount(fs, &mount_point, &options).context(format!("mount on {:?} failed", mount_point))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempdir::TempDir;

    #[test]
    fn test_mapping_new_ok() {
        let mapping = Mapping::new(
            PathBuf::from("/foo/.///bar"),  // Must be absolute and normalized.
            PathBuf::from("/bar/./baz/../abc"),  // Must be absolute but needn't be normalized.
            false).unwrap();
        assert_eq!(PathBuf::from("/foo/bar"), mapping.path);
        assert_eq!(PathBuf::from("/bar/baz/../abc"), mapping.underlying_path);
        assert!(!mapping.writable);
    }

    #[test]
    fn test_mapping_new_path_is_not_absolute() {
        let err = Mapping::new(PathBuf::from("foo"), PathBuf::from("/bar"), false).unwrap_err();
        assert_eq!(MappingError::PathNotAbsolute { path: PathBuf::from("foo") }, err);
    }

    #[test]
    fn test_mapping_new_path_is_not_normalized() {
        let trailing_dotdot = PathBuf::from("/foo/..");
        assert_eq!(
            MappingError::PathNotNormalized { path: trailing_dotdot.clone() },
            Mapping::new(trailing_dotdot, PathBuf::from("/bar"), false).unwrap_err());

        let intermediate_dotdot = PathBuf::from("/foo/../bar/baz");
        assert_eq!(
            MappingError::PathNotNormalized { path: intermediate_dotdot.clone() },
            Mapping::new(intermediate_dotdot, PathBuf::from("/bar"), true).unwrap_err());
    }

    #[test]
    fn test_mapping_new_underlying_path_is_not_absolute() {
        let err = Mapping::new(PathBuf::from("/foo"), PathBuf::from("bar"), false).unwrap_err();
        assert_eq!(MappingError::PathNotAbsolute { path: PathBuf::from("bar") }, err);
    }

    #[test]
    fn test_mapping_is_root() {
        let irrelevant = PathBuf::from("/some/place");
        assert!(Mapping::new(PathBuf::from("/"), irrelevant.clone(), false).unwrap().is_root());
        assert!(Mapping::new(PathBuf::from("///"), irrelevant.clone(), false).unwrap().is_root());
        assert!(Mapping::new(PathBuf::from("/./"), irrelevant.clone(), false).unwrap().is_root());
        assert!(!Mapping::new(PathBuf::from("/a"), irrelevant.clone(), false).unwrap().is_root());
        assert!(!Mapping::new(PathBuf::from("/a/b"), irrelevant.clone(), false).unwrap().is_root());
    }

    #[test]
    fn id_generator_ok() {
        let ids = IdGenerator::new(10);
        assert_eq!(10, ids.next());
        assert_eq!(11, ids.next());
        assert_eq!(12, ids.next());
    }

    #[test]
    #[should_panic(expected = "Ran out of identifiers")]
    fn id_generator_exhaustion() {
        let ids = IdGenerator::new(std::u64::MAX);
        ids.next();  // OK, still at limit.
        ids.next();  // Should panic.
    }

    #[test]
    fn cache_behavior() {
        let tempdir = TempDir::new("test").unwrap();

        let dir1 = tempdir.path().join("dir1");
        fs::create_dir(&dir1).unwrap();
        let dir1attr = fs::symlink_metadata(&dir1).unwrap();

        let file1 = tempdir.path().join("file1");
        drop(fs::File::create(&file1).unwrap());
        let file1attr = fs::symlink_metadata(&file1).unwrap();

        let file2 = tempdir.path().join("file2");
        drop(fs::File::create(&file2).unwrap());
        let file2attr = fs::symlink_metadata(&file2).unwrap();

        let ids = IdGenerator::new(1);
        let cache = Cache::default();

        // Directories are not cached no matter what.
        assert_eq!(1, cache.get_or_create(&ids, &dir1, &dir1attr, false).inode());
        assert_eq!(2, cache.get_or_create(&ids, &dir1, &dir1attr, false).inode());
        assert_eq!(3, cache.get_or_create(&ids, &dir1, &dir1attr, true).inode());

        // Different files get different nodes.
        assert_eq!(4, cache.get_or_create(&ids, &file1, &file1attr, false).inode());
        assert_eq!(5, cache.get_or_create(&ids, &file2, &file2attr, true).inode());

        // Files we queried before but with different writability get different nodes.
        assert_eq!(6, cache.get_or_create(&ids, &file1, &file1attr, true).inode());
        assert_eq!(7, cache.get_or_create(&ids, &file2, &file2attr, false).inode());

        // We get cache hits when everything matches previous queries.
        assert_eq!(6, cache.get_or_create(&ids, &file1, &file1attr, true).inode());
        assert_eq!(7, cache.get_or_create(&ids, &file2, &file2attr, false).inode());

        // We don't get cache hits for nodes whose writability changed.
        assert_eq!(8, cache.get_or_create(&ids, &file1, &file1attr, false).inode());
        assert_eq!(9, cache.get_or_create(&ids, &file2, &file2attr, true).inode());
    }

    #[test]
    fn cache_nodes_support_all_file_types() {
        let ids = IdGenerator::new(1);
        let cache = Cache::default();

        for (_fuse_type, path) in testutils::AllFileTypes::new().entries {
            let fs_attr = fs::symlink_metadata(&path).unwrap();
            // The following panics if it's impossible to represent the given file type, which is
            // what we are testing.
            cache.get_or_create(&ids, &path, &fs_attr, false);
        }
    }
}
