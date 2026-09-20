//! The FUSE adapter: translates kernel requests into [`FsCore`] calls.
//!
//! Deliberately thin. Every rule about what may happen lives in `FsCore`,
//! where it is tested without a mount; this file only converts between the
//! kernel's vocabulary (inodes, handles, `errno`) and paths.

use std::{
    collections::HashMap,
    ffi::OsStr,
    sync::{
        Arc, Mutex, MutexGuard, PoisonError,
        atomic::{AtomicU64, Ordering},
    },
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use fuser::{
    AccessFlags, Errno, FileAttr, FileHandle, FileType, Filesystem, FopenFlags, Generation, INodeNo, KernelConfig,
    OpenAccMode, OpenFlags, RenameFlags, ReplyAttr, ReplyCreate, ReplyData, ReplyDirectory, ReplyEmpty, ReplyEntry,
    ReplyOpen, ReplyStatfs, ReplyWrite, ReplyXattr, Request, TimeOrNow, WriteFlags,
};
use icloud_core::{Attr, DirItem, FileKind, FsCore, IcPath, ProcessReader};

use crate::inodes::InodeTable;

/// How long the kernel may cache what we tell it about a name or a file.
/// Short, because changes made on other devices arrive through refreshes.
const TTL: Duration = Duration::from_secs(1);
const BLOCK: u64 = 512;

pub struct IcloudFs {
    core: Arc<FsCore>,
    inodes: Mutex<InodeTable>,
    /// Directory listings taken at `opendir`, so paging through a large
    /// folder lists it once rather than once per page.
    listings: Mutex<HashMap<u64, Arc<Vec<DirItem>>>>,
    next_listing: AtomicU64,
}

impl std::fmt::Debug for IcloudFs {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("IcloudFs").finish_non_exhaustive()
    }
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

fn errno(err: icloud_core::Errno) -> Errno {
    Errno::from_i32(err.raw_os_error())
}

fn to_time(secs: i64) -> SystemTime {
    UNIX_EPOCH + Duration::from_secs(u64::try_from(secs).unwrap_or(0))
}

fn to_secs(time: SystemTime) -> i64 {
    time.duration_since(UNIX_EPOCH).map_or(0, |d| i64::try_from(d.as_secs()).unwrap_or(i64::MAX))
}

fn file_type(kind: FileKind) -> FileType {
    match kind {
        FileKind::File => FileType::RegularFile,
        FileKind::Directory => FileType::Directory,
    }
}

fn file_attr(ino: u64, attr: &Attr) -> FileAttr {
    FileAttr {
        ino: INodeNo(ino),
        size: attr.size,
        blocks: attr.size.div_ceil(BLOCK),
        atime: to_time(attr.atime),
        mtime: to_time(attr.mtime),
        ctime: to_time(attr.ctime),
        crtime: to_time(attr.ctime),
        kind: file_type(attr.kind),
        perm: attr.perm,
        nlink: attr.nlink,
        uid: attr.uid,
        gid: attr.gid,
        rdev: 0,
        blksize: 4096,
        flags: 0,
    }
}

impl IcloudFs {
    pub fn new(core: Arc<FsCore>) -> Self {
        Self {
            core,
            inodes: Mutex::new(InodeTable::new()),
            listings: Mutex::new(HashMap::new()),
            next_listing: AtomicU64::new(1),
        }
    }

    fn path_of(&self, ino: INodeNo) -> Result<IcPath, Errno> {
        lock(&self.inodes).path(ino.0).cloned().ok_or(Errno::ENOENT)
    }

    /// The path of `name` inside directory `parent`.
    fn child_path(&self, parent: INodeNo, name: &OsStr) -> Result<IcPath, Errno> {
        let name = name.to_str().ok_or(Errno::EINVAL)?;
        self.path_of(parent)?.join(name).ok_or(Errno::EINVAL)
    }

    /// Attributes of `path` with its inode, assigning one if needed.
    fn attr_of(&self, path: &IcPath) -> Result<FileAttr, Errno> {
        let attr = self.core.getattr(path).map_err(errno)?;
        Ok(file_attr(lock(&self.inodes).ino_for(path), &attr))
    }
}

impl Filesystem for IcloudFs {
    fn init(&mut self, _req: &Request, _config: &mut KernelConfig) -> std::io::Result<()> {
        Ok(())
    }

    fn lookup(&self, _req: &Request, parent: INodeNo, name: &OsStr, reply: ReplyEntry) {
        let result = self.child_path(parent, name).and_then(|path| self.attr_of(&path));
        match result {
            Ok(attr) => reply.entry(&TTL, &attr, Generation(0)),
            Err(err) => reply.error(err),
        }
    }

    fn getattr(&self, _req: &Request, ino: INodeNo, _fh: Option<FileHandle>, reply: ReplyAttr) {
        match self.path_of(ino).and_then(|path| self.attr_of(&path)) {
            Ok(attr) => reply.attr(&TTL, &attr),
            Err(err) => reply.error(err),
        }
    }

    fn setattr(
        &self,
        _req: &Request,
        ino: INodeNo,
        mode: Option<u32>,
        uid: Option<u32>,
        gid: Option<u32>,
        size: Option<u64>,
        _atime: Option<TimeOrNow>,
        mtime: Option<TimeOrNow>,
        _ctime: Option<SystemTime>,
        _fh: Option<FileHandle>,
        _crtime: Option<SystemTime>,
        _chgtime: Option<SystemTime>,
        _bkuptime: Option<SystemTime>,
        _flags: Option<fuser::BsdFileFlags>,
        reply: ReplyAttr,
    ) {
        let result = self.path_of(ino).and_then(|path| {
            if let Some(len) = size {
                self.core.truncate(&path, len).map_err(errno)?;
            }
            if let Some(time) = mtime {
                let secs = match time {
                    TimeOrNow::SpecificTime(t) => to_secs(t),
                    TimeOrNow::Now => to_secs(SystemTime::now()),
                };
                self.core.set_mtime(&path, secs).map_err(errno)?;
            }
            if mode.is_some() || uid.is_some() || gid.is_some() {
                // iCloud cannot store these; accept them so `cp -a` and `rsync -a` work.
                self.core.accept_permission_change(&path).map_err(errno)?;
            }
            self.attr_of(&path)
        });
        match result {
            Ok(attr) => reply.attr(&TTL, &attr),
            Err(err) => reply.error(err),
        }
    }

    fn mknod(
        &self,
        _req: &Request,
        parent: INodeNo,
        name: &OsStr,
        mode: u32,
        _umask: u32,
        _rdev: u32,
        reply: ReplyEntry,
    ) {
        // Only regular files exist on iCloud Drive: no devices, pipes or sockets.
        if mode & libc_s_ifmt() != libc_s_ifreg() {
            return reply.error(Errno::EPERM);
        }
        let result = self.child_path(parent, name).and_then(|path| {
            self.core.create(&path, mode).map_err(errno)?;
            self.attr_of(&path)
        });
        match result {
            Ok(attr) => reply.entry(&TTL, &attr, Generation(0)),
            Err(err) => reply.error(err),
        }
    }

    fn mkdir(&self, _req: &Request, parent: INodeNo, name: &OsStr, mode: u32, _umask: u32, reply: ReplyEntry) {
        let result = self.child_path(parent, name).and_then(|path| {
            self.core.mkdir(&path, mode).map_err(errno)?;
            self.attr_of(&path)
        });
        match result {
            Ok(attr) => reply.entry(&TTL, &attr, Generation(0)),
            Err(err) => reply.error(err),
        }
    }

    fn unlink(&self, _req: &Request, parent: INodeNo, name: &OsStr, reply: ReplyEmpty) {
        let result = self.child_path(parent, name).and_then(|path| {
            self.core.unlink(&path).map_err(errno)?;
            lock(&self.inodes).remove_subtree(&path);
            Ok(())
        });
        match result {
            Ok(()) => reply.ok(),
            Err(err) => reply.error(err),
        }
    }

    fn rmdir(&self, _req: &Request, parent: INodeNo, name: &OsStr, reply: ReplyEmpty) {
        let result = self.child_path(parent, name).and_then(|path| {
            self.core.rmdir(&path).map_err(errno)?;
            lock(&self.inodes).remove_subtree(&path);
            Ok(())
        });
        match result {
            Ok(()) => reply.ok(),
            Err(err) => reply.error(err),
        }
    }

    fn rename(
        &self,
        _req: &Request,
        parent: INodeNo,
        name: &OsStr,
        newparent: INodeNo,
        newname: &OsStr,
        flags: RenameFlags,
        reply: ReplyEmpty,
    ) {
        // RENAME_EXCHANGE swaps two paths atomically; iCloud has no such thing.
        if flags.contains(RenameFlags::RENAME_EXCHANGE) {
            return reply.error(Errno::EINVAL);
        }
        let result = self.child_path(parent, name).and_then(|from| {
            let to = self.child_path(newparent, newname)?;
            if flags.contains(RenameFlags::RENAME_NOREPLACE) && self.core.getattr(&to).is_ok() {
                return Err(Errno::EEXIST);
            }
            self.core.rename(&from, &to).map_err(errno)?;
            lock(&self.inodes).rename(&from, &to);
            Ok(())
        });
        match result {
            Ok(()) => reply.ok(),
            Err(err) => reply.error(err),
        }
    }

    fn open(&self, req: &Request, ino: INodeNo, flags: OpenFlags, reply: ReplyOpen) {
        let for_write = flags.acc_mode() != OpenAccMode::O_RDONLY;
        let who = ProcessReader::new(req.pid());
        match self.path_of(ino).and_then(|path| self.core.open_as(&path, for_write, &who).map_err(errno)) {
            Ok(()) => reply.opened(FileHandle(0), FopenFlags::empty()),
            Err(err) => reply.error(err),
        }
    }

    fn read(
        &self,
        req: &Request,
        ino: INodeNo,
        _fh: FileHandle,
        offset: u64,
        size: u32,
        _flags: OpenFlags,
        _lock_owner: Option<fuser::LockOwner>,
        reply: ReplyData,
    ) {
        let who = ProcessReader::new(req.pid());
        let result =
            self.path_of(ino).and_then(|path| self.core.read_as(&path, offset, size as usize, &who).map_err(errno));
        match result {
            Ok(data) => reply.data(&data),
            Err(err) => reply.error(err),
        }
    }

    fn write(
        &self,
        _req: &Request,
        ino: INodeNo,
        _fh: FileHandle,
        offset: u64,
        data: &[u8],
        _write_flags: WriteFlags,
        _flags: OpenFlags,
        _lock_owner: Option<fuser::LockOwner>,
        reply: ReplyWrite,
    ) {
        let result = self.path_of(ino).and_then(|path| self.core.write(&path, offset, data).map_err(errno));
        match result {
            Ok(written) => reply.written(u32::try_from(written).unwrap_or(u32::MAX)),
            Err(err) => reply.error(err),
        }
    }

    fn flush(&self, _req: &Request, _ino: INodeNo, _fh: FileHandle, _lock_owner: fuser::LockOwner, reply: ReplyEmpty) {
        // Data is already in the mirror; iCloud gets it on the next upload pass.
        reply.ok();
    }

    fn fsync(&self, _req: &Request, _ino: INodeNo, _fh: FileHandle, _datasync: bool, reply: ReplyEmpty) {
        reply.ok();
    }

    fn create(
        &self,
        _req: &Request,
        parent: INodeNo,
        name: &OsStr,
        mode: u32,
        _umask: u32,
        _flags: i32,
        reply: ReplyCreate,
    ) {
        let result = self.child_path(parent, name).and_then(|path| {
            self.core.create(&path, mode).map_err(errno)?;
            self.attr_of(&path)
        });
        match result {
            Ok(attr) => reply.created(&TTL, &attr, Generation(0), FileHandle(0), FopenFlags::empty()),
            Err(err) => reply.error(err),
        }
    }

    fn opendir(&self, _req: &Request, ino: INodeNo, _flags: OpenFlags, reply: ReplyOpen) {
        match self.path_of(ino).and_then(|path| self.core.readdir(&path).map_err(errno)) {
            Ok(items) => {
                let handle = self.next_listing.fetch_add(1, Ordering::Relaxed);
                lock(&self.listings).insert(handle, Arc::new(items));
                reply.opened(FileHandle(handle), FopenFlags::empty());
            }
            Err(err) => reply.error(err),
        }
    }

    fn readdir(&self, _req: &Request, ino: INodeNo, fh: FileHandle, offset: u64, mut reply: ReplyDirectory) {
        let Some(items) = lock(&self.listings).get(&fh.0).cloned() else {
            return reply.error(Errno::EBADF);
        };
        let Ok(dir) = self.path_of(ino) else { return reply.error(Errno::ENOENT) };
        let parent_ino = lock(&self.inodes).ino_for(&dir.parent());

        // Entry n of the stream is `.`, `..`, then the listing in order.
        let mut index = offset;
        let skip = usize::try_from(offset).unwrap_or(usize::MAX);
        let dots = [(ino.0, FileType::Directory, "."), (parent_ino, FileType::Directory, "..")];
        for (ino, kind, name) in dots.into_iter().skip(skip) {
            index += 1;
            if reply.add(INodeNo(ino), index, kind, name) {
                return reply.ok();
            }
        }
        for item in items.iter().skip(skip.saturating_sub(dots.len())) {
            let Some(child) = dir.join(&item.name) else { continue };
            index += 1;
            let child_ino = lock(&self.inodes).ino_for(&child);
            if reply.add(INodeNo(child_ino), index, file_type(item.kind), &item.name) {
                return reply.ok();
            }
        }
        reply.ok();
    }

    fn releasedir(&self, _req: &Request, _ino: INodeNo, fh: FileHandle, _flags: OpenFlags, reply: ReplyEmpty) {
        lock(&self.listings).remove(&fh.0);
        reply.ok();
    }

    fn statfs(&self, _req: &Request, _ino: INodeNo, reply: ReplyStatfs) {
        match self.core.statfs() {
            Ok(c) => reply.statfs(
                c.blocks,
                c.blocks_free,
                c.blocks_available,
                c.files,
                c.files_free,
                c.block_size,
                c.name_max,
                c.fragment_size,
            ),
            Err(err) => reply.error(errno(err)),
        }
    }

    // Extended attributes do not exist on iCloud Drive. The kernel and file
    // managers ask constantly (`system.posix_acl_access`, SELinux labels…), and
    // answering "none" quietly keeps the log readable; the default handlers log
    // a warning for every single request.
    fn getxattr(&self, _req: &Request, _ino: INodeNo, _name: &OsStr, _size: u32, reply: ReplyXattr) {
        reply.error(Errno::ENODATA);
    }

    fn listxattr(&self, _req: &Request, _ino: INodeNo, size: u32, reply: ReplyXattr) {
        if size == 0 {
            reply.size(0);
        } else {
            reply.data(&[]);
        }
    }

    fn setxattr(
        &self,
        _req: &Request,
        _ino: INodeNo,
        _name: &OsStr,
        _value: &[u8],
        _flags: i32,
        _position: u32,
        reply: ReplyEmpty,
    ) {
        reply.error(Errno::ENOTSUP);
    }

    fn removexattr(&self, _req: &Request, _ino: INodeNo, _name: &OsStr, reply: ReplyEmpty) {
        reply.error(Errno::ENODATA);
    }

    fn access(&self, _req: &Request, ino: INodeNo, _mask: AccessFlags, reply: ReplyEmpty) {
        // Permissions are those of the local user; there is nothing to refuse
        // beyond the path existing.
        match self.path_of(ino).and_then(|path| self.core.getattr(&path).map(drop).map_err(errno)) {
            Ok(()) => reply.ok(),
            Err(err) => reply.error(err),
        }
    }
}

/// `S_IFMT` and `S_IFREG`, spelled out to avoid a `libc` dependency for two constants.
const fn libc_s_ifmt() -> u32 {
    0o170_000
}
const fn libc_s_ifreg() -> u32 {
    0o100_000
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn errno_values_survive_the_translation() {
        assert_eq!(errno(icloud_core::Errno::NOENT), Errno::ENOENT);
        assert_eq!(errno(icloud_core::Errno::ACCESS), Errno::EACCES);
        assert_eq!(errno(icloud_core::Errno::NOTEMPTY).code(), 39);
        assert_eq!(errno(icloud_core::Errno::ROFS), Errno::EROFS);
    }

    #[test]
    fn times_convert_both_ways_and_clamp_negatives() {
        assert_eq!(to_secs(to_time(1_700_000_000)), 1_700_000_000);
        assert_eq!(to_time(-5), UNIX_EPOCH);
        assert_eq!(to_secs(UNIX_EPOCH - Duration::from_secs(10)), 0);
    }

    #[test]
    fn attributes_report_blocks_in_512_byte_units_rounded_up() {
        let attr = Attr {
            kind: FileKind::File,
            size: 1025,
            atime: 1,
            mtime: 2,
            ctime: 3,
            perm: 0o644,
            nlink: 1,
            uid: 1000,
            gid: 1000,
        };
        let out = file_attr(7, &attr);
        assert_eq!((out.ino.0, out.blocks, out.kind, out.perm), (7, 3, FileType::RegularFile, 0o644));
        assert_eq!(file_attr(1, &Attr { kind: FileKind::Directory, ..attr }).kind, FileType::Directory);
    }

    #[test]
    fn only_regular_files_can_be_made_with_mknod() {
        assert_eq!(0o100_644 & libc_s_ifmt(), libc_s_ifreg());
        assert_ne!(0o060_644 & libc_s_ifmt(), libc_s_ifreg(), "block device");
        assert_ne!(0o010_644 & libc_s_ifmt(), libc_s_ifreg(), "fifo");
    }
}
