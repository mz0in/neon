//!
//! VirtualFile is like a normal File, but it's not bound directly to
//! a file descriptor. Instead, the file is opened when it's read from,
//! and if too many files are open globally in the system, least-recently
//! used ones are closed.
//!
//! To track which files have been recently used, we use the clock algorithm
//! with a 'recently_used' flag on each slot.
//!
//! This is similar to PostgreSQL's virtual file descriptor facility in
//! src/backend/storage/file/fd.c
//!
use crate::metrics::{StorageIoOperation, STORAGE_IO_SIZE, STORAGE_IO_TIME_METRIC};
use crate::tenant::TENANTS_SEGMENT_NAME;
use camino::{Utf8Path, Utf8PathBuf};
use once_cell::sync::OnceCell;
use std::fs::{self, File, OpenOptions};
use std::io::{Error, ErrorKind, Seek, SeekFrom};
use std::os::unix::fs::FileExt;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use tokio::sync::{RwLock, RwLockReadGuard, RwLockWriteGuard};
use tokio::time::Instant;
use utils::fs_ext;

///
/// A virtual file descriptor. You can use this just like std::fs::File, but internally
/// the underlying file is closed if the system is low on file descriptors,
/// and re-opened when it's accessed again.
///
/// Like with std::fs::File, multiple threads can read/write the file concurrently,
/// holding just a shared reference the same VirtualFile, using the read_at() / write_at()
/// functions from the FileExt trait. But the functions from the Read/Write/Seek traits
/// require a mutable reference, because they modify the "current position".
///
/// Each VirtualFile has a physical file descriptor in the global OPEN_FILES array, at the
/// slot that 'handle points to, if the underlying file is currently open. If it's not
/// currently open, the 'handle' can still point to the slot where it was last kept. The
/// 'tag' field is used to detect whether the handle still is valid or not.
///
#[derive(Debug)]
pub struct VirtualFile {
    /// Lazy handle to the global file descriptor cache. The slot that this points to
    /// might contain our File, or it may be empty, or it may contain a File that
    /// belongs to a different VirtualFile.
    handle: RwLock<SlotHandle>,

    /// Current file position
    pos: u64,

    /// File path and options to use to open it.
    ///
    /// Note: this only contains the options needed to re-open it. For example,
    /// if a new file is created, we only pass the create flag when it's initially
    /// opened, in the VirtualFile::create() function, and strip the flag before
    /// storing it here.
    pub path: Utf8PathBuf,
    open_options: OpenOptions,

    // These are strings becase we only use them for metrics, and those expect strings.
    // It makes no sense for us to constantly turn the `TimelineId` and `TenantId` into
    // strings.
    tenant_id: String,
    timeline_id: String,
}

#[derive(Debug, PartialEq, Clone, Copy)]
struct SlotHandle {
    /// Index into OPEN_FILES.slots
    index: usize,

    /// Value of 'tag' in the slot. If slot's tag doesn't match, then the slot has
    /// been recycled and no longer contains the FD for this virtual file.
    tag: u64,
}

/// OPEN_FILES is the global array that holds the physical file descriptors that
/// are currently open. Each slot in the array is protected by a separate lock,
/// so that different files can be accessed independently. The lock must be held
/// in write mode to replace the slot with a different file, but a read mode
/// is enough to operate on the file, whether you're reading or writing to it.
///
/// OPEN_FILES starts in uninitialized state, and it's initialized by
/// the virtual_file::init() function. It must be called exactly once at page
/// server startup.
static OPEN_FILES: OnceCell<OpenFiles> = OnceCell::new();

struct OpenFiles {
    slots: &'static [Slot],

    /// clock arm for the clock algorithm
    next: AtomicUsize,
}

struct Slot {
    inner: RwLock<SlotInner>,

    /// has this file been used since last clock sweep?
    recently_used: AtomicBool,
}

struct SlotInner {
    /// Counter that's incremented every time a different file is stored here.
    /// To avoid the ABA problem.
    tag: u64,

    /// the underlying file
    file: Option<File>,
}

