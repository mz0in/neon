//
//! The Page Service listens for client connections and serves their GetPage@LSN
//! requests.
//
//   It is possible to connect here using usual psql/pgbench/libpq. Following
// commands are supported now:
//     *status* -- show actual info about this pageserver,
//     *pagestream* -- enter mode where smgr and pageserver talk with their
//  custom protocol.
//

use anyhow::Context;
use async_compression::tokio::write::GzipEncoder;
use bytes::Buf;
use bytes::Bytes;
use futures::Stream;
use pageserver_api::models::TenantState;
use pageserver_api::models::{
    PagestreamBeMessage, PagestreamDbSizeRequest, PagestreamDbSizeResponse,
    PagestreamErrorResponse, PagestreamExistsRequest, PagestreamExistsResponse,
    PagestreamFeMessage, PagestreamGetPageRequest, PagestreamGetPageResponse,
    PagestreamNblocksRequest, PagestreamNblocksResponse,
};
use postgres_backend::{self, is_expected_io_error, AuthType, PostgresBackend, QueryError};
use pq_proto::framed::ConnectionError;
use pq_proto::FeStartupPacket;
use pq_proto::{BeMessage, FeMessage, RowDescriptor};
use std::borrow::Cow;
use std::io;
use std::net::TcpListener;
use std::pin::pin;
use std::str;
use std::str::FromStr;
use std::sync::Arc;
use std::time::Duration;
use tokio::io::AsyncWriteExt;
use tokio::io::{AsyncRead, AsyncWrite};
use tokio_util::io::StreamReader;
use tokio_util::sync::CancellationToken;
use tracing::field;
use tracing::*;
use utils::id::ConnectionId;
use utils::{
    auth::{Claims, Scope, SwappableJwtAuth},
    id::{TenantId, TimelineId},
    lsn::Lsn,
    simple_rcu::RcuReadGuard,
};

use crate::auth::check_permission;
use crate::basebackup;
use crate::config::PageServerConf;
use crate::context::{DownloadBehavior, RequestContext};
use crate::import_datadir::import_wal_from_tar;
use crate::metrics;
use crate::metrics::LIVE_CONNECTIONS_COUNT;
use crate::pgdatadir_mapping::{rel_block_to_key, Version};
use crate::task_mgr;
use crate::task_mgr::TaskKind;
use crate::tenant::debug_assert_current_span_has_tenant_and_timeline_id;
use crate::tenant::mgr;
use crate::tenant::mgr::get_active_tenant_with_timeout;
use crate::tenant::mgr::GetActiveTenantError;
use crate::tenant::mgr::ShardSelector;
use crate::tenant::timeline::WaitLsnError;
use crate::tenant::GetTimelineError;
use crate::tenant::PageReconstructError;
use crate::tenant::Timeline;
use crate::trace::Tracer;

use postgres_ffi::pg_constants::DEFAULTTABLESPACE_OID;
use postgres_ffi::BLCKSZ;

// How long we may wait for a [`TenantSlot::InProgress`]` and/or a [`Tenant`] which
// is not yet in state [`TenantState::Active`].
const ACTIVE_TENANT_TIMEOUT: Duration = Duration::from_millis(30000);

/// Read the end of a tar archive.
///
/// A tar archive normally ends with two consecutive blocks of zeros, 512 bytes each.
/// `tokio_tar` already read the first such block. Read the second all-zeros block,
/// and check that there is no more data after the EOF marker.
///
/// XXX: Currently, any trailing data after the EOF marker prints a warning.
/// Perhaps it should be a hard error?
async fn read_tar_eof(mut reader: (impl AsyncRead + Unpin)) -> anyhow::Result<()> {
    use tokio::io::AsyncReadExt;
    let mut buf = [0u8; 512];

    // Read the all-zeros block, and verify it
    let mut total_bytes = 0;
    while total_bytes < 512 {
        let nbytes = reader.read(&mut buf[total_bytes..]).await?;
        total_bytes += nbytes;
        if nbytes == 0 {
            break;
        }
    }
    if total_bytes < 512 {
        anyhow::bail!("incomplete or invalid tar EOF marker");
    }
    if !buf.iter().all(|&x| x == 0) {
        anyhow::bail!("invalid tar EOF marker");
    }

    // Drain any data after the EOF marker
    let mut trailing_bytes = 0;
    loop {
        let nbytes = reader.read(&mut buf).await?;
        trailing_bytes += nbytes;
        if nbytes == 0 {
            break;
        }
    }
    if trailing_bytes > 0 {
        warn!("ignored {trailing_bytes} unexpected bytes after the tar archive");
    }
    Ok(())
}

///////////////////////////////////////////////////////////////////////////////

///
/// Main loop of the page service.
///
/// Listens for connections, and launches a new handler task for each.
///
pub async fn libpq_listener_main(
    conf: &'static PageServerConf,
    broker_client: storage_broker::BrokerClientChannel,
    auth: Option<Arc<SwappableJwtAuth>>,
    listener: TcpListener,
    auth_type: AuthType,
    listener_ctx: RequestContext,
    cancel: CancellationToken,
) -> anyhow::Result<()> {
    listener.set_nonblocking(true)?;
    let tokio_listener = tokio::net::TcpListener::from_std(listener)?;

    // Wait for a new connection to arrive, or for server shutdown.
    while let Some(res) = tokio::select! {
        biased;

        _ = cancel.cancelled() => {
            // We were requested to shut down.
            None
        }

        res = tokio_listener.accept() => {
            Some(res)
        }
    } {
        match res {
            Ok((socket, peer_addr)) => {
                // Connection established. Spawn a new task to handle it.
                debug!("accepted connection from {}", peer_addr);
                let local_auth = auth.clone();

                let connection_ctx = listener_ctx
                    .detached_child(TaskKind::PageRequestHandler, DownloadBehavior::Download);

                // PageRequestHandler tasks are not associated with any particular
                // timeline in the task manager. In practice most connections will
                // only deal with a particular timeline, but we don't know which one
                // yet.
                task_mgr::spawn(
                    &tokio::runtime::Handle::current(),
                    TaskKind::PageRequestHandler,
                    None,
                    None,
                    "serving compute connection task",
                    false,
                    page_service_conn_main(
                        conf,
                        broker_client.clone(),
                        local_auth,
                        socket,
                        auth_type,
                        connection_ctx,
                    ),
                );
            }
            Err(err) => {
                // accept() failed. Log the error, and loop back to retry on next connection.
                error!("accept() failed: {:?}", err);
            }
        }
    }

    debug!("page_service loop terminated");

    Ok(())
}

