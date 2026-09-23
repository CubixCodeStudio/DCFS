//! WebDAV, so the filesystem can be mounted without FUSE.
//!
//! FUSE is Linux-only and needs a kernel module; WebDAV is a protocol every
//! desktop already speaks. This is a second door onto the same rooms — every
//! read and write goes through the same functions the byte API uses, so the
//! two cannot drift on holes, segment boundaries, locking, or what an open
//! write is allowed to show.
//!
//! It inherits the API's authentication exactly: one shared token, one
//! namespace. WebDAV makes a filesystem easy to hand to a group of people,
//! which is precisely what this is not ready for.

use crate::handlers::data;
use crate::state::AppState;
use dav_server::davpath::DavPath;
use dav_server::fs::{
    DavDirEntry, DavFile, DavFileSystem, DavMetaData, FsError, FsFuture, FsResult, FsStream,
    OpenOptions, ReadDirMeta,
};
use dcfs_db::{NodeRecord, RepositoryError};
use std::io::SeekFrom;
use std::time::SystemTime;
use uuid::Uuid;

#[derive(Clone)]
pub struct DcfsFs {
    state: AppState,
}

impl DcfsFs {
    pub fn new(state: AppState) -> Box<Self> {
        Box::new(Self { state })
    }
}

fn to_fs_error(e: RepositoryError) -> FsError {
    match e {
        RepositoryError::NotFound => FsError::NotFound,
        RepositoryError::AlreadyExists => FsError::Exists,
        RepositoryError::DirectoryNotEmpty => FsError::Forbidden,
        _ => FsError::GeneralFailure,
    }
}

/// Walk a path from the root, one name at a time.
///
/// Paths arrive as raw bytes and node names are stored as raw bytes, so
/// nothing here goes through a string: a name that is not valid UTF-8 is a
/// name like any other.
async fn resolve(state: &AppState, path: &DavPath) -> FsResult<NodeRecord> {
    let mut node = state.repo.get_root().await.map_err(to_fs_error)?;
    for segment in path.as_bytes().split(|b| *b == b'/') {
        if segment.is_empty() {
            continue;
        }
        node = state
            .repo
            .find_child(node.id, segment)
            .await
            .map_err(to_fs_error)?;
    }
    Ok(node)
}

/// The directory a path names something in, and the name itself.
async fn resolve_parent(state: &AppState, path: &DavPath) -> FsResult<(NodeRecord, Vec<u8>)> {
    let name = path.file_name_bytes().to_vec();
    if name.is_empty() {
        // The root has no parent to create anything in.
        return Err(FsError::Forbidden);
    }
    let parent = resolve(state, &path.parent()).await?;
    Ok((parent, name))
}

#[derive(Debug, Clone)]
struct Meta {
    len: u64,
    is_dir: bool,
    modified: SystemTime,
}

impl From<&NodeRecord> for Meta {
    fn from(node: &NodeRecord) -> Self {
        Self {
            len: node.size.max(0) as u64,
            is_dir: node.kind == "directory",
            modified: SystemTime::UNIX_EPOCH
                + std::time::Duration::from_secs(node.mtime.timestamp().max(0) as u64),
        }
    }
}

impl DavMetaData for Meta {
    fn len(&self) -> u64 {
        self.len
    }
    fn modified(&self) -> FsResult<SystemTime> {
        Ok(self.modified)
    }
    fn is_dir(&self) -> bool {
        self.is_dir
    }
}

struct Entry {
    name: Vec<u8>,
    meta: Meta,
}

impl DavDirEntry for Entry {
    fn name(&self) -> Vec<u8> {
        self.name.clone()
    }
    fn metadata(&self) -> FsFuture<'_, Box<dyn DavMetaData>> {
        let meta = self.meta.clone();
        Box::pin(async move { Ok(Box::new(meta) as Box<dyn DavMetaData>) })
    }
    fn is_dir(&self) -> FsFuture<'_, bool> {
        let is_dir = self.meta.is_dir;
        Box::pin(async move { Ok(is_dir) })
    }
}

/// An open file. Position is this handle's own; the bytes are the server's.
struct File {
    state: AppState,
    node_id: Uuid,
    pos: u64,
}

// DavFile wants Debug, and the state behind this is a pile of connection
// pools nobody wants printed.
impl std::fmt::Debug for File {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("File")
            .field("node_id", &self.node_id)
            .field("pos", &self.pos)
            .finish()
    }
}

impl DavFile for File {
    fn metadata(&mut self) -> FsFuture<'_, Box<dyn DavMetaData>> {
        Box::pin(async move {
            let node = self
                .state
                .repo
                .get_node(self.node_id)
                .await
                .map_err(to_fs_error)?;
            Ok(Box::new(Meta::from(&node)) as Box<dyn DavMetaData>)
        })
    }

    fn write_bytes(&mut self, buf: bytes::Bytes) -> FsFuture<'_, ()> {
        Box::pin(async move {
            let written = data::write_bytes(&self.state, self.node_id, self.pos, buf)
                .await
                .map_err(|_| FsError::GeneralFailure)?;
            self.pos += written as u64;
            Ok(())
        })
    }

    fn write_buf(&mut self, mut buf: Box<dyn bytes::Buf + Send>) -> FsFuture<'_, ()> {
        Box::pin(async move {
            let bytes = buf.copy_to_bytes(buf.remaining());
            let written = data::write_bytes(&self.state, self.node_id, self.pos, bytes)
                .await
                .map_err(|_| FsError::GeneralFailure)?;
            self.pos += written as u64;
            Ok(())
        })
    }

    fn read_bytes(&mut self, count: usize) -> FsFuture<'_, bytes::Bytes> {
        Box::pin(async move {
            let (bytes, _) = data::read_bytes(
                &self.state,
                self.node_id,
                Some(self.pos),
                Some(count as u64),
            )
            .await
            .map_err(|_| FsError::GeneralFailure)?;
            self.pos += bytes.len() as u64;
            Ok(bytes)
        })
    }

    fn seek(&mut self, pos: SeekFrom) -> FsFuture<'_, u64> {
        Box::pin(async move {
            let end = || async {
                self.state
                    .repo
                    .get_node(self.node_id)
                    .await
                    .map(|n| n.size.max(0) as u64)
                    .map_err(to_fs_error)
            };
            self.pos = match pos {
                SeekFrom::Start(at) => at,
                SeekFrom::Current(by) => self.pos.saturating_add_signed(by),
                SeekFrom::End(by) => end().await?.saturating_add_signed(by),
            };
            Ok(self.pos)
        })
    }

    fn flush(&mut self) -> FsFuture<'_, ()> {
        // Closing the file is what commits the write, exactly as it is for a
        // client of the byte API.
        Box::pin(async move {
            data::commit_node(&self.state, self.node_id)
                .await
                .map_err(|_| FsError::GeneralFailure)?;
            Ok(())
        })
    }
}