impl OpenFiles {
    /// Find a slot to use, evicting an existing file descriptor if needed.
    ///
    /// On return, we hold a lock on the slot, and its 'tag' has been updated
    /// recently_used has been set. It's all ready for reuse.
    async fn find_victim_slot(&self) -> (SlotHandle, RwLockWriteGuard<SlotInner>) {
        //
        // Run the clock algorithm to find a slot to replace.
        //
        let num_slots = self.slots.len();
        let mut retries = 0;
        let mut slot;
        let mut slot_guard;
        let index;
        loop {
            let next = self.next.fetch_add(1, Ordering::AcqRel) % num_slots;
            slot = &self.slots[next];

            // If the recently_used flag on this slot is set, continue the clock
            // sweep. Otherwise try to use this slot. If we cannot acquire the
            // lock, also continue the clock sweep.
            //
            // We only continue in this manner for a while, though. If we loop
            // through the array twice without finding a victim, just pick the
            // next slot and wait until we can reuse it. This way, we avoid
            // spinning in the extreme case that all the slots are busy with an
            // I/O operation.
            if retries < num_slots * 2 {
                if !slot.recently_used.swap(false, Ordering::Release) {
                    if let Ok(guard) = slot.inner.try_write() {
                        slot_guard = guard;
                        index = next;
                        break;
                    }
                }
                retries += 1;
            } else {
                slot_guard = slot.inner.write().await;
                index = next;
                break;
            }
        }

        //
        // We now have the victim slot locked. If it was in use previously, close the
        // old file.
        //
        if let Some(old_file) = slot_guard.file.take() {
            // the normal path of dropping VirtualFile uses "close", use "close-by-replace" here to
            // distinguish the two.
            STORAGE_IO_TIME_METRIC
                .get(StorageIoOperation::CloseByReplace)
                .observe_closure_duration(|| drop(old_file));
        }

        // Prepare the slot for reuse and return it
        slot_guard.tag += 1;
        slot.recently_used.store(true, Ordering::Relaxed);
        (
            SlotHandle {
                index,
                tag: slot_guard.tag,
            },
            slot_guard,
        )
    }
}

/// Identify error types that should alwways terminate the process.  Other
/// error types may be elegible for retry.
pub(crate) fn is_fatal_io_error(e: &std::io::Error) -> bool {
    use nix::errno::Errno::*;
    match e.raw_os_error().map(nix::errno::from_i32) {
        Some(EIO) => {
            // Terminate on EIO because we no longer trust the device to store
            // data safely, or to uphold persistence guarantees on fsync.
            true
        }
        Some(EROFS) => {
            // Terminate on EROFS because a filesystem is usually remounted
            // readonly when it has experienced some critical issue, so the same
            // logic as EIO applies.
            true
        }
        Some(EACCES) => {
            // Terminate on EACCESS because we should always have permissions
            // for our own data dir: if we don't, then we can't do our job and
            // need administrative intervention to fix permissions.  Terminating
            // is the best way to make sure we stop cleanly rather than going
            // into infinite retry loops, and will make it clear to the outside
            // world that we need help.
            true
        }
        _ => {
            // Treat all other local file I/O errors are retryable.  This includes:
            // - ENOSPC: we stay up and wait for eviction to free some space
            // - EINVAL, EBADF, EBADFD: this is a code bug, not a filesystem/hardware issue
            // - WriteZero, Interrupted: these are used internally VirtualFile
            false
        }
    }
}

/// Call this when the local filesystem gives us an error with an external
/// cause: this includes EIO, EROFS, and EACCESS: all these indicate either
/// bad storage or bad configuration, and we can't fix that from inside
/// a running process.
pub(crate) fn on_fatal_io_error(e: &std::io::Error, context: &str) -> ! {
    tracing::error!("Fatal I/O error: {e}: {context})");
    std::process::abort();
}

pub(crate) trait MaybeFatalIo<T> {
    fn maybe_fatal_err(self, context: &str) -> std::io::Result<T>;
    fn fatal_err(self, context: &str) -> T;
}

impl<T> MaybeFatalIo<T> for std::io::Result<T> {
    /// Terminate the process if the result is an error of a fatal type, else pass it through
    ///
    /// This is appropriate for writes, where we typically want to die on EIO/ACCES etc, but
    /// not on ENOSPC.
    fn maybe_fatal_err(self, context: &str) -> std::io::Result<T> {
        if let Err(e) = &self {
            if is_fatal_io_error(e) {
                on_fatal_io_error(e, context);
            }
        }
        self
    }

    /// Terminate the process on any I/O error.
    ///
    /// This is appropriate for reads on files that we know exist: they should always work.
    fn fatal_err(self, context: &str) -> T {
        match self {
            Ok(v) => v,
            Err(e) => {
                on_fatal_io_error(&e, context);
            }
        }
    }
}

/// Observe duration for the given storage I/O operation
///
/// Unlike `observe_closure_duration`, this supports async,
/// where "support" means that we measure wall clock time.
macro_rules! observe_duration {
    ($op:expr, $($body:tt)*) => {{
        let instant = Instant::now();
        let result = $($body)*;
        let elapsed = instant.elapsed().as_secs_f64();
        STORAGE_IO_TIME_METRIC
            .get($op)
            .observe(elapsed);
        result
    }}
}

