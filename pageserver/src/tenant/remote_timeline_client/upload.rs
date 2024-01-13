//! Helper functions to upload files to remote storage with a RemoteStorage

use anyhow::{bail, Context};
use camino::Utf8Path;
use fail::fail_point;
use pageserver_api::shard::TenantShardId;
use std::io::{ErrorKind, SeekFrom};
use tokio::fs::{self, File};
use tokio::io::AsyncSeekExt;
use tokio_util::sync::CancellationToken;

use super::Generation;
use crate::{
    config::PageServerConf,
    tenant::remote_timeline_client::{
        index::IndexPart, remote_index_path, remote_initdb_archive_path, remote_path,
        upload_cancellable,
    },
};
use remote_storage::GenericRemoteStorage;
use utils::id::{TenantId, TimelineId};

use super::index::LayerFileMetadata;

use tracing::info;

/// Serializes and uploads the given index part data to the remote storage.
pub(super) async fn upload_index_part<'a>(
    storage: &'a GenericRemoteStorage,
    tenant_shard_id: &TenantShardId,
    timeline_id: &TimelineId,
    generation: Generation,
    index_part: &'a IndexPart,
    cancel: &CancellationToken,
) -> anyhow::Result<()> {
    tracing::trace!("uploading new index part");

    fail_point!("before-upload-index", |_| {
        bail!("failpoint before-upload-index")
    });
    pausable_failpoint!("before-upload-index-pausable");

    let index_part_bytes = index_part
        .to_s3_bytes()
        .context("serialize index part file into bytes")?;
    let index_part_size = index_part_bytes.len();
    let index_part_bytes = bytes::Bytes::from(index_part_bytes);

    let remote_path = remote_index_path(tenant_shard_id, timeline_id, generation);
    upload_cancellable(
        cancel,
        storage.upload_storage_object(
            futures::stream::once(futures::future::ready(Ok(index_part_bytes))),
            index_part_size,
            &remote_path,
        ),
    )
    .await
    .with_context(|| format!("upload index part for '{tenant_shard_id} / {timeline_id}'"))
}

/// Attempts to upload given layer files.
/// No extra checks for overlapping files is made and any files that are already present remotely will be overwritten, if submitted during the upload.
///
/// On an error, bumps the retries count and reschedules the entire task.
pub(super) async fn upload_timeline_layer<'a>(
    conf: &'static PageServerConf,
    storage: &'a GenericRemoteStorage,
    source_path: &'a Utf8Path,
    known_metadata: &'a LayerFileMetadata,
    generation: Generation,
    cancel: &CancellationToken,
) -> anyhow::Result<()> {
    fail_point!("before-upload-layer", |_| {
        bail!("failpoint before-upload-layer")
    });

    pausable_failpoint!("before-upload-layer-pausable");

    let storage_path = remote_path(conf, source_path, generation)?;
    let source_file_res = fs::File::open(&source_path).await;
    let source_file = match source_file_res {
        Ok(source_file) => source_file,
        Err(e) if e.kind() == ErrorKind::NotFound => {
            // If we encounter this arm, it wasn't intended, but it's also not
            // a big problem, if it's because the file was deleted before an
            // upload. However, a nonexistent file can also be indicative of
            // something worse, like when a file is scheduled for upload before
            // it has been written to disk yet.
            //
            // This is tested against `test_compaction_delete_before_upload`
            info!(path = %source_path, "File to upload doesn't exist. Likely the file has been deleted and an upload is not required any more.");
            return Ok(());
        }
        Err(e) => {
            Err(e).with_context(|| format!("open a source file for layer {source_path:?}"))?
        }
    };

    let fs_size = source_file
        .metadata()
        .await
        .with_context(|| format!("get the source file metadata for layer {source_path:?}"))?
        .len();

    let metadata_size = known_metadata.file_size();
    if metadata_size != fs_size {
        bail!("File {source_path:?} has its current FS size {fs_size} diferent from initially determined {metadata_size}");
    }

    let fs_size = usize::try_from(fs_size)
        .with_context(|| format!("convert {source_path:?} size {fs_size} usize"))?;

    let reader = tokio_util::io::ReaderStream::with_capacity(source_file, super::BUFFER_SIZE);

    upload_cancellable(cancel, storage.upload(reader, fs_size, &storage_path, None))
        .await
        .with_context(|| format!("upload layer from local path '{source_path}'"))?;

    Ok(())
}

/// Uploads the given `initdb` data to the remote storage.
pub(crate) async fn upload_initdb_dir(
    storage: &GenericRemoteStorage,
    tenant_id: &TenantId,
    timeline_id: &TimelineId,
    mut initdb_tar_zst: File,
    size: u64,
    cancel: &CancellationToken,
) -> anyhow::Result<()> {
    tracing::trace!("uploading initdb dir");

    // We might have read somewhat into the file already in the prior retry attempt
    initdb_tar_zst.seek(SeekFrom::Start(0)).await?;

    let file = tokio_util::io::ReaderStream::with_capacity(initdb_tar_zst, super::BUFFER_SIZE);

    let remote_path = remote_initdb_archive_path(tenant_id, timeline_id);
    upload_cancellable(
        cancel,
        storage.upload_storage_object(file, size as usize, &remote_path),
    )
    .await
    .with_context(|| format!("upload initdb dir for '{tenant_id} / {timeline_id}'"))
}
