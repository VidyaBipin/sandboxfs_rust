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

#![cfg(test)]

use fuse;
use nix::{sys, unistd};
use std::collections::HashMap;
use std::fs;
use std::os::unix;
use std::path::PathBuf;
use tempfile::{TempDir, tempdir};

/// Holds a temporary directory and files of all possible kinds within it.
///
/// The directory (including all of its contents) is removed when this object is dropped.
pub struct AllFileTypes {
    #[allow(unused)]  // Must retain to delay directory deletion.
    root: TempDir,

    /// Collection of test files keyed by their type.
    ///
    /// Tests should iterate over this map and consume all entries to ensure all possible file types
    /// are verified everywhere.  Prefer using `match` on the key to achieve this.
    pub entries: HashMap<fuse::FileType, PathBuf>,
}

impl AllFileTypes {
    /// Creates a new temporary directory with files of all possible kinds within it.
    pub fn new() -> Self {
        let root = tempdir().unwrap();

        let mut entries: HashMap<fuse::FileType, PathBuf> = HashMap::new();

        if unistd::getuid().is_root() {
            let block_device = root.path().join("block_device");
            sys::stat::mknod(
                &block_device, sys::stat::SFlag::S_IFBLK, sys::stat::Mode::S_IRUSR, 50).unwrap();
            entries.insert(fuse::FileType::BlockDevice, block_device);

            let char_device = root.path().join("char_device");
            sys::stat::mknod(
                &char_device, sys::stat::SFlag::S_IFCHR, sys::stat::Mode::S_IRUSR, 50).unwrap();
            entries.insert(fuse::FileType::CharDevice, char_device);
        } else {
            warn!("Not running as root; cannot create block/char devices");
        }

        let directory = root.path().join("dir");
        fs::create_dir(&directory).unwrap();
        entries.insert(fuse::FileType::Directory, directory);

        let named_pipe = root.path().join("named_pipe");
        unistd::mkfifo(&named_pipe, sys::stat::Mode::S_IRUSR).unwrap();
        entries.insert(fuse::FileType::NamedPipe, named_pipe);

        let regular = root.path().join("regular");
        drop(fs::File::create(&regular).unwrap());
        entries.insert(fuse::FileType::RegularFile, regular);

        let socket = root.path().join("socket");
        drop(unix::net::UnixListener::bind(&socket).unwrap());
        entries.insert(fuse::FileType::Socket, socket);

        let symlink = root.path().join("symlink");
        unix::fs::symlink("irrelevant", &symlink).unwrap();
        entries.insert(fuse::FileType::Symlink, symlink);

        AllFileTypes { root, entries }
    }
}
