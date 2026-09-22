//! Thin adapter from `fuser`'s callbacks to [`crate::service::Fs`].
//!
//! Everything here is translation: inode numbers and raw name bytes in, kernel
//! replies out. The filesystem logic lives in the service so it can be tested
//! without a kernel.

use crate::client::ServerClient;
use crate::errno::to_errno;
use crate::handle::HandleTable;
use crate::service::{Attr, Fs};
use dcfs_core::NodeKind;
use fuser::{
    FileAttr, FileType, Filesystem, ReplyAttr, ReplyCreate, ReplyData, ReplyDirectory, ReplyEmpty,
    ReplyEntry, ReplyOpen, ReplyWrite, Request, TimeOrNow,
};
use std::ffi::OsStr;
use std::os::unix::ffi::OsStrExt;
use std::sync::Arc;
use std::time::{Duration, SystemTime};
use tokio::runtime::Handle;

/// How long the kernel may cache what we tell it.
///
/// The kernel treats an attribute reply as authoritative and shrinks its own
/// idea of a file to match, then trusts that until this expires. Writes are
/// buffered on this side, so a reply that lagged them made a freshly copied
/// file read back truncated, with no error anywhere. The service guards
/// against that by never reporting a size below the writes it has accepted;
/// without that guard this has to be zero.
///
/// It is short because another client can commit a new version at any time,
/// and `fuser` uses one value for both the name lookup and the attributes it
/// carries.
const TTL: Duration = Duration::from_secs(1);

pub struct DiscordFs<C: ServerClient> {
    fs: Arc<Fs<C>>,
    handles: Arc<HandleTable>,
    rt: Handle,
}

impl<C: ServerClient> DiscordFs<C> {
    pub fn new(fs: Arc<Fs<C>>, rt: Handle) -> Self {
        Self {
            fs,
            handles: Arc::new(HandleTable::new()),
            rt,
        }
    }
}

fn to_file_attr(attr: &Attr) -> FileAttr {
    FileAttr {
        ino: attr.ino,
        size: attr.size,
        blocks: attr.size.div_ceil(512),
        atime: to_system_time(&attr.atime),
        mtime: to_system_time(&attr.mtime),
        ctime: to_system_time(&attr.ctime),
        crtime: to_system_time(&attr.ctime),
        kind: match attr.kind {
            NodeKind::File => FileType::RegularFile,
            NodeKind::Directory => FileType::Directory,
            NodeKind::Symlink => FileType::Symlink,
        },
        // The stored mode carries the format bits too; the kernel wants only
        // the permission bits here.
        perm: (attr.mode & 0o7777) as u16,
        nlink: if attr.kind == NodeKind::Directory {
            2
        } else {
            1
        },
        uid: attr.uid,
        gid: attr.gid,
        rdev: 0,
        blksize: 4096,
        flags: 0,
    }
}

fn to_system_time(dt: &chrono::DateTime<chrono::Utc>) -> SystemTime {
    let secs = dt.timestamp();
    if secs < 0 {
        return std::time::UNIX_EPOCH;
    }
    std::time::UNIX_EPOCH + Duration::new(secs as u64, dt.timestamp_subsec_nanos())
}

fn from_time_or_now(t: TimeOrNow) -> chrono::DateTime<chrono::Utc> {
    match t {
        TimeOrNow::Now => chrono::Utc::now(),
        TimeOrNow::SpecificTime(time) => chrono::DateTime::from(time),
    }
}

impl<C: ServerClient> Filesystem for DiscordFs<C> {
    fn lookup(&mut self, _req: &Request, parent: u64, name: &OsStr, reply: ReplyEntry) {
        let fs = self.fs.clone();
        let name = name.as_bytes().to_vec();
        self.rt.spawn(async move {
            match fs.lookup(parent, &name).await {
                Ok(attr) => reply.entry(&TTL, &to_file_attr(&attr), 0),
                Err(e) => reply.error(to_errno(&e)),
            }
        });
    }

    fn getattr(&mut self, _req: &Request, ino: u64, reply: ReplyAttr) {
        let fs = self.fs.clone();
        self.rt.spawn(async move {
            match fs.getattr(ino).await {
                Ok(attr) => reply.attr(&TTL, &to_file_attr(&attr)),
                Err(e) => reply.error(to_errno(&e)),
            }
        });
    }