macro_rules! with_file {
    ($this:expr, $op:expr, | $ident:ident | $($body:tt)*) => {{
        let $ident = $this.lock_file().await?;
        observe_duration!($op, $($body)*)
    }};
}

impl VirtualFile {
    /// Open a file in read-only mode. Like File::open.
    pub async fn open(path: &Utf8Path) -> Result<VirtualFile, std::io::Error> {
        Self::open_with_options(path, OpenOptions::new().read(true)).await
    }

    /// Create a new file for writing. If the file exists, it will be truncated.
    /// Like File::create.
    pub async fn create(path: &Utf8Path) -> Result<VirtualFile, std::io::Error> {
        Self::open_with_options(
            path,
            OpenOptions::new().write(true).create(true).truncate(true),
        )
        .await
    }

    /// Open a file with given options.
    ///
    /// Note: If any custom flags were set in 'open_options' through OpenOptionsExt,
    /// they will be applied also when the file is subsequently re-opened, not only
    /// on the first time. Make sure that's sane!
    pub async fn open_with_options(
        path: &Utf8Path,
        open_options: &OpenOptions,
    ) -> Result<VirtualFile, std::io::Error> {
        let path_str = path.to_string();
        let parts = path_str.split('/').collect::<Vec<&str>>();
        let tenant_id;
        let timeline_id;
        if parts.len() > 5 && parts[parts.len() - 5] == TENANTS_SEGMENT_NAME {
            tenant_id = parts[parts.len() - 4].to_string();
            timeline_id = parts[parts.len() - 2].to_string();
        } else {
            tenant_id = "*".to_string();
            timeline_id = "*".to_string();
        }
        let (handle, mut slot_guard) = get_open_files().find_victim_slot().await;

        // NB: there is also StorageIoOperation::OpenAfterReplace which is for the case
        // where our caller doesn't get to use the returned VirtualFile before its
        // slot gets re-used by someone else.
        let file = observe_duration!(StorageIoOperation::Open, open_options.open(path))?;

        // Strip all options other than read and write.
        //
        // It would perhaps be nicer to check just for the read and write flags
        // explicitly, but OpenOptions doesn't contain any functions to read flags,
        // only to set them.
        let mut reopen_options = open_options.clone();
        reopen_options.create(false);
        reopen_options.create_new(false);
        reopen_options.truncate(false);

        let vfile = VirtualFile {
            handle: RwLock::new(handle),
            pos: 0,
            path: path.to_path_buf(),
            open_options: reopen_options,
            tenant_id,
            timeline_id,
        };

        // TODO: Under pressure, it's likely the slot will get re-used and
        // the underlying file closed before they get around to using it.
        // => https://github.com/neondatabase/neon/issues/6065
        slot_guard.file.replace(file);

        Ok(vfile)
    }

    /// Writes a file to the specified `final_path` in a crash safe fasion
    ///
    /// The file is first written to the specified tmp_path, and in a second
    /// step, the tmp path is renamed to the final path. As renames are
    /// atomic, a crash during the write operation will never leave behind a
    /// partially written file.
    pub async fn crashsafe_overwrite(
        final_path: &Utf8Path,
        tmp_path: &Utf8Path,
        content: &[u8],
    ) -> std::io::Result<()> {
        let Some(final_path_parent) = final_path.parent() else {
            return Err(std::io::Error::from_raw_os_error(
                nix::errno::Errno::EINVAL as i32,
            ));
        };
        std::fs::remove_file(tmp_path).or_else(fs_ext::ignore_not_found)?;
        let mut file = Self::open_with_options(
            tmp_path,
            OpenOptions::new()
                .write(true)
                // Use `create_new` so that, if we race with ourselves or something else,
                // we bail out instead of causing damage.
                .create_new(true),
        )
        .await?;
        file.write_all(content).await?;
        file.sync_all().await?;
        drop(file); // before the rename, that's important!
                    // renames are atomic
        std::fs::rename(tmp_path, final_path)?;
        // Only open final path parent dirfd now, so that this operation only
        // ever holds one VirtualFile fd at a time.  That's important because
        // the current `find_victim_slot` impl might pick the same slot for both
        // VirtualFile., and it eventually does a blocking write lock instead of
        // try_lock.
        let final_parent_dirfd =
            Self::open_with_options(final_path_parent, OpenOptions::new().read(true)).await?;
        final_parent_dirfd.sync_all().await?;
        Ok(())
    }