#[instrument(skip_all, fields(peer_addr))]
async fn page_service_conn_main(
    conf: &'static PageServerConf,
    broker_client: storage_broker::BrokerClientChannel,
    auth: Option<Arc<SwappableJwtAuth>>,
    socket: tokio::net::TcpStream,
    auth_type: AuthType,
    connection_ctx: RequestContext,
) -> anyhow::Result<()> {
    // Immediately increment the gauge, then create a job to decrement it on task exit.
    // One of the pros of `defer!` is that this will *most probably*
    // get called, even in presence of panics.
    let gauge = LIVE_CONNECTIONS_COUNT.with_label_values(&["page_service"]);
    gauge.inc();
    scopeguard::defer! {
        gauge.dec();
    }

    socket
        .set_nodelay(true)
        .context("could not set TCP_NODELAY")?;

    let peer_addr = socket.peer_addr().context("get peer address")?;
    tracing::Span::current().record("peer_addr", field::display(peer_addr));

    // setup read timeout of 10 minutes. the timeout is rather arbitrary for requirements:
    // - long enough for most valid compute connections
    // - less than infinite to stop us from "leaking" connections to long-gone computes
    //
    // no write timeout is used, because the kernel is assumed to error writes after some time.
    let mut socket = tokio_io_timeout::TimeoutReader::new(socket);

    let default_timeout_ms = 10 * 60 * 1000; // 10 minutes by default
    let socket_timeout_ms = (|| {
        fail::fail_point!("simulated-bad-compute-connection", |avg_timeout_ms| {
            // Exponential distribution for simulating
            // poor network conditions, expect about avg_timeout_ms to be around 15
            // in tests
            if let Some(avg_timeout_ms) = avg_timeout_ms {
                let avg = avg_timeout_ms.parse::<i64>().unwrap() as f32;
                let u = rand::random::<f32>();
                ((1.0 - u).ln() / (-avg)) as u64
            } else {
                default_timeout_ms
            }
        });
        default_timeout_ms
    })();

    // A timeout here does not mean the client died, it can happen if it's just idle for
    // a while: we will tear down this PageServerHandler and instantiate a new one if/when
    // they reconnect.
    socket.set_timeout(Some(std::time::Duration::from_millis(socket_timeout_ms)));
    let socket = std::pin::pin!(socket);

    // XXX: pgbackend.run() should take the connection_ctx,
    // and create a child per-query context when it invokes process_query.
    // But it's in a shared crate, so, we store connection_ctx inside PageServerHandler
    // and create the per-query context in process_query ourselves.
    let mut conn_handler = PageServerHandler::new(conf, broker_client, auth, connection_ctx);
    let pgbackend = PostgresBackend::new_from_io(socket, peer_addr, auth_type, None)?;

    match pgbackend
        .run(&mut conn_handler, task_mgr::shutdown_watcher)
        .await
    {
        Ok(()) => {
            // we've been requested to shut down
            Ok(())
        }
        Err(QueryError::Disconnected(ConnectionError::Io(io_error))) => {
            if is_expected_io_error(&io_error) {
                info!("Postgres client disconnected ({io_error})");
                Ok(())
            } else {
                Err(io_error).context("Postgres connection error")
            }
        }
        other => other.context("Postgres query error"),
    }
}

struct PageServerHandler {
    _conf: &'static PageServerConf,
    broker_client: storage_broker::BrokerClientChannel,
    auth: Option<Arc<SwappableJwtAuth>>,
    claims: Option<Claims>,

    /// The context created for the lifetime of the connection
    /// services by this PageServerHandler.
    /// For each query received over the connection,
    /// `process_query` creates a child context from this one.
    connection_ctx: RequestContext,
}