    #[allow(clippy::too_many_arguments)]
    fn setattr(
        &mut self,
        _req: &Request,
        ino: u64,
        mode: Option<u32>,
        uid: Option<u32>,
        gid: Option<u32>,
        size: Option<u64>,
        atime: Option<TimeOrNow>,
        mtime: Option<TimeOrNow>,
        _ctime: Option<SystemTime>,
        _fh: Option<u64>,
        _crtime: Option<SystemTime>,
        _chgtime: Option<SystemTime>,
        _bkuptime: Option<SystemTime>,
        _flags: Option<u32>,
        reply: ReplyAttr,
    ) {
        let fs = self.fs.clone();
        let atime = atime.map(from_time_or_now);
        let mtime = mtime.map(from_time_or_now);
        self.rt.spawn(async move {
            match fs.setattr(ino, mode, uid, gid, size, mtime, atime).await {
                Ok(attr) => reply.attr(&TTL, &to_file_attr(&attr)),
                Err(e) => reply.error(to_errno(&e)),
            }
        });
    }

    fn readdir(
        &mut self,
        _req: &Request,
        ino: u64,
        _fh: u64,
        offset: i64,
        mut reply: ReplyDirectory,
    ) {
        let fs = self.fs.clone();
        self.rt.spawn(async move {
            match fs.readdir(ino).await {
                Ok(entries) => {
                    for (index, entry) in entries.into_iter().enumerate().skip(offset as usize) {
                        let kind = match entry.kind {
                            NodeKind::File => FileType::RegularFile,
                            NodeKind::Directory => FileType::Directory,
                            NodeKind::Symlink => FileType::Symlink,
                        };
                        // Raw bytes straight through: filenames are not UTF-8.
                        let name = OsStr::from_bytes(&entry.name);
                        if reply.add(entry.ino, index as i64 + 1, kind, name) {
                            break;
                        }
                    }
                    reply.ok();
                }
                Err(e) => reply.error(to_errno(&e)),
            }
        });
    }

    fn create(
        &mut self,
        req: &Request,
        parent: u64,
        name: &OsStr,
        mode: u32,
        _umask: u32,
        _flags: i32,
        reply: ReplyCreate,
    ) {
        let fs = self.fs.clone();
        let handles = self.handles.clone();
        let name = name.as_bytes().to_vec();
        let (uid, gid) = (req.uid(), req.gid());
        self.rt.spawn(async move {
            match fs
                .create(parent, &name, NodeKind::File, mode, uid, gid)
                .await
            {
                Ok(attr) => {
                    let fh = handles.open(attr.ino, true);
                    // The last argument is FUSE's FOPEN_* set, not the caller's
                    // O_* flags: passing the open flags through here turns on
                    // FOPEN_DIRECT_IO and friends by accident.
                    reply.created(&TTL, &to_file_attr(&attr), 0, fh, 0);
                }
                Err(e) => reply.error(to_errno(&e)),
            }
        });
    }

    fn mkdir(
        &mut self,
        req: &Request,
        parent: u64,
        name: &OsStr,
        mode: u32,
        _umask: u32,
        reply: ReplyEntry,
    ) {
        let fs = self.fs.clone();
        let name = name.as_bytes().to_vec();
        let (uid, gid) = (req.uid(), req.gid());
        self.rt.spawn(async move {
            match fs
                .create(parent, &name, NodeKind::Directory, mode, uid, gid)
                .await
            {
                Ok(attr) => reply.entry(&TTL, &to_file_attr(&attr), 0),
                Err(e) => reply.error(to_errno(&e)),
            }
        });
    }

    fn symlink(
        &mut self,
        req: &Request,
        parent: u64,
        link_name: &OsStr,
        target: &std::path::Path,
        reply: ReplyEntry,
    ) {
        let fs = self.fs.clone();
        let name = link_name.as_bytes().to_vec();
        // Targets are raw bytes, not text: a link may point anywhere, valid
        // UTF-8 or not, existing or not.
        let target = target.as_os_str().as_bytes().to_vec();
        let (uid, gid) = (req.uid(), req.gid());
        self.rt.spawn(async move {
            match fs.symlink(parent, &name, &target, uid, gid).await {
                Ok(attr) => reply.entry(&TTL, &to_file_attr(&attr), 0),
                Err(e) => reply.error(to_errno(&e)),
            }
        });
    }