    /// Call File::sync_all() on the underlying File.
    pub async fn sync_all(&self) -> Result<(), Error> {
        with_file!(self, StorageIoOperation::Fsync, |file| file
            .as_ref()
            .sync_all())
    }

    pub async fn metadata(&self) -> Result<fs::Metadata, Error> {
        with_file!(self, StorageIoOperation::Metadata, |file| file
            .as_ref()
            .metadata())
    }

    /// Helper function internal to `VirtualFile` that looks up the underlying File,
    /// opens it and evicts some other File if necessary. The passed parameter is
    /// assumed to be a function available for the physical `File`.
    ///
    /// We are doing it via a macro as Rust doesn't support async closures that
    /// take on parameters with lifetimes.
    async fn lock_file(&self) -> Result<FileGuard<'_>, Error> {
        let open_files = get_open_files();

        let mut handle_guard = {
            // Read the cached slot handle, and see if the slot that it points to still
            // contains our File.
            //
            // We only need to hold the handle lock while we read the current handle. If
            // another thread closes the file and recycles the slot for a different file,
            // we will notice that the handle we read is no longer valid and retry.
            let mut handle = *self.handle.read().await;
            loop {
                // Check if the slot contains our File
                {
                    let slot = &open_files.slots[handle.index];
                    let slot_guard = slot.inner.read().await;
                    if slot_guard.tag == handle.tag && slot_guard.file.is_some() {
                        // Found a cached file descriptor.
                        slot.recently_used.store(true, Ordering::Relaxed);
                        return Ok(FileGuard { slot_guard });
                    }
                }

                // The slot didn't contain our File. We will have to open it ourselves,
                // but before that, grab a write lock on handle in the VirtualFile, so
                // that no other thread will try to concurrently open the same file.
                let handle_guard = self.handle.write().await;

                // If another thread changed the handle while we were not holding the lock,
                // then the handle might now be valid again. Loop back to retry.
                if *handle_guard != handle {
                    handle = *handle_guard;
                    continue;
                }
                break handle_guard;
            }
        };

        // We need to open the file ourselves. The handle in the VirtualFile is
        // now locked in write-mode. Find a free slot to put it in.
        let (handle, mut slot_guard) = open_files.find_victim_slot().await;

        // Re-open the physical file.
        // NB: we use StorageIoOperation::OpenAferReplace for this to distinguish this
        // case from StorageIoOperation::Open. This helps with identifying thrashing
        // of the virtual file descriptor cache.
        let file = observe_duration!(
            StorageIoOperation::OpenAfterReplace,
            self.open_options.open(&self.path)
        )?;

        // Store the File in the slot and update the handle in the VirtualFile
        // to point to it.
        slot_guard.file.replace(file);

        *handle_guard = handle;