impl DavFileSystem for DcfsFs {
    fn metadata<'a>(&'a self, path: &'a DavPath) -> FsFuture<'a, Box<dyn DavMetaData>> {
        Box::pin(async move {
            let node = resolve(&self.state, path).await?;
            Ok(Box::new(Meta::from(&node)) as Box<dyn DavMetaData>)
        })
    }

    fn read_dir<'a>(
        &'a self,
        path: &'a DavPath,
        _meta: ReadDirMeta,
    ) -> FsFuture<'a, FsStream<Box<dyn DavDirEntry>>> {
        Box::pin(async move {
            let dir = resolve(&self.state, path).await?;
            if dir.kind != "directory" {
                return Err(FsError::Forbidden);
            }
            // One page is all a listing gets. A directory with more children
            // than this reads as truncated rather than as an error, which is
            // the same ceiling the rest of the server has.
            let children = self
                .state
                .repo
                .list_children(dir.id, 10_000, 0)
                .await
                .map_err(to_fs_error)?;

            let entries: Vec<FsResult<Box<dyn DavDirEntry>>> = children
                .iter()
                .map(|child| {
                    Ok(Box::new(Entry {
                        name: child.name.clone(),
                        meta: Meta::from(child),
                    }) as Box<dyn DavDirEntry>)
                })
                .collect();
            Ok(Box::pin(futures_util::stream::iter(entries)) as FsStream<Box<dyn DavDirEntry>>)
        })
    }

    fn open<'a>(
        &'a self,
        path: &'a DavPath,
        options: OpenOptions,
    ) -> FsFuture<'a, Box<dyn DavFile>> {
        Box::pin(async move {
            let existing = resolve(&self.state, path).await;
            let node = match existing {
                Ok(node) if options.create_new => {
                    let _ = node;
                    return Err(FsError::Exists);
                }
                Ok(node) => {
                    if node.kind == "directory" {
                        return Err(FsError::Forbidden);
                    }
                    // A PUT replaces a file rather than editing it, and reads
                    // stop at the recorded size, so this is what makes the old
                    // contents stop being part of the file.
                    if options.truncate {
                        self.state
                            .repo
                            .update_node_attr(node.id, None, None, None, Some(0), None, None)
                            .await
                            .map_err(to_fs_error)?;
                    }
                    node
                }
                Err(FsError::NotFound) if options.create || options.create_new => {
                    let (parent, name) = resolve_parent(&self.state, path).await?;
                    self.state
                        .repo
                        .create_node(Uuid::new_v4(), parent.id, name, "file", 0o100644, 0, 0)
                        .await
                        .map_err(to_fs_error)?
                }
                Err(e) => return Err(e),
            };

            Ok(Box::new(File {
                state: self.state.clone(),
                node_id: node.id,
                pos: 0,
            }) as Box<dyn DavFile>)
        })
    }

    fn create_dir<'a>(&'a self, path: &'a DavPath) -> FsFuture<'a, ()> {
        Box::pin(async move {
            if resolve(&self.state, path).await.is_ok() {
                return Err(FsError::Exists);
            }
            let (parent, name) = resolve_parent(&self.state, path).await?;
            self.state
                .repo
                .create_node(Uuid::new_v4(), parent.id, name, "directory", 0o40755, 0, 0)
                .await
                .map_err(to_fs_error)?;
            Ok(())
        })
    }

    fn remove_file<'a>(&'a self, path: &'a DavPath) -> FsFuture<'a, ()> {
        Box::pin(async move {
            let node = resolve(&self.state, path).await?;
            if node.kind == "directory" {
                return Err(FsError::Forbidden);
            }
            self.state
                .repo
                .delete_node(node.id)
                .await
                .map_err(to_fs_error)
        })
    }

    fn remove_dir<'a>(&'a self, path: &'a DavPath) -> FsFuture<'a, ()> {
        Box::pin(async move {
            let node = resolve(&self.state, path).await?;
            if node.kind != "directory" {
                return Err(FsError::Forbidden);
            }
            self.state
                .repo
                .delete_node(node.id)
                .await
                .map_err(to_fs_error)
        })
    }

    fn rename<'a>(&'a self, from: &'a DavPath, to: &'a DavPath) -> FsFuture<'a, ()> {
        Box::pin(async move {
            let node = resolve(&self.state, from).await?;
            let (parent, name) = resolve_parent(&self.state, to).await?;
            self.state
                .repo
                .rename_node(node.id, parent.id, name)
                .await
                .map_err(to_fs_error)
        })
    }
}