    fn readlink(&mut self, _req: &Request, ino: u64, reply: ReplyData) {
        let fs = self.fs.clone();
        self.rt.spawn(async move {
            match fs.readlink(ino).await {
                Ok(target) => reply.data(&target),
                Err(e) => reply.error(to_errno(&e)),
            }
        });
    }

    fn statfs(&mut self, _req: &Request, _ino: u64, reply: fuser::ReplyStatfs) {
        let stats = self.fs.statfs();
        reply.statfs(
            stats.total_blocks,
            stats.free_blocks,
            stats.free_blocks,
            0, // files: unknown, and nothing needs the count
            0, // free files
            stats.block_size,
            stats.name_max,
            stats.block_size,
        );
    }

    fn open(&mut self, _req: &Request, ino: u64, flags: i32, reply: ReplyOpen) {
        let writable = flags & (libc::O_WRONLY | libc::O_RDWR) != 0;
        reply.opened(self.handles.open(ino, writable), 0);
    }

    fn read(
        &mut self,
        _req: &Request,
        ino: u64,
        _fh: u64,
        offset: i64,
        size: u32,
        _flags: i32,
        _lock_owner: Option<u64>,
        reply: ReplyData,
    ) {
        let fs = self.fs.clone();
        self.rt.spawn(async move {
            match fs.read(ino, offset.max(0) as u64, size as u64).await {
                Ok(data) => reply.data(&data),
                Err(e) => reply.error(to_errno(&e)),
            }
        });
    }

    #[allow(clippy::too_many_arguments)]
    fn write(
        &mut self,
        _req: &Request,
        ino: u64,
        _fh: u64,
        offset: i64,
        data: &[u8],
        _write_flags: u32,
        _flags: i32,
        _lock_owner: Option<u64>,
        reply: ReplyWrite,
    ) {
        let fs = self.fs.clone();
        let data = data.to_vec();
        self.rt.spawn(async move {
            match fs.write(ino, offset.max(0) as u64, &data).await {
                Ok(written) => reply.written(written as u32),
                Err(e) => reply.error(to_errno(&e)),
            }
        });
    }

    fn fsync(&mut self, _req: &Request, ino: u64, _fh: u64, _datasync: bool, reply: ReplyEmpty) {
        let fs = self.fs.clone();
        self.rt.spawn(async move {
            match fs.fsync(ino).await {
                Ok(()) => reply.ok(),
                Err(e) => reply.error(to_errno(&e)),
            }
        });
    }

    fn flush(&mut self, _req: &Request, ino: u64, _fh: u64, _lock_owner: u64, reply: ReplyEmpty) {
        let fs = self.fs.clone();
        self.rt.spawn(async move {
            match fs.fsync(ino).await {
                Ok(()) => reply.ok(),
                Err(e) => reply.error(to_errno(&e)),
            }
        });
    }

    fn release(
        &mut self,
        _req: &Request,
        _ino: u64,
        fh: u64,
        _flags: i32,
        _lock_owner: Option<u64>,
        _flush: bool,
        reply: ReplyEmpty,
    ) {
        self.handles.close(fh);
        reply.ok();
    }

    fn unlink(&mut self, _req: &Request, parent: u64, name: &OsStr, reply: ReplyEmpty) {
        let fs = self.fs.clone();
        let name = name.as_bytes().to_vec();
        self.rt.spawn(async move {
            match fs.remove(parent, &name).await {
                Ok(()) => reply.ok(),
                Err(e) => reply.error(to_errno(&e)),
            }
        });
    }

    fn rmdir(&mut self, req: &Request, parent: u64, name: &OsStr, reply: ReplyEmpty) {
        // The server refuses to remove a directory that still has children.
        self.unlink(req, parent, name, reply);
    }

    fn rename(
        &mut self,
        _req: &Request,
        parent: u64,
        name: &OsStr,
        newparent: u64,
        newname: &OsStr,
        _flags: u32,
        reply: ReplyEmpty,
    ) {
        let fs = self.fs.clone();
        let name = name.as_bytes().to_vec();
        let newname = newname.as_bytes().to_vec();
        self.rt.spawn(async move {
            match fs.rename(parent, &name, newparent, &newname).await {
                Ok(()) => reply.ok(),
                Err(e) => reply.error(to_errno(&e)),
            }
        });
    }
}