        return Ok(FileGuard {
            slot_guard: slot_guard.downgrade(),
        });
    }

    pub fn remove(self) {
        let path = self.path.clone();
        drop(self);
        std::fs::remove_file(path).expect("failed to remove the virtual file");
    }

    pub async fn seek(&mut self, pos: SeekFrom) -> Result<u64, Error> {
        match pos {
            SeekFrom::Start(offset) => {
                self.pos = offset;
            }
            SeekFrom::End(offset) => {
                self.pos = with_file!(self, StorageIoOperation::Seek, |file| file
                    .as_ref()
                    .seek(SeekFrom::End(offset)))?
            }
            SeekFrom::Current(offset) => {
                let pos = self.pos as i128 + offset as i128;
                if pos < 0 {
                    return Err(Error::new(
                        ErrorKind::InvalidInput,
                        "offset would be negative",
                    ));
                }
                if pos > u64::MAX as i128 {
                    return Err(Error::new(ErrorKind::InvalidInput, "offset overflow"));
                }
                self.pos = pos as u64;
            }
        }
        Ok(self.pos)
    }

    // Copied from https://doc.rust-lang.org/1.72.0/src/std/os/unix/fs.rs.html#117-135
    pub async fn read_exact_at(&self, mut buf: &mut [u8], mut offset: u64) -> Result<(), Error> {
        while !buf.is_empty() {
            match self.read_at(buf, offset).await {
                Ok(0) => {
                    return Err(Error::new(
                        std::io::ErrorKind::UnexpectedEof,
                        "failed to fill whole buffer",
                    ))
                }
                Ok(n) => {
                    buf = &mut buf[n..];
                    offset += n as u64;
                }
                Err(ref e) if e.kind() == std::io::ErrorKind::Interrupted => {}
                Err(e) => return Err(e),
            }
        }
        Ok(())
    }

    // Copied from https://doc.rust-lang.org/1.72.0/src/std/os/unix/fs.rs.html#219-235
    pub async fn write_all_at(&self, mut buf: &[u8], mut offset: u64) -> Result<(), Error> {
        while !buf.is_empty() {
            match self.write_at(buf, offset).await {
                Ok(0) => {
                    return Err(Error::new(
                        std::io::ErrorKind::WriteZero,
                        "failed to write whole buffer",
                    ));
                }
                Ok(n) => {
                    buf = &buf[n..];
                    offset += n as u64;
                }
                Err(ref e) if e.kind() == std::io::ErrorKind::Interrupted => {}
                Err(e) => return Err(e),
            }
        }
        Ok(())
    }

    pub async fn write_all(&mut self, mut buf: &[u8]) -> Result<(), Error> {
        while !buf.is_empty() {
            match self.write(buf).await {
                Ok(0) => {
                    return Err(Error::new(
                        std::io::ErrorKind::WriteZero,
                        "failed to write whole buffer",
                    ));
                }
                Ok(n) => {
                    buf = &buf[n..];
                }
                Err(ref e) if e.kind() == std::io::ErrorKind::Interrupted => {}
                Err(e) => return Err(e),
            }
        }
        Ok(())
    }

    async fn write(&mut self, buf: &[u8]) -> Result<usize, std::io::Error> {
        let pos = self.pos;
        let n = self.write_at(buf, pos).await?;
        self.pos += n as u64;
        Ok(n)
    }

    pub async fn read_at(&self, buf: &mut [u8], offset: u64) -> Result<usize, Error> {
        let result = with_file!(self, StorageIoOperation::Read, |file| file
            .as_ref()
            .read_at(buf, offset));
        if let Ok(size) = result {
            STORAGE_IO_SIZE
                .with_label_values(&["read", &self.tenant_id, &self.timeline_id])
                .add(size as i64);
        }
        result
    }

    async fn write_at(&self, buf: &[u8], offset: u64) -> Result<usize, Error> {
        let result = with_file!(self, StorageIoOperation::Write, |file| file
            .as_ref()
            .write_at(buf, offset));
        if let Ok(size) = result {
            STORAGE_IO_SIZE
                .with_label_values(&["write", &self.tenant_id, &self.timeline_id])
                .add(size as i64);
        }
        result
    }
}

struct FileGuard<'a> {
    slot_guard: RwLockReadGuard<'a, SlotInner>,
}

impl<'a> AsRef<File> for FileGuard<'a> {
    fn as_ref(&self) -> &File {
        // This unwrap is safe because we only create `FileGuard`s
        // if we know that the file is Some.
        self.slot_guard.file.as_ref().unwrap()
    }
}

#[cfg(test)]
impl VirtualFile {
    pub(crate) async fn read_blk(
        &self,
        blknum: u32,
    ) -> Result<crate::tenant::block_io::BlockLease<'_>, std::io::Error> {
        use crate::page_cache::PAGE_SZ;
        let mut buf = [0; PAGE_SZ];
        self.read_exact_at(&mut buf, blknum as u64 * (PAGE_SZ as u64))
            .await?;
        Ok(std::sync::Arc::new(buf).into())
    }

    async fn read_to_end(&mut self, buf: &mut Vec<u8>) -> Result<(), Error> {
        loop {
            let mut tmp = [0; 128];
            match self.read_at(&mut tmp, self.pos).await {
                Ok(0) => return Ok(()),
                Ok(n) => {
                    self.pos += n as u64;
                    buf.extend_from_slice(&tmp[..n]);
                }
                Err(ref e) if e.kind() == std::io::ErrorKind::Interrupted => {}
                Err(e) => return Err(e),
            }
        }
    }
}