#[derive(thiserror::Error, Debug)]
enum PageStreamError {
    /// We encountered an error that should prompt the client to reconnect:
    /// in practice this means we drop the connection without sending a response.
    #[error("Reconnect required: {0}")]
    Reconnect(Cow<'static, str>),

    /// We were instructed to shutdown while processing the query
    #[error("Shutting down")]
    Shutdown,

    /// Something went wrong reading a page: this likely indicates a pageserver bug
    #[error("Read error: {0}")]
    Read(PageReconstructError),

    /// Ran out of time waiting for an LSN
    #[error("LSN timeout: {0}")]
    LsnTimeout(WaitLsnError),

    /// The entity required to serve the request (tenant or timeline) is not found,
    /// or is not found in a suitable state to serve a request.
    #[error("Not found: {0}")]
    NotFound(std::borrow::Cow<'static, str>),

    /// Request asked for something that doesn't make sense, like an invalid LSN
    #[error("Bad request: {0}")]
    BadRequest(std::borrow::Cow<'static, str>),
}

impl From<PageReconstructError> for PageStreamError {
    fn from(value: PageReconstructError) -> Self {
        match value {
            PageReconstructError::Cancelled => Self::Shutdown,
            e => Self::Read(e),
        }
    }
}

impl From<GetActiveTimelineError> for PageStreamError {
    fn from(value: GetActiveTimelineError) -> Self {
        match value {
            GetActiveTimelineError::Tenant(GetActiveTenantError::Cancelled) => Self::Shutdown,
            GetActiveTimelineError::Tenant(e) => Self::NotFound(format!("{e}").into()),
            GetActiveTimelineError::Timeline(e) => Self::NotFound(format!("{e}").into()),
        }
    }
}

impl From<WaitLsnError> for PageStreamError {
    fn from(value: WaitLsnError) -> Self {
        match value {
            e @ WaitLsnError::Timeout(_) => Self::LsnTimeout(e),
            WaitLsnError::Shutdown => Self::Shutdown,
            WaitLsnError::BadState => Self::Reconnect("Timeline is not active".into()),
        }
    }
}

impl PageServerHandler {
    pub fn new(
        conf: &'static PageServerConf,
        broker_client: storage_broker::BrokerClientChannel,
        auth: Option<Arc<SwappableJwtAuth>>,
        connection_ctx: RequestContext,
    ) -> Self {
        PageServerHandler {
            _conf: conf,
            broker_client,
            auth,
            claims: None,
            connection_ctx,
        }
    }

    /// Wrap PostgresBackend::flush to respect our CancellationToken: it is important to use
    /// this rather than naked flush() in order to shut down promptly.  Without this, we would
    /// block shutdown of a tenant if a postgres client was failing to consume bytes we send
    /// in the flush.
    async fn flush_cancellable<IO>(
        &self,
        pgb: &mut PostgresBackend<IO>,
        cancel: &CancellationToken,
    ) -> Result<(), QueryError>
    where
        IO: AsyncRead + AsyncWrite + Send + Sync + Unpin,
    {
        tokio::select!(
            flush_r = pgb.flush() => {
                Ok(flush_r?)
            },
            _ = cancel.cancelled() => {
                Err(QueryError::Shutdown)
            }
        )
    }

    fn copyin_stream<'a, IO>(
        &'a self,
        pgb: &'a mut PostgresBackend<IO>,
        cancel: &'a CancellationToken,
    ) -> impl Stream<Item = io::Result<Bytes>> + 'a
    where
        IO: AsyncRead + AsyncWrite + Send + Sync + Unpin,
    {
        async_stream::try_stream! {
            loop {
                let msg = tokio::select! {
                    biased;

                    _ = cancel.cancelled() => {
                        // We were requested to shut down.
                        let msg = "pageserver is shutting down";
                        let _ = pgb.write_message_noflush(&BeMessage::ErrorResponse(msg, None));
                        Err(QueryError::Shutdown)
                    }

                    msg = pgb.read_message() => { msg.map_err(QueryError::from)}
                };

                match msg {
                    Ok(Some(message)) => {
                        let copy_data_bytes = match message {
                            FeMessage::CopyData(bytes) => bytes,
                            FeMessage::CopyDone => { break },
                            FeMessage::Sync => continue,
                            FeMessage::Terminate => {
                                let msg = "client terminated connection with Terminate message during COPY";
                                let query_error = QueryError::Disconnected(ConnectionError::Io(io::Error::new(io::ErrorKind::ConnectionReset, msg)));
                                // error can't happen here, ErrorResponse serialization should be always ok
                                pgb.write_message_noflush(&BeMessage::ErrorResponse(msg, Some(query_error.pg_error_code()))).map_err(|e| e.into_io_error())?;
                                Err(io::Error::new(io::ErrorKind::ConnectionReset, msg))?;
                                break;
                            }
                            m => {
                                let msg = format!("unexpected message {m:?}");
                                // error can't happen here, ErrorResponse serialization should be always ok
                                pgb.write_message_noflush(&BeMessage::ErrorResponse(&msg, None)).map_err(|e| e.into_io_error())?;
                                Err(io::Error::new(io::ErrorKind::Other, msg))?;
                                break;
                            }
                        };

                        yield copy_data_bytes;
                    }
                    Ok(None) => {
                        let msg = "client closed connection during COPY";
                        let query_error = QueryError::Disconnected(ConnectionError::Io(io::Error::new(io::ErrorKind::ConnectionReset, msg)));
                        // error can't happen here, ErrorResponse serialization should be always ok
                        pgb.write_message_noflush(&BeMessage::ErrorResponse(msg, Some(query_error.pg_error_code()))).map_err(|e| e.into_io_error())?;
                        self.flush_cancellable(pgb, cancel).await.map_err(|e| io::Error::new(io::ErrorKind::Other, e.to_string()))?;
                        Err(io::Error::new(io::ErrorKind::ConnectionReset, msg))?;
                    }
                    Err(QueryError::Disconnected(ConnectionError::Io(io_error))) => {
                        Err(io_error)?;
                    }
                    Err(other) => {
                        Err(io::Error::new(io::ErrorKind::Other, other.to_string()))?;
                    }
                };
            }
        }
    }

    #[instrument(skip_all)]
    async fn handle_pagerequests<IO>(
        &self,
        pgb: &mut PostgresBackend<IO>,
        tenant_id: TenantId,
        timeline_id: TimelineId,
        ctx: RequestContext,
    ) -> Result<(), QueryError>
    where
        IO: AsyncRead + AsyncWrite + Send + Sync + Unpin,
    {
        debug_assert_current_span_has_tenant_and_timeline_id();

        // Note that since one connection may contain getpage requests that target different
        // shards (e.g. during splitting when the compute is not yet aware of the split), the tenant
        // that we look up here may not be the one that serves all the actual requests: we will double
        // check the mapping of key->shard later before calling into Timeline for getpage requests.
        let tenant = mgr::get_active_tenant_with_timeout(
            tenant_id,
            ShardSelector::First,
            ACTIVE_TENANT_TIMEOUT,
            &task_mgr::shutdown_token(),
        )
        .await?;

        // Make request tracer if needed
        let mut tracer = if tenant.get_trace_read_requests() {
            let connection_id = ConnectionId::generate();
            let path =
                tenant
                    .conf
                    .trace_path(&tenant.tenant_shard_id(), &timeline_id, &connection_id);
            Some(Tracer::new(path))
        } else {
            None
        };

        // Check that the timeline exists
        let timeline = tenant
            .get_timeline(timeline_id, true)
            .map_err(|e| QueryError::NotFound(format!("{e}").into()))?;

        // Avoid starting new requests if the timeline has already started shutting down,
        // and block timeline shutdown until this request is complete, or drops out due
        // to cancellation.
        let _timeline_guard = timeline.gate.enter().map_err(|_| QueryError::Shutdown)?;

        // switch client to COPYBOTH
        pgb.write_message_noflush(&BeMessage::CopyBothResponse)?;
        self.flush_cancellable(pgb, &timeline.cancel).await?;

        let metrics = metrics::SmgrQueryTimePerTimeline::new(&tenant_id, &timeline_id);

        loop {
            let msg = tokio::select! {
                biased;

                _ = timeline.cancel.cancelled() => {
                    // We were requested to shut down.
                    info!("shutdown request received in page handler");
                    return Err(QueryError::Shutdown)
                }

                msg = pgb.read_message() => { msg }
            };

            let copy_data_bytes = match msg? {
                Some(FeMessage::CopyData(bytes)) => bytes,
                Some(FeMessage::Terminate) => break,
                Some(m) => {
                    return Err(QueryError::Other(anyhow::anyhow!(
                        "unexpected message: {m:?} during COPY"
                    )));
                }
                None => break, // client disconnected
            };

            trace!("query: {copy_data_bytes:?}");

            // Trace request if needed
            if let Some(t) = tracer.as_mut() {
                t.trace(&copy_data_bytes)
            }

            let neon_fe_msg = PagestreamFeMessage::parse(&mut copy_data_bytes.reader())?;

            // TODO: We could create a new per-request context here, with unique ID.
            // Currently we use the same per-timeline context for all requests

            let (response, span) = match neon_fe_msg {
                PagestreamFeMessage::Exists(req) => {
                    let _timer = metrics.start_timer(metrics::SmgrQueryType::GetRelExists);
                    let span = tracing::info_span!("handle_get_rel_exists_request", rel = %req.rel, req_lsn = %req.lsn);
                    (
                        self.handle_get_rel_exists_request(&timeline, &req, &ctx)
                            .instrument(span.clone())
                            .await,
                        span,
                    )
                }
                PagestreamFeMessage::Nblocks(req) => {
                    let _timer = metrics.start_timer(metrics::SmgrQueryType::GetRelSize);
                    let span = tracing::info_span!("handle_get_nblocks_request", rel = %req.rel, req_lsn = %req.lsn);
                    (
                        self.handle_get_nblocks_request(&timeline, &req, &ctx)
                            .instrument(span.clone())
                            .await,
                        span,
                    )
                }
                PagestreamFeMessage::GetPage(req) => {
                    let _timer = metrics.start_timer(metrics::SmgrQueryType::GetPageAtLsn);
                    let span = tracing::info_span!("handle_get_page_at_lsn_request", rel = %req.rel, blkno = %req.blkno, req_lsn = %req.lsn);
                    (
                        self.handle_get_page_at_lsn_request(&timeline, &req, &ctx)
                            .instrument(span.clone())
                            .await,
                        span,
                    )
                }
                PagestreamFeMessage::DbSize(req) => {
                    let _timer = metrics.start_timer(metrics::SmgrQueryType::GetDbSize);
                    let span = tracing::info_span!("handle_db_size_request", dbnode = %req.dbnode, req_lsn = %req.lsn);
                    (
                        self.handle_db_size_request(&timeline, &req, &ctx)
                            .instrument(span.clone())
                            .await,
                        span,
                    )
                }
            };

            match response {
                Err(PageStreamError::Shutdown) => {
                    // If we fail to fulfil a request during shutdown, which may be _because_ of
                    // shutdown, then do not send the error to the client.  Instead just drop the
                    // connection.
                    span.in_scope(|| info!("dropping connection due to shutdown"));
                    return Err(QueryError::Shutdown);
                }
                Err(PageStreamError::Reconnect(reason)) => {
                    span.in_scope(|| info!("handler requested reconnect: {reason}"));
                    return Err(QueryError::Reconnect);
                }
                Err(e) if timeline.cancel.is_cancelled() || timeline.is_stopping() => {
                    // This branch accomodates code within request handlers that returns an anyhow::Error instead of a clean
                    // shutdown error, this may be buried inside a PageReconstructError::Other for example.
                    //
                    // Requests may fail as soon as we are Stopping, even if the Timeline's cancellation token wasn't fired yet,
                    // because wait_lsn etc will drop out
                    // is_stopping(): [`Timeline::flush_and_shutdown`] has entered
                    // is_canceled(): [`Timeline::shutdown`]` has entered
                    span.in_scope(|| info!("dropped error response during shutdown: {e:#}"));
                    return Err(QueryError::Shutdown);
                }
                r => {
                    let response_msg = r.unwrap_or_else(|e| {
                        // print the all details to the log with {:#}, but for the client the
                        // error message is enough.  Do not log if shutting down, as the anyhow::Error
                        // here includes cancellation which is not an error.
                        span.in_scope(|| error!("error reading relation or page version: {:#}", e));
                        PagestreamBeMessage::Error(PagestreamErrorResponse {
                            message: e.to_string(),
                        })
                    });

                    pgb.write_message_noflush(&BeMessage::CopyData(&response_msg.serialize()))?;
                    self.flush_cancellable(pgb, &timeline.cancel).await?;
                }
            }
        }
        Ok(())
    }

    #[allow(clippy::too_many_arguments)]
    #[instrument(skip_all, fields(%base_lsn, end_lsn=%_end_lsn, %pg_version))]
    async fn handle_import_basebackup<IO>(
        &self,
        pgb: &mut PostgresBackend<IO>,
        tenant_id: TenantId,
        timeline_id: TimelineId,
        base_lsn: Lsn,
        _end_lsn: Lsn,
        pg_version: u32,
        ctx: RequestContext,
    ) -> Result<(), QueryError>
    where
        IO: AsyncRead + AsyncWrite + Send + Sync + Unpin,
    {
        debug_assert_current_span_has_tenant_and_timeline_id();

        // Create empty timeline
        info!("creating new timeline");
        let tenant = get_active_tenant_with_timeout(
            tenant_id,
            ShardSelector::Zero,
            ACTIVE_TENANT_TIMEOUT,
            &task_mgr::shutdown_token(),
        )
        .await?;
        let timeline = tenant
            .create_empty_timeline(timeline_id, base_lsn, pg_version, &ctx)
            .await?;

        // TODO mark timeline as not ready until it reaches end_lsn.
        // We might have some wal to import as well, and we should prevent compute
        // from connecting before that and writing conflicting wal.
        //
        // This is not relevant for pageserver->pageserver migrations, since there's
        // no wal to import. But should be fixed if we want to import from postgres.

        // TODO leave clean state on error. For now you can use detach to clean
        // up broken state from a failed import.

        // Import basebackup provided via CopyData
        info!("importing basebackup");
        pgb.write_message_noflush(&BeMessage::CopyInResponse)?;
        self.flush_cancellable(pgb, &tenant.cancel).await?;

        let mut copyin_reader = pin!(StreamReader::new(self.copyin_stream(pgb, &tenant.cancel)));
        timeline
            .import_basebackup_from_tar(
                &mut copyin_reader,
                base_lsn,
                self.broker_client.clone(),
                &ctx,
            )
            .await?;

        // Read the end of the tar archive.
        read_tar_eof(copyin_reader).await?;

        // TODO check checksum
        // Meanwhile you can verify client-side by taking fullbackup
        // and checking that it matches in size with what was imported.
        // It wouldn't work if base came from vanilla postgres though,
        // since we discard some log files.

        info!("done");
        Ok(())
    }

    #[instrument(skip_all, fields(%start_lsn, %end_lsn))]
    async fn handle_import_wal<IO>(
        &self,
        pgb: &mut PostgresBackend<IO>,
        tenant_id: TenantId,
        timeline_id: TimelineId,
        start_lsn: Lsn,
        end_lsn: Lsn,
        ctx: RequestContext,
    ) -> Result<(), QueryError>
    where
        IO: AsyncRead + AsyncWrite + Send + Sync + Unpin,
    {
        debug_assert_current_span_has_tenant_and_timeline_id();

        let timeline = self
            .get_active_tenant_timeline(tenant_id, timeline_id, ShardSelector::Zero)
            .await?;
        let last_record_lsn = timeline.get_last_record_lsn();
        if last_record_lsn != start_lsn {
            return Err(QueryError::Other(
                anyhow::anyhow!("Cannot import WAL from Lsn {start_lsn} because timeline does not start from the same lsn: {last_record_lsn}"))
            );
        }

        // TODO leave clean state on error. For now you can use detach to clean
        // up broken state from a failed import.

        // Import wal provided via CopyData
        info!("importing wal");
        pgb.write_message_noflush(&BeMessage::CopyInResponse)?;
        self.flush_cancellable(pgb, &timeline.cancel).await?;
        let mut copyin_reader = pin!(StreamReader::new(self.copyin_stream(pgb, &timeline.cancel)));
        import_wal_from_tar(&timeline, &mut copyin_reader, start_lsn, end_lsn, &ctx).await?;
        info!("wal import complete");

        // Read the end of the tar archive.
        read_tar_eof(copyin_reader).await?;

        // TODO Does it make sense to overshoot?
        if timeline.get_last_record_lsn() < end_lsn {
            return Err(QueryError::Other(
                anyhow::anyhow!("Cannot import WAL from Lsn {start_lsn} because timeline does not start from the same lsn: {last_record_lsn}"))
            );
        }

        // Flush data to disk, then upload to s3. No need for a forced checkpoint.
        // We only want to persist the data, and it doesn't matter if it's in the
        // shape of deltas or images.
        info!("flushing layers");
        timeline.freeze_and_flush().await?;

        info!("done");
        Ok(())
    }

    /// Helper function to handle the LSN from client request.
    ///
    /// Each GetPage (and Exists and Nblocks) request includes information about
    /// which version of the page is being requested. The client can request the
    /// latest version of the page, or the version that's valid at a particular
    /// LSN. The primary compute node will always request the latest page
    /// version, while a standby will request a version at the LSN that it's
    /// currently caught up to.
    ///
    /// In either case, if the page server hasn't received the WAL up to the
    /// requested LSN yet, we will wait for it to arrive. The return value is
    /// the LSN that should be used to look up the page versions.
    async fn wait_or_get_last_lsn(
        timeline: &Timeline,
        mut lsn: Lsn,
        latest: bool,
        latest_gc_cutoff_lsn: &RcuReadGuard<Lsn>,
        ctx: &RequestContext,
    ) -> Result<Lsn, PageStreamError> {
        if latest {
            // Latest page version was requested. If LSN is given, it is a hint
            // to the page server that there have been no modifications to the
            // page after that LSN. If we haven't received WAL up to that point,
            // wait until it arrives.
            let last_record_lsn = timeline.get_last_record_lsn();

            // Note: this covers the special case that lsn == Lsn(0). That
            // special case means "return the latest version whatever it is",
            // and it's used for bootstrapping purposes, when the page server is
            // connected directly to the compute node. That is needed because
            // when you connect to the compute node, to receive the WAL, the
            // walsender process will do a look up in the pg_authid catalog
            // table for authentication. That poses a deadlock problem: the
            // catalog table lookup will send a GetPage request, but the GetPage
            // request will block in the page server because the recent WAL
            // hasn't been received yet, and it cannot be received until the
            // walsender completes the authentication and starts streaming the
            // WAL.
            if lsn <= last_record_lsn {
                lsn = last_record_lsn;
            } else {
                timeline.wait_lsn(lsn, ctx).await?;
                // Since we waited for 'lsn' to arrive, that is now the last
                // record LSN. (Or close enough for our purposes; the
                // last-record LSN can advance immediately after we return
                // anyway)
            }
        } else {
            if lsn == Lsn(0) {
                return Err(PageStreamError::BadRequest(
                    "invalid LSN(0) in request".into(),
                ));
            }
            timeline.wait_lsn(lsn, ctx).await?;
        }

        if lsn < **latest_gc_cutoff_lsn {
            return Err(PageStreamError::BadRequest(format!(
                "tried to request a page version that was garbage collected. requested at {} gc cutoff {}",
                lsn, **latest_gc_cutoff_lsn
            ).into()));
        }
        Ok(lsn)
    }

    async fn handle_get_rel_exists_request(
        &self,
        timeline: &Timeline,
        req: &PagestreamExistsRequest,
        ctx: &RequestContext,
    ) -> Result<PagestreamBeMessage, PageStreamError> {
        let latest_gc_cutoff_lsn = timeline.get_latest_gc_cutoff_lsn();
        let lsn =
            Self::wait_or_get_last_lsn(timeline, req.lsn, req.latest, &latest_gc_cutoff_lsn, ctx)
                .await?;

        let exists = timeline
            .get_rel_exists(req.rel, Version::Lsn(lsn), req.latest, ctx)
            .await?;

        Ok(PagestreamBeMessage::Exists(PagestreamExistsResponse {
            exists,
        }))
    }

    async fn handle_get_nblocks_request(
        &self,
        timeline: &Timeline,
        req: &PagestreamNblocksRequest,
        ctx: &RequestContext,
    ) -> Result<PagestreamBeMessage, PageStreamError> {
        let latest_gc_cutoff_lsn = timeline.get_latest_gc_cutoff_lsn();
        let lsn =
            Self::wait_or_get_last_lsn(timeline, req.lsn, req.latest, &latest_gc_cutoff_lsn, ctx)
                .await?;

        let n_blocks = timeline
            .get_rel_size(req.rel, Version::Lsn(lsn), req.latest, ctx)
            .await?;

        Ok(PagestreamBeMessage::Nblocks(PagestreamNblocksResponse {
            n_blocks,
        }))
    }

    async fn handle_db_size_request(
        &self,
        timeline: &Timeline,
        req: &PagestreamDbSizeRequest,
        ctx: &RequestContext,
    ) -> Result<PagestreamBeMessage, PageStreamError> {
        let latest_gc_cutoff_lsn = timeline.get_latest_gc_cutoff_lsn();
        let lsn =
            Self::wait_or_get_last_lsn(timeline, req.lsn, req.latest, &latest_gc_cutoff_lsn, ctx)
                .await?;

        let total_blocks = timeline
            .get_db_size(
                DEFAULTTABLESPACE_OID,
                req.dbnode,
                Version::Lsn(lsn),
                req.latest,
                ctx,
            )
            .await?;
        let db_size = total_blocks as i64 * BLCKSZ as i64;

        Ok(PagestreamBeMessage::DbSize(PagestreamDbSizeResponse {
            db_size,
        }))
    }

    async fn do_handle_get_page_at_lsn_request(
        &self,
        timeline: &Timeline,
        req: &PagestreamGetPageRequest,
        ctx: &RequestContext,
    ) -> Result<PagestreamBeMessage, PageStreamError> {
        let latest_gc_cutoff_lsn = timeline.get_latest_gc_cutoff_lsn();
        let lsn =
            Self::wait_or_get_last_lsn(timeline, req.lsn, req.latest, &latest_gc_cutoff_lsn, ctx)
                .await?;
        let page = timeline
            .get_rel_page_at_lsn(req.rel, req.blkno, Version::Lsn(lsn), req.latest, ctx)
            .await?;

        Ok(PagestreamBeMessage::GetPage(PagestreamGetPageResponse {
            page,
        }))
    }

    async fn handle_get_page_at_lsn_request(
        &self,
        timeline: &Timeline,
        req: &PagestreamGetPageRequest,
        ctx: &RequestContext,
    ) -> Result<PagestreamBeMessage, PageStreamError> {
        let key = rel_block_to_key(req.rel, req.blkno);
        if timeline.get_shard_identity().is_key_local(&key) {
            self.do_handle_get_page_at_lsn_request(timeline, req, ctx)
                .await
        } else {
            // The Tenant shard we looked up at connection start does not hold this particular
            // key: look for other shards in this tenant.  This scenario occurs if a pageserver
            // has multiple shards for the same tenant.
            //
            // TODO: optimize this (https://github.com/neondatabase/neon/pull/6037)
            let timeline = match self
                .get_active_tenant_timeline(
                    timeline.tenant_shard_id.tenant_id,
                    timeline.timeline_id,
                    ShardSelector::Page(key),
                )
                .await
            {
                Ok(t) => t,
                Err(GetActiveTimelineError::Tenant(GetActiveTenantError::NotFound(_))) => {
                    // We already know this tenant exists in general, because we resolved it at
                    // start of connection.  Getting a NotFound here indicates that the shard containing
                    // the requested page is not present on this node: the client's knowledge of shard->pageserver
                    // mapping is out of date.
                    tracing::info!("Page request routed to wrong shard: my identity {:?}, should go to shard {}, key {}",
                        timeline.get_shard_identity(), timeline.get_shard_identity().get_shard_number(&key).0, key);
                    // Closing the connection by returning ``::Reconnect` has the side effect of rate-limiting above message, via
                    // client's reconnect backoff, as well as hopefully prompting the client to load its updated configuration
                    // and talk to a different pageserver.
                    return Err(PageStreamError::Reconnect(
                        "getpage@lsn request routed to wrong shard".into(),
                    ));
                }
                Err(e) => return Err(e.into()),
            };

            // Take a GateGuard for the duration of this request.  If we were using our main Timeline object,
            // the GateGuard was already held over the whole connection.
            let _timeline_guard = timeline
                .gate
                .enter()
                .map_err(|_| PageStreamError::Shutdown)?;

            self.do_handle_get_page_at_lsn_request(&timeline, req, ctx)
                .await
        }
    }

    #[allow(clippy::too_many_arguments)]
    #[instrument(skip_all, fields(?lsn, ?prev_lsn, %full_backup))]
    async fn handle_basebackup_request<IO>(
        &mut self,
        pgb: &mut PostgresBackend<IO>,
        tenant_id: TenantId,
        timeline_id: TimelineId,
        lsn: Option<Lsn>,
        prev_lsn: Option<Lsn>,
        full_backup: bool,
        gzip: bool,
        ctx: RequestContext,
    ) -> anyhow::Result<()>
    where
        IO: AsyncRead + AsyncWrite + Send + Sync + Unpin,
    {
        debug_assert_current_span_has_tenant_and_timeline_id();

        let started = std::time::Instant::now();

        // check that the timeline exists
        let timeline = self
            .get_active_tenant_timeline(tenant_id, timeline_id, ShardSelector::Zero)
            .await?;
        let latest_gc_cutoff_lsn = timeline.get_latest_gc_cutoff_lsn();
        if let Some(lsn) = lsn {
            // Backup was requested at a particular LSN. Wait for it to arrive.
            info!("waiting for {}", lsn);
            timeline.wait_lsn(lsn, &ctx).await?;
            timeline
                .check_lsn_is_in_scope(lsn, &latest_gc_cutoff_lsn)
                .context("invalid basebackup lsn")?;
        }

        let lsn_awaited_after = started.elapsed();

        // switch client to COPYOUT
        pgb.write_message_noflush(&BeMessage::CopyOutResponse)?;
        self.flush_cancellable(pgb, &timeline.cancel).await?;

        // Send a tarball of the latest layer on the timeline. Compress if not
        // fullbackup. TODO Compress in that case too (tests need to be updated)
        if full_backup {
            let mut writer = pgb.copyout_writer();
            basebackup::send_basebackup_tarball(
                &mut writer,
                &timeline,
                lsn,
                prev_lsn,
                full_backup,
                &ctx,
            )
            .await?;
        } else {
            let mut writer = pgb.copyout_writer();
            if gzip {
                let mut encoder = GzipEncoder::with_quality(
                    writer,
                    // NOTE using fast compression because it's on the critical path
                    //      for compute startup. For an empty database, we get
                    //      <100KB with this method. The Level::Best compression method
                    //      gives us <20KB, but maybe we should add basebackup caching
                    //      on compute shutdown first.
                    async_compression::Level::Fastest,
                );
                basebackup::send_basebackup_tarball(
                    &mut encoder,
                    &timeline,
                    lsn,
                    prev_lsn,
                    full_backup,
                    &ctx,
                )
                .await?;
                // shutdown the encoder to ensure the gzip footer is written
                encoder.shutdown().await?;
            } else {
                basebackup::send_basebackup_tarball(
                    &mut writer,
                    &timeline,
                    lsn,
                    prev_lsn,
                    full_backup,
                    &ctx,
                )
                .await?;
            }
        }

        pgb.write_message_noflush(&BeMessage::CopyDone)?;
        self.flush_cancellable(pgb, &timeline.cancel).await?;

        let basebackup_after = started
            .elapsed()
            .checked_sub(lsn_awaited_after)
            .unwrap_or(Duration::ZERO);

        info!(
            lsn_await_millis = lsn_awaited_after.as_millis(),
            basebackup_millis = basebackup_after.as_millis(),
            "basebackup complete"
        );

        Ok(())
    }

    // when accessing management api supply None as an argument
    // when using to authorize tenant pass corresponding tenant id
    fn check_permission(&self, tenant_id: Option<TenantId>) -> Result<(), QueryError> {
        if self.auth.is_none() {
            // auth is set to Trust, nothing to check so just return ok
            return Ok(());
        }
        // auth is some, just checked above, when auth is some
        // then claims are always present because of checks during connection init
        // so this expect won't trigger
        let claims = self
            .claims
            .as_ref()
            .expect("claims presence already checked");
        check_permission(claims, tenant_id).map_err(|e| QueryError::Unauthorized(e.0))
    }

    /// Shorthand for getting a reference to a Timeline of an Active tenant.
    async fn get_active_tenant_timeline(
        &self,
        tenant_id: TenantId,
        timeline_id: TimelineId,
        selector: ShardSelector,
    ) -> Result<Arc<Timeline>, GetActiveTimelineError> {
        let tenant = get_active_tenant_with_timeout(
            tenant_id,
            selector,
            ACTIVE_TENANT_TIMEOUT,
            &task_mgr::shutdown_token(),
        )
        .await
        .map_err(GetActiveTimelineError::Tenant)?;
        let timeline = tenant.get_timeline(timeline_id, true)?;
        Ok(timeline)
    }
}

#[async_trait::async_trait]
impl<IO> postgres_backend::Handler<IO> for PageServerHandler
where
    IO: AsyncRead + AsyncWrite + Send + Sync + Unpin,
{
    fn check_auth_jwt(
        &mut self,
        _pgb: &mut PostgresBackend<IO>,
        jwt_response: &[u8],
    ) -> Result<(), QueryError> {
        // this unwrap is never triggered, because check_auth_jwt only called when auth_type is NeonJWT
        // which requires auth to be present
        let data = self
            .auth
            .as_ref()
            .unwrap()
            .decode(str::from_utf8(jwt_response).context("jwt response is not UTF-8")?)
            .map_err(|e| QueryError::Unauthorized(e.0))?;

        if matches!(data.claims.scope, Scope::Tenant) && data.claims.tenant_id.is_none() {
            return Err(QueryError::Unauthorized(
                "jwt token scope is Tenant, but tenant id is missing".into(),
            ));
        }

        debug!(
            "jwt scope check succeeded for scope: {:#?} by tenant id: {:?}",
            data.claims.scope, data.claims.tenant_id,
        );

        self.claims = Some(data.claims);
        Ok(())
    }

    fn startup(
        &mut self,
        _pgb: &mut PostgresBackend<IO>,
        _sm: &FeStartupPacket,
    ) -> Result<(), QueryError> {
        Ok(())
    }

    #[instrument(skip_all, fields(tenant_id, timeline_id))]
    async fn process_query(
        &mut self,
        pgb: &mut PostgresBackend<IO>,
        query_string: &str,
    ) -> Result<(), QueryError> {
        fail::fail_point!("simulated-bad-compute-connection", |_| {
            info!("Hit failpoint for bad connection");
            Err(QueryError::SimulatedConnectionError)
        });

        let ctx = self.connection_ctx.attached_child();
        debug!("process query {query_string:?}");
        if query_string.starts_with("pagestream ") {
            let (_, params_raw) = query_string.split_at("pagestream ".len());
            let params = params_raw.split(' ').collect::<Vec<_>>();
            if params.len() != 2 {
                return Err(QueryError::Other(anyhow::anyhow!(
                    "invalid param number for pagestream command"
                )));
            }
            let tenant_id = TenantId::from_str(params[0])
                .with_context(|| format!("Failed to parse tenant id from {}", params[0]))?;
            let timeline_id = TimelineId::from_str(params[1])
                .with_context(|| format!("Failed to parse timeline id from {}", params[1]))?;

            tracing::Span::current()
                .record("tenant_id", field::display(tenant_id))
                .record("timeline_id", field::display(timeline_id));

            self.check_permission(Some(tenant_id))?;

            self.handle_pagerequests(pgb, tenant_id, timeline_id, ctx)
                .await?;
        } else if query_string.starts_with("basebackup ") {
            let (_, params_raw) = query_string.split_at("basebackup ".len());
            let params = params_raw.split_whitespace().collect::<Vec<_>>();

            if params.len() < 2 {
                return Err(QueryError::Other(anyhow::anyhow!(
                    "invalid param number for basebackup command"
                )));
            }

            let tenant_id = TenantId::from_str(params[0])
                .with_context(|| format!("Failed to parse tenant id from {}", params[0]))?;
            let timeline_id = TimelineId::from_str(params[1])
                .with_context(|| format!("Failed to parse timeline id from {}", params[1]))?;

            tracing::Span::current()
                .record("tenant_id", field::display(tenant_id))
                .record("timeline_id", field::display(timeline_id));

            self.check_permission(Some(tenant_id))?;

            let lsn = if params.len() >= 3 {
                Some(
                    Lsn::from_str(params[2])
                        .with_context(|| format!("Failed to parse Lsn from {}", params[2]))?,
                )
            } else {
                None
            };

            let gzip = if params.len() >= 4 {
                if params[3] == "--gzip" {
                    true
                } else {
                    return Err(QueryError::Other(anyhow::anyhow!(
                        "Parameter in position 3 unknown {}",
                        params[3],
                    )));
                }
            } else {
                false
            };

            ::metrics::metric_vec_duration::observe_async_block_duration_by_result(
                &*metrics::BASEBACKUP_QUERY_TIME,
                async move {
                    self.handle_basebackup_request(
                        pgb,
                        tenant_id,
                        timeline_id,
                        lsn,
                        None,
                        false,
                        gzip,
                        ctx,
                    )
                    .await?;
                    pgb.write_message_noflush(&BeMessage::CommandComplete(b"SELECT 1"))?;
                    anyhow::Ok(())
                },
            )
            .await?;
        }
        // return pair of prev_lsn and last_lsn
        else if query_string.starts_with("get_last_record_rlsn ") {
            let (_, params_raw) = query_string.split_at("get_last_record_rlsn ".len());
            let params = params_raw.split_whitespace().collect::<Vec<_>>();

            if params.len() != 2 {
                return Err(QueryError::Other(anyhow::anyhow!(
                    "invalid param number for get_last_record_rlsn command"
                )));
            }

            let tenant_id = TenantId::from_str(params[0])
                .with_context(|| format!("Failed to parse tenant id from {}", params[0]))?;
            let timeline_id = TimelineId::from_str(params[1])
                .with_context(|| format!("Failed to parse timeline id from {}", params[1]))?;

            tracing::Span::current()
                .record("tenant_id", field::display(tenant_id))
                .record("timeline_id", field::display(timeline_id));

            self.check_permission(Some(tenant_id))?;
            let timeline = self
                .get_active_tenant_timeline(tenant_id, timeline_id, ShardSelector::Zero)
                .await?;

            let end_of_timeline = timeline.get_last_record_rlsn();

            pgb.write_message_noflush(&BeMessage::RowDescription(&[
                RowDescriptor::text_col(b"prev_lsn"),
                RowDescriptor::text_col(b"last_lsn"),
            ]))?
            .write_message_noflush(&BeMessage::DataRow(&[
                Some(end_of_timeline.prev.to_string().as_bytes()),
                Some(end_of_timeline.last.to_string().as_bytes()),
            ]))?
            .write_message_noflush(&BeMessage::CommandComplete(b"SELECT 1"))?;
        }
        // same as basebackup, but result includes relational data as well
        else if query_string.starts_with("fullbackup ") {
            let (_, params_raw) = query_string.split_at("fullbackup ".len());
            let params = params_raw.split_whitespace().collect::<Vec<_>>();

            if params.len() < 2 {
                return Err(QueryError::Other(anyhow::anyhow!(
                    "invalid param number for fullbackup command"
                )));
            }

            let tenant_id = TenantId::from_str(params[0])
                .with_context(|| format!("Failed to parse tenant id from {}", params[0]))?;
            let timeline_id = TimelineId::from_str(params[1])
                .with_context(|| format!("Failed to parse timeline id from {}", params[1]))?;

            tracing::Span::current()
                .record("tenant_id", field::display(tenant_id))
                .record("timeline_id", field::display(timeline_id));

            // The caller is responsible for providing correct lsn and prev_lsn.
            let lsn = if params.len() > 2 {
                Some(
                    Lsn::from_str(params[2])
                        .with_context(|| format!("Failed to parse Lsn from {}", params[2]))?,
                )
            } else {
                None
            };
            let prev_lsn = if params.len() > 3 {
                Some(
                    Lsn::from_str(params[3])
                        .with_context(|| format!("Failed to parse Lsn from {}", params[3]))?,
                )
            } else {
                None
            };

            self.check_permission(Some(tenant_id))?;

            // Check that the timeline exists
            self.handle_basebackup_request(
                pgb,
                tenant_id,
                timeline_id,
                lsn,
                prev_lsn,
                true,
                false,
                ctx,
            )
            .await?;
            pgb.write_message_noflush(&BeMessage::CommandComplete(b"SELECT 1"))?;
        } else if query_string.starts_with("import basebackup ") {
            // Import the `base` section (everything but the wal) of a basebackup.
            // Assumes the tenant already exists on this pageserver.
            //
            // Files are scheduled to be persisted to remote storage, and the
            // caller should poll the http api to check when that is done.
            //
            // Example import command:
            // 1. Get start/end LSN from backup_manifest file
            // 2. Run:
            // cat my_backup/base.tar | psql -h $PAGESERVER \
            //     -c "import basebackup $TENANT $TIMELINE $START_LSN $END_LSN $PG_VERSION"
            let (_, params_raw) = query_string.split_at("import basebackup ".len());
            let params = params_raw.split_whitespace().collect::<Vec<_>>();
            if params.len() != 5 {
                return Err(QueryError::Other(anyhow::anyhow!(
                    "invalid param number for import basebackup command"
                )));
            }
            let tenant_id = TenantId::from_str(params[0])
                .with_context(|| format!("Failed to parse tenant id from {}", params[0]))?;
            let timeline_id = TimelineId::from_str(params[1])
                .with_context(|| format!("Failed to parse timeline id from {}", params[1]))?;
            let base_lsn = Lsn::from_str(params[2])
                .with_context(|| format!("Failed to parse Lsn from {}", params[2]))?;
            let end_lsn = Lsn::from_str(params[3])
                .with_context(|| format!("Failed to parse Lsn from {}", params[3]))?;
            let pg_version = u32::from_str(params[4])
                .with_context(|| format!("Failed to parse pg_version from {}", params[4]))?;

            tracing::Span::current()
                .record("tenant_id", field::display(tenant_id))
                .record("timeline_id", field::display(timeline_id));

            self.check_permission(Some(tenant_id))?;

            match self
                .handle_import_basebackup(
                    pgb,
                    tenant_id,
                    timeline_id,
                    base_lsn,
                    end_lsn,
                    pg_version,
                    ctx,
                )
                .await
            {
                Ok(()) => pgb.write_message_noflush(&BeMessage::CommandComplete(b"SELECT 1"))?,
                Err(e) => {
                    error!("error importing base backup between {base_lsn} and {end_lsn}: {e:?}");
                    pgb.write_message_noflush(&BeMessage::ErrorResponse(
                        &e.to_string(),
                        Some(e.pg_error_code()),
                    ))?
                }
            };
        } else if query_string.starts_with("import wal ") {
            // Import the `pg_wal` section of a basebackup.
            //
            // Files are scheduled to be persisted to remote storage, and the
            // caller should poll the http api to check when that is done.
            let (_, params_raw) = query_string.split_at("import wal ".len());
            let params = params_raw.split_whitespace().collect::<Vec<_>>();
            if params.len() != 4 {
                return Err(QueryError::Other(anyhow::anyhow!(
                    "invalid param number for import wal command"
                )));
            }
            let tenant_id = TenantId::from_str(params[0])
                .with_context(|| format!("Failed to parse tenant id from {}", params[0]))?;
            let timeline_id = TimelineId::from_str(params[1])
                .with_context(|| format!("Failed to parse timeline id from {}", params[1]))?;
            let start_lsn = Lsn::from_str(params[2])
                .with_context(|| format!("Failed to parse Lsn from {}", params[2]))?;
            let end_lsn = Lsn::from_str(params[3])
                .with_context(|| format!("Failed to parse Lsn from {}", params[3]))?;

            tracing::Span::current()
                .record("tenant_id", field::display(tenant_id))
                .record("timeline_id", field::display(timeline_id));

            self.check_permission(Some(tenant_id))?;

            match self
                .handle_import_wal(pgb, tenant_id, timeline_id, start_lsn, end_lsn, ctx)
                .await
            {
                Ok(()) => pgb.write_message_noflush(&BeMessage::CommandComplete(b"SELECT 1"))?,
                Err(e) => {
                    error!("error importing WAL between {start_lsn} and {end_lsn}: {e:?}");
                    pgb.write_message_noflush(&BeMessage::ErrorResponse(
                        &e.to_string(),
                        Some(e.pg_error_code()),
                    ))?
                }
            };
        } else if query_string.to_ascii_lowercase().starts_with("set ") {
            // important because psycopg2 executes "SET datestyle TO 'ISO'"
            // on connect
            pgb.write_message_noflush(&BeMessage::CommandComplete(b"SELECT 1"))?;
        } else if query_string.starts_with("show ") {
            // show <tenant_id>
            let (_, params_raw) = query_string.split_at("show ".len());
            let params = params_raw.split(' ').collect::<Vec<_>>();
            if params.len() != 1 {
                return Err(QueryError::Other(anyhow::anyhow!(
                    "invalid param number for config command"
                )));
            }
            let tenant_id = TenantId::from_str(params[0])
                .with_context(|| format!("Failed to parse tenant id from {}", params[0]))?;

            tracing::Span::current().record("tenant_id", field::display(tenant_id));

            self.check_permission(Some(tenant_id))?;

            let tenant = get_active_tenant_with_timeout(
                tenant_id,
                ShardSelector::Zero,
                ACTIVE_TENANT_TIMEOUT,
                &task_mgr::shutdown_token(),
            )
            .await?;
            pgb.write_message_noflush(&BeMessage::RowDescription(&[
                RowDescriptor::int8_col(b"checkpoint_distance"),
                RowDescriptor::int8_col(b"checkpoint_timeout"),
                RowDescriptor::int8_col(b"compaction_target_size"),
                RowDescriptor::int8_col(b"compaction_period"),
                RowDescriptor::int8_col(b"compaction_threshold"),
                RowDescriptor::int8_col(b"gc_horizon"),
                RowDescriptor::int8_col(b"gc_period"),
                RowDescriptor::int8_col(b"image_creation_threshold"),
                RowDescriptor::int8_col(b"pitr_interval"),
            ]))?
            .write_message_noflush(&BeMessage::DataRow(&[
                Some(tenant.get_checkpoint_distance().to_string().as_bytes()),
                Some(
                    tenant
                        .get_checkpoint_timeout()
                        .as_secs()
                        .to_string()
                        .as_bytes(),
                ),
                Some(tenant.get_compaction_target_size().to_string().as_bytes()),
                Some(
                    tenant
                        .get_compaction_period()
                        .as_secs()
                        .to_string()
                        .as_bytes(),
                ),
                Some(tenant.get_compaction_threshold().to_string().as_bytes()),
                Some(tenant.get_gc_horizon().to_string().as_bytes()),
                Some(tenant.get_gc_period().as_secs().to_string().as_bytes()),
                Some(tenant.get_image_creation_threshold().to_string().as_bytes()),
                Some(tenant.get_pitr_interval().as_secs().to_string().as_bytes()),
            ]))?
            .write_message_noflush(&BeMessage::CommandComplete(b"SELECT 1"))?;
        } else {
            return Err(QueryError::Other(anyhow::anyhow!(
                "unknown command {query_string}"
            )));
        }

        Ok(())
    }
}

impl From<GetActiveTenantError> for QueryError {
    fn from(e: GetActiveTenantError) -> Self {
        match e {
            GetActiveTenantError::WaitForActiveTimeout { .. } => QueryError::Disconnected(
                ConnectionError::Io(io::Error::new(io::ErrorKind::TimedOut, e.to_string())),
            ),
            GetActiveTenantError::Cancelled
            | GetActiveTenantError::WillNotBecomeActive(TenantState::Stopping { .. }) => {
                QueryError::Shutdown
            }
            e => QueryError::Other(anyhow::anyhow!(e)),
        }
    }
}

#[derive(Debug, thiserror::Error)]
enum GetActiveTimelineError {
    #[error(transparent)]
    Tenant(GetActiveTenantError),
    #[error(transparent)]
    Timeline(#[from] GetTimelineError),
}

impl From<GetActiveTimelineError> for QueryError {
    fn from(e: GetActiveTimelineError) -> Self {
        match e {
            GetActiveTimelineError::Tenant(GetActiveTenantError::Cancelled) => QueryError::Shutdown,
            GetActiveTimelineError::Tenant(e) => e.into(),
            GetActiveTimelineError::Timeline(e) => QueryError::NotFound(format!("{e}").into()),
        }
    }
}