impl Drop for VirtualFile {
    /// If a VirtualFile is dropped, close the underlying file if it was open.
    fn drop(&mut self) {
        let handle = self.handle.get_mut();

        fn clean_slot(slot: &Slot, mut slot_guard: RwLockWriteGuard<'_, SlotInner>, tag: u64) {
            if slot_guard.tag == tag {
                slot.recently_used.store(false, Ordering::Relaxed);
                // there is also operation "close-by-replace" for closes done on eviction for
                // comparison.
                if let Some(fd) = slot_guard.file.take() {
                    STORAGE_IO_TIME_METRIC
                        .get(StorageIoOperation::Close)
                        .observe_closure_duration(|| drop(fd));
                }
            }
        }

        // We don't have async drop so we cannot directly await the lock here.
        // Instead, first do a best-effort attempt at closing the underlying
        // file descriptor by using `try_write`, and if that fails, spawn
        // a tokio task to do it asynchronously: we just want it to be
        // cleaned up eventually.
        // Most of the time, the `try_lock` should succeed though,
        // as we have `&mut self` access. In other words, if the slot
        // is still occupied by our file, there should be no access from
        // other I/O operations; the only other possible place to lock
        // the slot is the lock algorithm looking for free slots.
        let slot = &get_open_files().slots[handle.index];
        if let Ok(slot_guard) = slot.inner.try_write() {
            clean_slot(slot, slot_guard, handle.tag);
        } else {
            let tag = handle.tag;
            tokio::spawn(async move {
                let slot_guard = slot.inner.write().await;
                clean_slot(slot, slot_guard, tag);
            });
        };
    }
}

impl OpenFiles {
    fn new(num_slots: usize) -> OpenFiles {
        let mut slots = Box::new(Vec::with_capacity(num_slots));
        for _ in 0..num_slots {
            let slot = Slot {
                recently_used: AtomicBool::new(false),
                inner: RwLock::new(SlotInner { tag: 0, file: None }),
            };
            slots.push(slot);
        }

        OpenFiles {
            next: AtomicUsize::new(0),
            slots: Box::leak(slots),
        }
    }
}

///
/// Initialize the virtual file module. This must be called once at page
/// server startup.
///
pub fn init(num_slots: usize) {
    if OPEN_FILES.set(OpenFiles::new(num_slots)).is_err() {
        panic!("virtual_file::init called twice");
    }
    crate::metrics::virtual_file_descriptor_cache::SIZE_MAX.set(num_slots as u64);
}

const TEST_MAX_FILE_DESCRIPTORS: usize = 10;

// Get a handle to the global slots array.
fn get_open_files() -> &'static OpenFiles {
    //
    // In unit tests, page server startup doesn't happen and no one calls
    // virtual_file::init(). Initialize it here, with a small array.
    //
    // This applies to the virtual file tests below, but all other unit
    // tests too, so the virtual file facility is always usable in
    // unit tests.
    //
    if cfg!(test) {
        OPEN_FILES.get_or_init(|| OpenFiles::new(TEST_MAX_FILE_DESCRIPTORS))
    } else {
        OPEN_FILES.get().expect("virtual_file::init not called yet")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rand::seq::SliceRandom;
    use rand::thread_rng;
    use rand::Rng;
    use std::future::Future;
    use std::io::Write;
    use std::sync::Arc;

    enum MaybeVirtualFile {
        VirtualFile(VirtualFile),
        File(File),
    }

    impl From<VirtualFile> for MaybeVirtualFile {
        fn from(vf: VirtualFile) -> Self {
            MaybeVirtualFile::VirtualFile(vf)
        }
    }

    impl MaybeVirtualFile {
        async fn read_exact_at(&self, buf: &mut [u8], offset: u64) -> Result<(), Error> {
            match self {
                MaybeVirtualFile::VirtualFile(file) => file.read_exact_at(buf, offset).await,
                MaybeVirtualFile::File(file) => file.read_exact_at(buf, offset),
            }
        }
        async fn write_all_at(&self, buf: &[u8], offset: u64) -> Result<(), Error> {
            match self {
                MaybeVirtualFile::VirtualFile(file) => file.write_all_at(buf, offset).await,
                MaybeVirtualFile::File(file) => file.write_all_at(buf, offset),
            }
        }
        async fn seek(&mut self, pos: SeekFrom) -> Result<u64, Error> {
            match self {
                MaybeVirtualFile::VirtualFile(file) => file.seek(pos).await,
                MaybeVirtualFile::File(file) => file.seek(pos),
            }
        }
        async fn write_all(&mut self, buf: &[u8]) -> Result<(), Error> {
            match self {
                MaybeVirtualFile::VirtualFile(file) => file.write_all(buf).await,
                MaybeVirtualFile::File(file) => file.write_all(buf),
            }
        }

        // Helper function to slurp contents of a file, starting at the current position,
        // into a string
        async fn read_string(&mut self) -> Result<String, Error> {
            use std::io::Read;
            let mut buf = String::new();
            match self {
                MaybeVirtualFile::VirtualFile(file) => {
                    let mut buf = Vec::new();
                    file.read_to_end(&mut buf).await?;
                    return Ok(String::from_utf8(buf).unwrap());
                }
                MaybeVirtualFile::File(file) => {
                    file.read_to_string(&mut buf)?;
                }
            }
            Ok(buf)
        }

        // Helper function to slurp a portion of a file into a string
        async fn read_string_at(&mut self, pos: u64, len: usize) -> Result<String, Error> {
            let mut buf = vec![0; len];
            self.read_exact_at(&mut buf, pos).await?;
            Ok(String::from_utf8(buf).unwrap())
        }
    }

    #[tokio::test]
    async fn test_virtual_files() -> Result<(), Error> {
        // The real work is done in the test_files() helper function. This
        // allows us to run the same set of tests against a native File, and
        // VirtualFile. We trust the native Files and wouldn't need to test them,
        // but this allows us to verify that the operations return the same
        // results with VirtualFiles as with native Files. (Except that with
        // native files, you will run out of file descriptors if the ulimit
        // is low enough.)
        test_files("virtual_files", |path, open_options| async move {
            let vf = VirtualFile::open_with_options(&path, &open_options).await?;
            Ok(MaybeVirtualFile::VirtualFile(vf))
        })
        .await
    }

    #[tokio::test]
    async fn test_physical_files() -> Result<(), Error> {
        test_files("physical_files", |path, open_options| async move {
            Ok(MaybeVirtualFile::File(open_options.open(path)?))
        })
        .await
    }

    async fn test_files<OF, FT>(testname: &str, openfunc: OF) -> Result<(), Error>
    where
        OF: Fn(Utf8PathBuf, OpenOptions) -> FT,
        FT: Future<Output = Result<MaybeVirtualFile, std::io::Error>>,
    {
        let testdir = crate::config::PageServerConf::test_repo_dir(testname);
        std::fs::create_dir_all(&testdir)?;

        let path_a = testdir.join("file_a");
        let mut file_a = openfunc(
            path_a.clone(),
            OpenOptions::new()
                .write(true)
                .create(true)
                .truncate(true)
                .to_owned(),
        )
        .await?;
        file_a.write_all(b"foobar").await?;

        // cannot read from a file opened in write-only mode
        let _ = file_a.read_string().await.unwrap_err();

        // Close the file and re-open for reading
        let mut file_a = openfunc(path_a, OpenOptions::new().read(true).to_owned()).await?;

        // cannot write to a file opened in read-only mode
        let _ = file_a.write_all(b"bar").await.unwrap_err();

        // Try simple read
        assert_eq!("foobar", file_a.read_string().await?);

        // It's positioned at the EOF now.
        assert_eq!("", file_a.read_string().await?);

        // Test seeks.
        assert_eq!(file_a.seek(SeekFrom::Start(1)).await?, 1);
        assert_eq!("oobar", file_a.read_string().await?);

        assert_eq!(file_a.seek(SeekFrom::End(-2)).await?, 4);
        assert_eq!("ar", file_a.read_string().await?);

        assert_eq!(file_a.seek(SeekFrom::Start(1)).await?, 1);
        assert_eq!(file_a.seek(SeekFrom::Current(2)).await?, 3);
        assert_eq!("bar", file_a.read_string().await?);

        assert_eq!(file_a.seek(SeekFrom::Current(-5)).await?, 1);
        assert_eq!("oobar", file_a.read_string().await?);

        // Test erroneous seeks to before byte 0
        file_a.seek(SeekFrom::End(-7)).await.unwrap_err();
        assert_eq!(file_a.seek(SeekFrom::Start(1)).await?, 1);
        file_a.seek(SeekFrom::Current(-2)).await.unwrap_err();

        // the erroneous seek should have left the position unchanged
        assert_eq!("oobar", file_a.read_string().await?);

        // Create another test file, and try FileExt functions on it.
        let path_b = testdir.join("file_b");
        let mut file_b = openfunc(
            path_b.clone(),
            OpenOptions::new()
                .read(true)
                .write(true)
                .create(true)
                .truncate(true)
                .to_owned(),
        )
        .await?;
        file_b.write_all_at(b"BAR", 3).await?;
        file_b.write_all_at(b"FOO", 0).await?;

        assert_eq!(file_b.read_string_at(2, 3).await?, "OBA");

        // Open a lot of files, enough to cause some evictions. (Or to be precise,
        // open the same file many times. The effect is the same.)
        //
        // leave file_a positioned at offset 1 before we start
        assert_eq!(file_a.seek(SeekFrom::Start(1)).await?, 1);

        let mut vfiles = Vec::new();
        for _ in 0..100 {
            let mut vfile =
                openfunc(path_b.clone(), OpenOptions::new().read(true).to_owned()).await?;
            assert_eq!("FOOBAR", vfile.read_string().await?);
            vfiles.push(vfile);
        }

        // make sure we opened enough files to definitely cause evictions.
        assert!(vfiles.len() > TEST_MAX_FILE_DESCRIPTORS * 2);

        // The underlying file descriptor for 'file_a' should be closed now. Try to read
        // from it again. We left the file positioned at offset 1 above.
        assert_eq!("oobar", file_a.read_string().await?);

        // Check that all the other FDs still work too. Use them in random order for
        // good measure.
        vfiles.as_mut_slice().shuffle(&mut thread_rng());
        for vfile in vfiles.iter_mut() {
            assert_eq!("OOBAR", vfile.read_string_at(1, 5).await?);
        }

        Ok(())
    }

    /// Test using VirtualFiles from many threads concurrently. This tests both using
    /// a lot of VirtualFiles concurrently, causing evictions, and also using the same
    /// VirtualFile from multiple threads concurrently.
    #[tokio::test]
    async fn test_vfile_concurrency() -> Result<(), Error> {
        const SIZE: usize = 8 * 1024;
        const VIRTUAL_FILES: usize = 100;
        const THREADS: usize = 100;
        const SAMPLE: [u8; SIZE] = [0xADu8; SIZE];

        let testdir = crate::config::PageServerConf::test_repo_dir("vfile_concurrency");
        std::fs::create_dir_all(&testdir)?;

        // Create a test file.
        let test_file_path = testdir.join("concurrency_test_file");
        {
            let file = File::create(&test_file_path)?;
            file.write_all_at(&SAMPLE, 0)?;
        }

        // Open the file many times.
        let mut files = Vec::new();
        for _ in 0..VIRTUAL_FILES {
            let f = VirtualFile::open_with_options(&test_file_path, OpenOptions::new().read(true))
                .await?;
            files.push(f);
        }
        let files = Arc::new(files);

        // Launch many threads, and use the virtual files concurrently in random order.
        let rt = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(THREADS)
            .thread_name("test_vfile_concurrency thread")
            .build()
            .unwrap();
        let mut hdls = Vec::new();
        for _threadno in 0..THREADS {
            let files = files.clone();
            let hdl = rt.spawn(async move {
                let mut buf = [0u8; SIZE];
                let mut rng = rand::rngs::OsRng;
                for _ in 1..1000 {
                    let f = &files[rng.gen_range(0..files.len())];
                    f.read_exact_at(&mut buf, 0).await.unwrap();
                    assert!(buf == SAMPLE);
                }
            });
            hdls.push(hdl);
        }
        for hdl in hdls {
            hdl.await?;
        }
        std::mem::forget(rt);

        Ok(())
    }

    #[tokio::test]
    async fn test_atomic_overwrite_basic() {
        let testdir = crate::config::PageServerConf::test_repo_dir("test_atomic_overwrite_basic");
        std::fs::create_dir_all(&testdir).unwrap();

        let path = testdir.join("myfile");
        let tmp_path = testdir.join("myfile.tmp");

        VirtualFile::crashsafe_overwrite(&path, &tmp_path, b"foo")
            .await
            .unwrap();
        let mut file = MaybeVirtualFile::from(VirtualFile::open(&path).await.unwrap());
        let post = file.read_string().await.unwrap();
        assert_eq!(post, "foo");
        assert!(!tmp_path.exists());
        drop(file);

        VirtualFile::crashsafe_overwrite(&path, &tmp_path, b"bar")
            .await
            .unwrap();
        let mut file = MaybeVirtualFile::from(VirtualFile::open(&path).await.unwrap());
        let post = file.read_string().await.unwrap();
        assert_eq!(post, "bar");
        assert!(!tmp_path.exists());
        drop(file);
    }

    #[tokio::test]
    async fn test_atomic_overwrite_preexisting_tmp() {
        let testdir =
            crate::config::PageServerConf::test_repo_dir("test_atomic_overwrite_preexisting_tmp");
        std::fs::create_dir_all(&testdir).unwrap();

        let path = testdir.join("myfile");
        let tmp_path = testdir.join("myfile.tmp");

        std::fs::write(&tmp_path, "some preexisting junk that should be removed").unwrap();
        assert!(tmp_path.exists());

        VirtualFile::crashsafe_overwrite(&path, &tmp_path, b"foo")
            .await
            .unwrap();

        let mut file = MaybeVirtualFile::from(VirtualFile::open(&path).await.unwrap());
        let post = file.read_string().await.unwrap();
        assert_eq!(post, "foo");
        assert!(!tmp_path.exists());
        drop(file);
    }
}
