//! The onion service publisher reactor.
//!
//! TODO HSS: write the docs

use std::fmt::Debug;
use std::iter;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime};

use async_trait::async_trait;
use derive_more::{From, Into};
use futures::channel::mpsc::{self, Receiver, Sender};
use futures::task::SpawnExt;
use futures::{select_biased, AsyncRead, AsyncWrite, FutureExt, SinkExt, StreamExt, TryStreamExt};
use postage::sink::SendError;
use postage::{broadcast, watch};
use tor_basic_utils::retry::RetryDelay;
use tor_hscrypto::ope::AesOpeKey;
use tor_hscrypto::RevisionCounter;
use tor_keymgr::KeyMgr;
use tor_llcrypto::pk::ed25519;
use tracing::{debug, error, info, trace, warn};

use tor_circmgr::hspool::{HsCircKind, HsCircPool};
use tor_dirclient::request::HsDescUploadRequest;
use tor_dirclient::{send_request, Error as DirClientError, RequestFailedError};
use tor_error::define_asref_dyn_std_error;
use tor_error::{error_report, internal, into_internal, warn_report};
use tor_hscrypto::pk::{
    HsBlindId, HsBlindIdKey, HsBlindIdKeypair, HsDescSigningKeypair, HsIdKeypair,
};
use tor_hscrypto::time::TimePeriod;
use tor_linkspec::{CircTarget, HasRelayIds, OwnedCircTarget, RelayIds};
use tor_netdir::{NetDir, NetDirProvider, Relay, Timeliness};
use tor_proto::circuit::ClientCirc;
use tor_rtcompat::{Runtime, SleepProviderExt};
use void::Void;

use crate::config::OnionServiceConfig;
use crate::ipt_set::{IptsPublisherUploadView, IptsPublisherView};
use crate::svc::netdir::wait_for_netdir;
use crate::svc::publish::backoff::{BackoffSchedule, RetriableError, Runner};
use crate::svc::publish::descriptor::{build_sign, DescriptorStatus, VersionedDescriptor};
use crate::svc::ShutdownStatus;
use crate::{
    BlindIdKeypairSpecifier, DescSigningKeypairSpecifier, FatalError, HsIdKeypairSpecifier,
    HsNickname,
};

/// The upload rate-limiting threshold.
///
/// Before initiating an upload, the reactor checks if the last upload was at least
/// `UPLOAD_RATE_LIM_THRESHOLD` seconds ago. If so, it uploads the descriptor to all HsDirs that
/// need it. If not, it schedules the upload to happen `UPLOAD_RATE_LIM_THRESHOLD` seconds from the
/// current time.
//
// TODO HSS: this value is probably not right.
const UPLOAD_RATE_LIM_THRESHOLD: Duration = Duration::from_secs(60);

/// The maximum number of concurrent upload tasks per time period.
//
// TODO HSS: this value was arbitrarily chosen and may not be optimal.
//
// The uploads for all TPs happen in parallel.  As a result, the actual limit for the maximum
// number of concurrent upload tasks is multiplied by a number which depends on the TP parameters
// (currently 2, which means the concurrency limit will, in fact, be 32).
//
// We should try to decouple this value from the TP parameters.
const MAX_CONCURRENT_UPLOADS: usize = 16;

/// The maximum time allowed for uploading a descriptor to an HSDirs.
//
// TODO HSS: this value is probably not right.
const UPLOAD_TIMEOUT: Duration = Duration::from_secs(5 * 60);

/// A reactor for the HsDir [`Publisher`](super::Publisher).
///
/// The entrypoint is [`Reactor::run`].
#[must_use = "If you don't call run() on the reactor, it won't publish any descriptors."]
pub(super) struct Reactor<R: Runtime, M: Mockable> {
    /// The immutable, shared inner state.
    imm: Arc<Immutable<R, M>>,
    /// A source for new network directories that we use to determine
    /// our HsDirs.
    dir_provider: Arc<dyn NetDirProvider>,
    /// The mutable inner state,
    inner: Arc<Mutex<Inner>>,
    /// A channel for receiving IPT change notifications.
    ipt_watcher: IptsPublisherView,
    /// A channel for receiving onion service config change notifications.
    config_rx: watch::Receiver<Arc<OnionServiceConfig>>,
    /// A channel for receiving the signal to shut down.
    shutdown_rx: broadcast::Receiver<Void>,
    /// A channel for receiving updates regarding our [`PublishStatus`].
    ///
    /// The main loop of the reactor watches for updates on this channel.
    ///
    /// When the [`PublishStatus`] changes to [`UploadScheduled`](PublishStatus::UploadScheduled),
    /// we can start publishing descriptors.
    ///
    /// If the [`PublishStatus`] is [`AwaitingIpts`](PublishStatus::AwaitingIpts), publishing is
    /// paused until we receive a notification on `ipt_watcher` telling us the IPT manager has
    /// established some introduction points.
    publish_status_rx: watch::Receiver<PublishStatus>,
    /// A sender for updating our [`PublishStatus`].
    ///
    /// When our [`PublishStatus`] changes to [`UploadScheduled`](PublishStatus::UploadScheduled),
    /// we can start publishing descriptors.
    publish_status_tx: watch::Sender<PublishStatus>,
    /// A channel for the telling the upload reminder task (spawned in [`Reactor::run`]) when to
    /// remind us that we need to retry a failed or rate-limited upload.
    ///
    /// The [`Instant`] sent on this channel represents the earliest time when the upload can be
    /// rescheduled. The receiving end of this channel will initially observe `None` (the default
    /// value of the inner type), which indicates there are no pending uploads to reschedule.
    ///
    /// Note: this can't be a non-optional `Instant` because:
    ///   * [`postage::watch`] channels require an inner type that implements `Default`, which
    ///   `Instant` does not implement
    ///   * `Receiver`s are always observe an initial value, even if nothing was sent on the
    ///   channel. Since we don't want to reschedule the upload until we receive a notification
    ///   from the sender, we `None` as a special value that tells the upload reminder task to
    ///   block until it receives a non-default value
    ///
    /// This field is initialized in [`Reactor::run`].
    ///
    // TODO HSS: decide if this is the right approach for implementing rate-limiting
    reattempt_upload_tx: Option<watch::Sender<Option<Instant>>>,
    /// A channel for sending upload completion notifications.
    ///
    /// This channel is polled in the main loop of the reactor.
    upload_task_complete_rx: Receiver<TimePeriodUploadResult>,
    /// A channel for receiving upload completion notifications.
    ///
    /// A copy of this sender is handed to each upload task.
    upload_task_complete_tx: Sender<TimePeriodUploadResult>,
}

/// The immutable, shared state of the descriptor publisher reactor.
#[derive(Clone)]
struct Immutable<R: Runtime, M: Mockable> {
    /// The runtime.
    runtime: R,
    /// Mockable state.
    ///
    /// This is used for launching circuits and for obtaining random number generators.
    mockable: M,
    /// The service for which we're publishing descriptors.
    nickname: HsNickname,
    /// The key manager,
    keymgr: Arc<KeyMgr>,
}

impl<R: Runtime, M: Mockable> Immutable<R, M> {
    /// Create an [`AesOpeKey`] for generating revision counters for the descriptors associated
    /// with the specified [`TimePeriod`].
    ///
    /// If the onion service is not running in offline mode, the key of the returned `AesOpeKey` is
    /// the private part of the blinded identity key. Otherwise, the key is the private part of the
    /// descriptor signing key.
    ///
    /// Returns an error if the service is running in offline mode and the descriptor signing
    /// keypair of the specified `period` is not available.
    //
    // TODO HSS: we don't support "offline" mode (yet), so this always returns an AesOpeKey
    // built from the blinded id key
    fn create_ope_key(&self, period: TimePeriod) -> Result<AesOpeKey, FatalError> {
        let ope_key = match read_blind_id_keypair(&self.keymgr, &self.nickname, period)? {
            Some(key) => {
                let key: ed25519::ExpandedKeypair = key.into();
                key.to_secret_key_bytes()[0..32]
                    .try_into()
                    .expect("Wrong length on slice")
            }
            None => {
                // TODO HSS: we don't support externally provisioned keys (yet), so this branch
                // is unreachable (for now).
                let desc_sign_key_spec =
                    DescSigningKeypairSpecifier::new(self.nickname.clone(), period);
                let key: ed25519::Keypair = self
                    .keymgr
                    .get::<HsDescSigningKeypair>(&desc_sign_key_spec)?
                    // TODO HSS(#1129): internal! is not the right type for this error (we need an
                    // error type for the case where a hidden service running in offline mode has
                    // run out of its pre-previsioned keys). This is somewhat related to #1083
                    // This will be addressed as part of #1129
                    .ok_or_else(|| internal!("identity keys are offline, but descriptor signing key is unavailable?!"))?
                    .into();
                key.to_bytes()
            }
        };

        Ok(AesOpeKey::from_secret(&ope_key))
    }

    /// Generate a revision counter for a descriptor associated with the specified
    /// [`TimePeriod`].
    ///
    /// Returns a revision counter generated according to the [encrypted time in period] scheme.
    ///
    /// [encrypted time in period]: https://spec.torproject.org/rend-spec/revision-counter-mgt.html#encrypted-time
    fn generate_revision_counter(
        &self,
        period: TimePeriod,
        now: SystemTime,
    ) -> Result<RevisionCounter, FatalError> {
        // TODO: in the future, we might want to compute ope_key once per time period (as oppposed
        // to each time we generate a new descriptor), for performance reasons.
        let ope_key = self.create_ope_key(period)?;
        let offset = period
            .offset_within_period(now)
            .ok_or_else(|| match period.range() {
                Ok(std::ops::Range { start, .. }) => {
                    internal!(
                        "current wallclock time not within TP?! (now={:?}, TP_start={:?})",
                        now,
                        start
                    )
                }
                Err(e) => into_internal!("failed to get TimePeriod::range()")(e),
            })?;
        let rev = ope_key.encrypt(offset);

        Ok(RevisionCounter::from(rev))
    }
}

/// Mockable state for the descriptor publisher reactor.
///
/// This enables us to mock parts of the [`Reactor`] for testing purposes.
#[async_trait]
pub(crate) trait Mockable: Clone + Send + Sync + Sized + 'static {
    /// The type of random number generator.
    type Rng: rand::Rng + rand::CryptoRng;

    /// The type of client circuit.
    type ClientCirc: MockableClientCirc;

    /// Return a random number generator.
    fn thread_rng(&self) -> Self::Rng;

    /// Create a circuit of the specified `kind` to `target`.
    async fn get_or_launch_specific<T>(
        &self,
        netdir: &NetDir,
        kind: HsCircKind,
        target: T,
    ) -> Result<Arc<Self::ClientCirc>, tor_circmgr::Error>
    where
        T: CircTarget + Send + Sync;
}

/// Mockable client circuit
#[async_trait]
pub(crate) trait MockableClientCirc: Send + Sync {
    /// The data stream type.
    type DataStream: AsyncRead + AsyncWrite + Send + Unpin;

    /// Start a new stream to the last relay in the circuit, using
    /// a BEGIN_DIR cell.
    async fn begin_dir_stream(self: Arc<Self>) -> Result<Self::DataStream, tor_proto::Error>;
}

#[async_trait]
impl MockableClientCirc for ClientCirc {
    type DataStream = tor_proto::stream::DataStream;

    async fn begin_dir_stream(self: Arc<Self>) -> Result<Self::DataStream, tor_proto::Error> {
        ClientCirc::begin_dir_stream(self).await
    }
}

/// The real version of the mockable state of the reactor.
#[derive(Clone, From, Into)]
pub(crate) struct Real<R: Runtime>(Arc<HsCircPool<R>>);

#[async_trait]
impl<R: Runtime> Mockable for Real<R> {
    type Rng = rand::rngs::ThreadRng;
    type ClientCirc = ClientCirc;

    fn thread_rng(&self) -> Self::Rng {
        rand::thread_rng()
    }

    async fn get_or_launch_specific<T>(
        &self,
        netdir: &NetDir,
        kind: HsCircKind,
        target: T,
    ) -> Result<Arc<ClientCirc>, tor_circmgr::Error>
    where
        T: CircTarget + Send + Sync,
    {
        self.0.get_or_launch_specific(netdir, kind, target).await
    }
}

/// The mutable state of a [`Reactor`].
struct Inner {
    /// The onion service config.
    config: Arc<OnionServiceConfig>,
    /// The relevant time periods.
    ///
    /// This includes the current time period, as well as any other time periods we need to be
    /// publishing descriptors for.
    ///
    /// This is empty until we fetch our first netdir in [`Reactor::run`].
    time_periods: Vec<TimePeriodContext>,
    /// Our most up to date netdir.
    ///
    /// This is initialized in [`Reactor::run`].
    netdir: Option<Arc<NetDir>>,
    /// The timestamp of our last upload.
    ///
    /// This is the time when the last update was _initiated_ (rather than completed), to prevent
    /// the publisher from spawning multiple upload tasks at once in response to multiple external
    /// events happening in quick succession, such as the IPT manager sending multiple IPT change
    /// notifications in a short time frame (#1142), or an IPT change notification that's
    /// immediately followed by a consensus change. Starting two upload tasks at once is not only
    /// inefficient, but it also causes the publisher to generate two different descriptors with
    /// the same revision counter (the revision counter is derived from the current timestamp),
    /// which ultimately causes the slower upload task to fail (see #1142).
    ///
    /// Note: This is only used for deciding when to reschedule a rate-limited upload. It is _not_
    /// used for retrying failed uploads (these are handled internally by
    /// [`Reactor::upload_descriptor_with_retries`]).
    last_uploaded: Option<Instant>,
}

/// The part of the reactor state that changes with every time period.
struct TimePeriodContext {
    /// The time period.
    period: TimePeriod,
    /// The blinded HsId.
    blind_id: HsBlindId,
    /// The HsDirs to use in this time period.
    ///
    // We keep a list of `RelayIds` because we can't store a `Relay<'_>` inside the reactor
    // (the lifetime of a relay is tied to the lifetime of its corresponding `NetDir`. To
    // store `Relay<'_>`s in the reactor, we'd need a way of atomically swapping out both the
    // `NetDir` and the cached relays, and to convince Rust what we're doing is sound)
    hs_dirs: Vec<(RelayIds, DescriptorStatus)>,
    /// The revision counter of the last successful upload, if any.
    last_successful: Option<RevisionCounter>,
}

impl TimePeriodContext {
    /// Create a new `TimePeriodContext`.
    ///
    /// Any of the specified `old_hsdirs` also present in the new list of HsDirs
    /// (returned by `NetDir::hs_dirs_upload`) will have their `DescriptorStatus` preserved.
    fn new<'r>(
        period: TimePeriod,
        blind_id: HsBlindId,
        netdir: &Arc<NetDir>,
        old_hsdirs: impl Iterator<Item = &'r (RelayIds, DescriptorStatus)>,
    ) -> Result<Self, FatalError> {
        Ok(Self {
            period,
            blind_id,
            hs_dirs: Self::compute_hsdirs(period, blind_id, netdir, old_hsdirs)?,
            last_successful: None,
        })
    }

    /// Recompute the HsDirs for this time period.
    fn compute_hsdirs<'r>(
        period: TimePeriod,
        blind_id: HsBlindId,
        netdir: &Arc<NetDir>,
        mut old_hsdirs: impl Iterator<Item = &'r (RelayIds, DescriptorStatus)>,
    ) -> Result<Vec<(RelayIds, DescriptorStatus)>, FatalError> {
        let hs_dirs = netdir.hs_dirs_upload([(blind_id, period)].into_iter())?;

        Ok(hs_dirs
            .map(|(_, hs_dir)| {
                let mut builder = RelayIds::builder();
                if let Some(ed_id) = hs_dir.ed_identity() {
                    builder.ed_identity(*ed_id);
                }

                if let Some(rsa_id) = hs_dir.rsa_identity() {
                    builder.rsa_identity(*rsa_id);
                }

                let relay_id = builder.build().unwrap_or_else(|_| RelayIds::empty());

                // Have we uploaded the descriptor to thiw relay before? If so, we don't need to
                // reupload it unless it was already dirty and due for a reupload.
                let status = match old_hsdirs.find(|(id, _)| *id == relay_id) {
                    Some((_, status)) => *status,
                    None => DescriptorStatus::Dirty,
                };

                (relay_id, status)
            })
            .collect::<Vec<_>>())
    }

    /// Mark the descriptor dirty for all HSDirs of this time period.
    fn mark_all_dirty(&mut self) {
        self.hs_dirs
            .iter_mut()
            .for_each(|(_relay_id, status)| *status = DescriptorStatus::Dirty);
    }
}

/// Authorized client configuration error.
#[derive(Debug, Clone, thiserror::Error)]
#[non_exhaustive]
pub(crate) enum AuthorizedClientConfigError {
    /// A key is malformed if it doesn't start with the "curve25519" prefix,
    /// or if its decoded content is not exactly 32 bytes long.
    #[error("Malformed authorized client key")]
    MalformedKey,

    /// Error while decoding an authorized client's key.
    #[error("Failed base64-decode an authorized client's key")]
    Base64Decode(#[from] base64ct::Error),

    /// Error while accessing the authorized_client key dir.
    #[error("Failed to {action} file {path}")]
    KeyDir {
        /// What we were doing when we encountered the error.
        action: &'static str,
        /// The file that we were trying to access.
        path: std::path::PathBuf,
        /// The underlying I/O error.
        #[source]
        error: Arc<std::io::Error>,
    },

    /// Error while accessing the authorized_client key dir.
    #[error("expected regular file, found directory: {path}")]
    MalformedFile {
        /// The file that we were trying to access.
        path: std::path::PathBuf,
    },
}

/// An error that occurs while trying to upload a descriptor.
#[derive(Clone, Debug, thiserror::Error)]
#[non_exhaustive]
pub(crate) enum UploadError {
    /// An error that has occurred after we have contacted a directory cache and made a circuit to it.
    #[error("descriptor upload request failed")]
    Request(#[from] RequestFailedError),

    /// Failed to establish circuit to hidden service directory
    #[error("circuit failed")]
    Circuit(#[from] tor_circmgr::Error),

    /// Failed to establish stream to hidden service directory
    #[error("stream failed")]
    Stream(#[source] tor_proto::Error),

    /// An internal error.
    #[error("Internal error")]
    Bug(#[from] tor_error::Bug),
}
define_asref_dyn_std_error!(UploadError);

impl<R: Runtime, M: Mockable> Reactor<R, M> {
    /// Create a new `Reactor`.
    #[allow(clippy::too_many_arguments)]
    pub(super) fn new(
        runtime: R,
        nickname: HsNickname,
        dir_provider: Arc<dyn NetDirProvider>,
        mockable: M,
        config: Arc<OnionServiceConfig>,
        ipt_watcher: IptsPublisherView,
        config_rx: watch::Receiver<Arc<OnionServiceConfig>>,
        shutdown_rx: broadcast::Receiver<Void>,
        keymgr: Arc<KeyMgr>,
    ) -> Self {
        /// The maximum size of the upload completion notifier channel.
        ///
        /// The channel we use this for is a futures::mpsc channel, which has a capacity of
        /// `UPLOAD_CHAN_BUF_SIZE + num-senders`. We don't need the buffer size to be non-zero, as
        /// each sender will send exactly one message.
        const UPLOAD_CHAN_BUF_SIZE: usize = 0;

        let (upload_task_complete_tx, upload_task_complete_rx) =
            mpsc::channel(UPLOAD_CHAN_BUF_SIZE);

        let (publish_status_tx, publish_status_rx) = watch::channel();

        let imm = Immutable {
            runtime,
            mockable,
            nickname,
            keymgr,
        };

        let inner = Inner {
            time_periods: vec![],
            config,
            netdir: None,
            last_uploaded: None,
        };

        Self {
            imm: Arc::new(imm),
            inner: Arc::new(Mutex::new(inner)),
            dir_provider,
            ipt_watcher,
            config_rx,
            shutdown_rx,
            publish_status_rx,
            publish_status_tx,
            reattempt_upload_tx: None,
            upload_task_complete_rx,
            upload_task_complete_tx,
        }
    }

    /// Start the reactor.
    ///
    /// Under normal circumstances, this function runs indefinitely.
    ///
    /// Note: this also spawns the "reminder task" that we use to reschedule uploads whenever an
    /// upload fails or is rate-limited.
    pub(super) async fn run(mut self) -> Result<(), FatalError> {
        debug!(nickname=%self.imm.nickname, "starting descriptor publisher reactor");

        {
            let netdir = wait_for_netdir(self.dir_provider.as_ref(), Timeliness::Timely).await?;
            let time_periods = self.compute_time_periods(&netdir, &[])?;

            let mut inner = self.inner.lock().expect("poisoned lock");

            inner.netdir = Some(netdir);
            inner.time_periods = time_periods;
        }

        // There will be at most one pending upload.
        let (reattempt_upload_tx, mut reattempt_upload_rx) = watch::channel();
        let (mut schedule_upload_tx, mut schedule_upload_rx) = watch::channel();

        self.reattempt_upload_tx = Some(reattempt_upload_tx);

        let nickname = self.imm.nickname.clone();
        let rt = self.imm.runtime.clone();
        // Spawn the task that will remind us to retry any rate-limited uploads.
        let _ = self.imm.runtime.spawn(async move {
            // The sender tells us how long to wait until to schedule the upload
            while let Some(scheduled_time) = reattempt_upload_rx.next().await {
                let Some(scheduled_time) = scheduled_time else {
                    // `None` is the initially observed, default value of this postage::watch
                    // channel, and it means there are no pending uploads to reschedule.
                    continue;
                };

                // Check how long we have to sleep until we're no longer rate-limited.
                let duration = scheduled_time.checked_duration_since(rt.now());

                // If duration is `None`, it means we're past `scheduled_time`, so we don't need to
                // sleep at all.
                if let Some(duration) = duration {
                    rt.sleep(duration).await;
                }

                // Enough time has elapsed. Remind the reactor to retry the upload.
                if let Err(e) = schedule_upload_tx.send(()).await {
                    // TODO HSS: update publisher state
                    debug!(nickname=%nickname, "failed to notify reactor to reattempt upload");
                }
            }

            debug!(nickname=%nickname, "reupload task channel closed!");
        });

        loop {
            match self.run_once(&mut schedule_upload_rx).await {
                Ok(ShutdownStatus::Continue) => continue,
                Ok(ShutdownStatus::Terminate) => return Ok(()),
                Err(e) => {
                    error_report!(
                        e,
                        "HS service {}: descriptor publisher crashed!",
                        self.imm.nickname
                    );

                    // TODO HSS: Set status to Shutdown.
                    return Err(e);
                }
            }
        }
    }

    /// Run one iteration of the reactor loop.
    async fn run_once(
        &mut self,
        schedule_upload_rx: &mut watch::Receiver<()>,
    ) -> Result<ShutdownStatus, FatalError> {
        let mut netdir_events = self.dir_provider.events();

        select_biased! {
            // TODO HSS: Stop waiting for the shutdown signal
            // (instead, let the sender of the ipt_watcher being dropped
            // be our shutdown signal)
            //
            // See https://gitlab.torproject.org/tpo/core/arti/-/merge_requests/1812#note_2976757
            shutdown = self.shutdown_rx.next().fuse() => {
                info!(
                    nickname=%self.imm.nickname,
                    "descriptor publisher terminating due to shutdown signal"
                );

                assert!(shutdown.is_none());
                return Ok(ShutdownStatus::Terminate);
            },
            res = self.upload_task_complete_rx.next().fuse() => {
                let Some(upload_res) = res else {
                    return Ok(ShutdownStatus::Terminate);
                };

                self.handle_upload_results(upload_res);
            }
            netidr_event = netdir_events.next().fuse() => {
                // The consensus changed. Grab a new NetDir.
                let netdir = match self.dir_provider.netdir(Timeliness::Timely) {
                    Ok(y) => y,
                    Err(e) => {
                        error_report!(e, "HS service {}: netdir unavailable. Retrying...", self.imm.nickname);
                        // Hopefully a netdir will appear in the future.
                        // in the meantime, suspend operations.
                        //
                        // TODO HSS there is a bug here: we stop reading on our inputs
                        // including eg publish_status_rx, but it is our job to log some of
                        // these things.  While we are waiting for a netdir, all those messages
                        // are "stuck"; they'll appear later, with misleading timestamps.
                        //
                        // Probably this should be fixed by moving the logging
                        // out of the reactor, where it won't be blocked.
                        wait_for_netdir(self.dir_provider.as_ref(), Timeliness::Timely)
                            .await?
                    }
                };
                self.handle_consensus_change(netdir).await?;
            }
            update = self.ipt_watcher.await_update().fuse() => {
                self.handle_ipt_change(update).await?;
            },
            config = self.config_rx.next().fuse() => {
                let Some(config) = config else {
                    return Ok(ShutdownStatus::Terminate);
                };

                self.handle_svc_config_change(config).await?;
            },
            res = schedule_upload_rx.next().fuse() => {
                let Some(()) = res else {
                    return Ok(ShutdownStatus::Terminate);
                };

                // Unless we're waiting for IPTs, reattempt the rate-limited upload in the next
                // iteration.
                self.update_publish_status_unless_waiting(PublishStatus::UploadScheduled).await?;
            },
            should_upload = self.publish_status_rx.next().fuse() => {
                let Some(should_upload) = should_upload else {
                    return Ok(ShutdownStatus::Terminate);
                };

                // Our PublishStatus changed -- are we ready to publish?
                if should_upload == PublishStatus::UploadScheduled {
                    self.update_publish_status_unless_waiting(PublishStatus::Idle).await?;
                    self.upload_all().await?;
                }
            }
        }

        Ok(ShutdownStatus::Continue)
    }

    /// Returns the current status of the publisher
    fn status(&self) -> PublishStatus {
        *self.publish_status_rx.borrow()
    }

    /// Handle a batch of upload outcomes,
    /// possibly updating the status of the descriptor for the corresponding HSDirs.
    fn handle_upload_results(&self, results: TimePeriodUploadResult) {
        let mut inner = self.inner.lock().expect("poisoned lock");

        // Check which time period these uploads pertain to.
        let period = inner
            .time_periods
            .iter_mut()
            .find(|ctx| ctx.period == results.time_period);

        let Some(period) = period else {
            // The uploads were for a time period that is no longer relevant, so we
            // can ignore the result.
            return;
        };

        for upload_res in results.hsdir_result {
            let relay = period
                .hs_dirs
                .iter_mut()
                .find(|(relay_ids, _status)| relay_ids == &upload_res.relay_ids);

            let Some((relay, status)) = relay else {
                // This HSDir went away, so the result doesn't matter.
                return;
            };

            if upload_res.upload_res == UploadStatus::Success {
                let update_last_successful = match period.last_successful {
                    None => true,
                    Some(counter) => counter <= upload_res.revision_counter,
                };

                if update_last_successful {
                    period.last_successful = Some(upload_res.revision_counter);
                    // TODO HSS: Is it possible that this won't update the statuses promptly
                    // enough. For example, it's possible for the reactor to see a Dirty descriptor
                    // and start an upload task for a descriptor has already been uploaded (or is
                    // being uploaded) in another task, but whose upload results have not yet been
                    // processed.
                    //
                    // This is probably made worse by the fact that the statuses are updated in
                    // batches (grouped by time period), rather than one by one as the upload tasks
                    // complete (updating the status involves locking the inner mutex, and I wanted
                    // to minimize the locking/unlocking overheads). I'm not sure handling the
                    // updates in batches was the correct decision here.
                    *status = DescriptorStatus::Clean;
                }
            }

            // TODO HSS: maybe the failed uploads should be rescheduled at some point.
        }
    }

    /// Maybe update our list of HsDirs.
    async fn handle_consensus_change(&mut self, netdir: Arc<NetDir>) -> Result<(), FatalError> {
        trace!("the consensus has changed; recomputing HSDirs");

        let _old: Option<Arc<NetDir>> = self.replace_netdir(netdir);

        self.recompute_hs_dirs()?;
        self.update_publish_status_unless_waiting(PublishStatus::UploadScheduled)
            .await?;

        Ok(())
    }

    /// Recompute the HsDirs for all relevant time periods.
    fn recompute_hs_dirs(&self) -> Result<(), FatalError> {
        let mut inner = self.inner.lock().expect("poisoned lock");
        let inner = &mut *inner;

        let netdir = Arc::clone(
            inner
                .netdir
                .as_ref()
                .ok_or_else(|| internal!("started upload task without a netdir"))?,
        );

        // Update our list of relevant time periods.
        let new_time_periods = self.compute_time_periods(&netdir, &inner.time_periods)?;
        inner.time_periods = new_time_periods;

        Ok(())
    }

    /// Compute the [`TimePeriodContext`]s for the time periods from the specified [`NetDir`].
    ///
    /// The specified `time_periods` are used to preserve the `DescriptorStatus` of the
    /// HsDirs where possible.
    fn compute_time_periods(
        &self,
        netdir: &Arc<NetDir>,
        time_periods: &[TimePeriodContext],
    ) -> Result<Vec<TimePeriodContext>, FatalError> {
        netdir
            .hs_all_time_periods()
            .iter()
            .map(|period| {
                let svc_key_spec = HsIdKeypairSpecifier::new(self.imm.nickname.clone());
                let hsid_kp = self
                    .imm
                    .keymgr
                    .get::<HsIdKeypair>(&svc_key_spec)?
                    .ok_or_else(|| FatalError::MissingHsIdKeypair(self.imm.nickname.clone()))?;
                let svc_key_spec = BlindIdKeypairSpecifier::new(self.imm.nickname.clone(), *period);

                // TODO HSS: make this configurable
                let keystore_selector = Default::default();
                let blind_id_kp = self
                    .imm
                    .keymgr
                    .get_or_generate_with_derived::<HsBlindIdKeypair>(
                        &svc_key_spec,
                        keystore_selector,
                        || {
                            let (_hs_blind_id_key, hs_blind_id_kp, _subcredential) = hsid_kp
                                .compute_blinded_key(*period)
                                .map_err(|_| internal!("failed to compute blinded key"))?;

                            Ok(hs_blind_id_kp)
                        },
                    )?;

                let blind_id: HsBlindIdKey = (&blind_id_kp).into();

                // If our previous `TimePeriodContext`s also had an entry for `period`, we need to
                // preserve the `DescriptorStatus` of its HsDirs. This helps prevent unnecessarily
                // publishing the descriptor to the HsDirs that already have it (the ones that are
                // marked with DescriptorStatus::Clean).
                //
                // In other words, we only want to publish to those HsDirs that
                //   * are part of a new time period (which we have never published the descriptor
                //   for), or
                //   * have just been added to the ring of a time period we already knew about
                if let Some(ctx) = time_periods.iter().find(|ctx| ctx.period == *period) {
                    TimePeriodContext::new(*period, blind_id.into(), netdir, ctx.hs_dirs.iter())
                } else {
                    // Passing an empty iterator here means all HsDirs in this TimePeriodContext
                    // will be marked as dirty, meaning we will need to upload our descriptor to them.
                    TimePeriodContext::new(*period, blind_id.into(), netdir, iter::empty())
                }
            })
            .collect::<Result<Vec<TimePeriodContext>, FatalError>>()
    }

    /// Replace the old netdir with the new, returning the old.
    fn replace_netdir(&self, new_netdir: Arc<NetDir>) -> Option<Arc<NetDir>> {
        self.inner
            .lock()
            .expect("poisoned lock")
            .netdir
            .replace(new_netdir)
    }

    /// Replace our view of the service config with `new_config` if `new_config` contains changes
    /// that would cause us to generate a new descriptor.
    fn replace_config_if_changed(&self, new_config: Arc<OnionServiceConfig>) -> bool {
        let mut inner = self.inner.lock().expect("poisoned lock");
        let old_config = &mut inner.config;

        // The fields we're interested in haven't changed, so there's no need to update
        // `inner.config`.
        //
        // TODO: maybe `Inner` should only contain the fields we're interested in instead of
        // the entire config.
        //
        // Alternatively, a less error-prone solution would be to introduce a separate
        // `DescriptorConfigView` as described in
        // https://gitlab.torproject.org/tpo/core/arti/-/merge_requests/1603#note_2944902

        // TODO HSS: Temporarily disabled while we figure out how we want the client auth config to
        // work; see #1028
        /*
        if old_config.anonymity == new_config.anonymity
            && old_config.encrypt_descriptor == new_config.encrypt_descriptor
        {
            return false;
        }
        */

        let _old: Arc<OnionServiceConfig> = std::mem::replace(old_config, new_config);

        true
    }

    /// Read the intro points from `ipt_watcher`, and decide whether we're ready to start
    /// uploading.
    fn note_ipt_change(&self) -> PublishStatus {
        let inner = self.inner.lock().expect("poisoned lock");

        let mut ipts = self.ipt_watcher.borrow_for_publish();
        match ipts.ipts.as_mut() {
            Some(ipts) => PublishStatus::UploadScheduled,
            None => PublishStatus::AwaitingIpts,
        }
    }

    /// Update our list of introduction points.
    async fn handle_ipt_change(
        &mut self,
        update: Option<Result<(), crate::FatalError>>,
    ) -> Result<(), FatalError> {
        trace!(nickname=%self.imm.nickname, "received IPT change notification from IPT manager");
        match update {
            Some(Ok(())) => {
                let should_upload = self.note_ipt_change();
                debug!(nickname=%self.imm.nickname, "the introduction points have changed");

                self.mark_all_dirty();
                self.update_publish_status(should_upload).await
            }
            Some(Err(e)) => Err(e),
            None => {
                debug!(nickname=%self.imm.nickname, "no IPTs available, ceasing uploads");
                self.update_publish_status(PublishStatus::AwaitingIpts)
                    .await
            }
        }
    }

    /// Update the `PublishStatus` of the reactor with `new_state`,
    /// unless the current state is `AwaitingIpts`.
    async fn update_publish_status_unless_waiting(
        &mut self,
        new_state: PublishStatus,
    ) -> Result<(), FatalError> {
        // Only update the state if we're not waiting for intro points.
        if self.status() != PublishStatus::AwaitingIpts {
            self.update_publish_status(new_state).await?;
        }

        Ok(())
    }

    /// Update the `PublishStatus` of the reactor with `new_state`.
    async fn update_publish_status(&mut self, new_state: PublishStatus) -> Result<(), FatalError> {
        trace!(
            "publisher reactor status change: {:?} -> {:?}",
            self.status(),
            new_state
        );

        self.publish_status_tx
            .send(new_state)
            .await
            .map_err(|_: SendError<_>| internal!("failed to send upload notification?!"))?;

        Ok(())
    }

    /// Use the new keys.
    async fn handle_new_keys(&self) -> Result<(), FatalError> {
        todo!()
    }

    /// Update the descriptors based on the config change.
    async fn handle_svc_config_change(
        &mut self,
        config: Arc<OnionServiceConfig>,
    ) -> Result<(), FatalError> {
        if self.replace_config_if_changed(config) {
            self.mark_all_dirty();

            // Schedule an upload, unless we're still waiting for IPTs.
            self.update_publish_status_unless_waiting(PublishStatus::UploadScheduled)
                .await?;
        }

        Ok(())
    }

    /// Mark the descriptor dirty for all time periods.
    fn mark_all_dirty(&self) {
        trace!("marking the descriptor dirty for all time periods");

        self.inner
            .lock()
            .expect("poisoned lock")
            .time_periods
            .iter_mut()
            .for_each(|tp| tp.mark_all_dirty());
    }

    /// Try to upload our descriptor to the HsDirs that need it.
    ///
    /// If we've recently uploaded some descriptors, we return immediately and schedule the upload
    /// to happen N minutes from now.
    ///
    /// Any failed uploads are retried (TODO HSS: document the retry logic when we implement it, as
    /// well as in what cases this will return an error).
    //
    // TODO HSS: what is N?
    async fn upload_all(&mut self) -> Result<(), FatalError> {
        trace!("starting descriptor upload task...");

        let last_uploaded = self.inner.lock().expect("poisoned lock").last_uploaded;
        let now = self.imm.runtime.now();
        // Check if we should rate-limit this upload.
        if let Some(ts) = last_uploaded {
            let duration_since_upload = now.duration_since(ts);

            if duration_since_upload < UPLOAD_RATE_LIM_THRESHOLD {
                trace!("we are rate-limited; deferring descriptor upload");
                return self
                    .schedule_pending_upload(UPLOAD_RATE_LIM_THRESHOLD)
                    .await;
            }
        }

        let mut inner = self.inner.lock().expect("poisoned lock");
        let inner = &mut *inner;

        let _ = inner.last_uploaded.insert(now);

        for period_ctx in inner.time_periods.iter_mut() {
            let upload_task_complete_tx = self.upload_task_complete_tx.clone();

            // Figure out which HsDirs we need to upload the descriptor to (some of them might already
            // have our latest descriptor, so we filter them out).
            let hs_dirs = period_ctx
                .hs_dirs
                .iter()
                .filter_map(|(relay_id, status)| {
                    if *status == DescriptorStatus::Dirty {
                        Some(relay_id.clone())
                    } else {
                        None
                    }
                })
                .collect::<Vec<_>>();

            if hs_dirs.is_empty() {
                trace!("the descriptor is clean for all HSDirs. Nothing to do");
                return Ok(());
            }

            let time_period = period_ctx.period;

            let worst_case_end = self.imm.runtime.now() + UPLOAD_TIMEOUT;
            // This scope exists because rng is not Send, so it needs to fall out of scope before we
            // await anything.
            let netdir = Arc::clone(
                inner
                    .netdir
                    .as_ref()
                    .ok_or_else(|| internal!("started upload task without a netdir"))?,
            );

            let imm = Arc::clone(&self.imm);
            let ipt_upload_view = self.ipt_watcher.upload_view();
            let config = Arc::clone(&inner.config);

            trace!(nickname=%self.imm.nickname, time_period=?time_period,
                "spawning upload task"
            );

            let _handle: () = self
                .imm
                .runtime
                .spawn(async move {
                    if let Err(e) = Self::upload_for_time_period(
                        hs_dirs,
                        &netdir,
                        config,
                        time_period,
                        Arc::clone(&imm),
                        ipt_upload_view.clone(),
                        upload_task_complete_tx,
                    )
                    .await
                    {
                        error_report!(
                            e,
                            "descriptor upload failed for HS service {} and time period {:?}",
                            imm.nickname,
                            time_period
                        );
                    }
                })
                .map_err(|e| FatalError::from_spawn("upload_for_time_period task", e))?;
        }

        Ok(())
    }

    /// Tell the "upload reminder" task to remind us to retry an upload that failed or was rate-limited.
    async fn schedule_pending_upload(&mut self, delay: Duration) -> Result<(), FatalError> {
        if let Err(e) = self
            .reattempt_upload_tx
            .as_mut()
            .ok_or(internal!(
                "channel not initialized (schedule_pending_upload called before run?!)"
            ))?
            .send(Some(self.imm.runtime.now() + delay))
            .await
        {
            // TODO HSS: return an error
            debug!(nickname=%self.imm.nickname, "failed to schedule upload reattempt");
        }

        Ok(())
    }

    /// Upload the descriptor for the specified time period.
    ///
    /// Any failed uploads are retried (TODO HSS: document the retry logic when we implement it, as
    /// well as in what cases this will return an error).
    async fn upload_for_time_period(
        hs_dirs: Vec<RelayIds>,
        netdir: &Arc<NetDir>,
        config: Arc<OnionServiceConfig>,
        time_period: TimePeriod,
        imm: Arc<Immutable<R, M>>,
        ipt_upload_view: IptsPublisherUploadView,
        mut upload_task_complete_tx: Sender<TimePeriodUploadResult>,
    ) -> Result<(), FatalError> {
        trace!(time_period=?time_period, "uploading descriptor to all HSDirs for this time period");

        let hsdir_count = hs_dirs.len();
        let upload_results = futures::stream::iter(hs_dirs)
            .map(|relay_ids| {
                let netdir = netdir.clone();
                let config = Arc::clone(&config);
                let imm = Arc::clone(&imm);
                let ipt_upload_view = ipt_upload_view.clone();

                let ed_id = relay_ids
                    .rsa_identity()
                    .map(|id| id.to_string())
                    .unwrap_or_else(|| "unknown".into());
                let rsa_id = relay_ids
                    .rsa_identity()
                    .map(|id| id.to_string())
                    .unwrap_or_else(|| "unknown".into());

                async move {
                    let run_upload = |desc| async {
                        let Some(hsdir) = netdir.by_ids(&relay_ids) else {
                            // This should never happen (all of our relay_ids are from the stored
                            // netdir).
                            warn!(
                                nickname=%imm.nickname, hsdir_id=%ed_id, hsdir_rsa_id=%rsa_id,
                                "tried to upload descriptor to relay not found in consensus?!"
                            );
                            return UploadStatus::Failure;
                        };

                        Self::upload_descriptor_with_retries(
                            desc,
                            &netdir,
                            &hsdir,
                            &ed_id,
                            &rsa_id,
                            Arc::clone(&imm),
                        )
                        .await
                    };

                    // How long until we're supposed to time out?
                    let worst_case_end = imm.runtime.now() + UPLOAD_TIMEOUT;
                    // We generate a new descriptor before _each_ HsDir upload. This means each
                    // HsDir could, in theory, receive a different descriptor (not just in terms of
                    // revision-counters, but also with a different set of IPTs). It may seem like
                    // this could lead to some HsDirs being left with an outdated descriptor, but
                    // that's not the case: after the upload completes, the publisher will be
                    // notified by the ipt_watcher of the IPT change event (if there was one to
                    // begin with), which will trigger another upload job.
                    let hsdesc = {
                        // This scope is needed because the ipt_set MutexGuard is not Send, so it
                        // needs to fall out of scope before the await point below
                        let mut ipt_set = ipt_upload_view.borrow_for_publish();

                        // If there are no IPTs, we abort the upload. At this point, we might have
                        // uploaded the descriptor to some, but not all, HSDirs from the specified
                        // time period.
                        //
                        // Returning an error here means the upload completion task is never
                        // notified of the outcome of any of these uploads (which means the
                        // descriptor is not marked clean). This is OK, because if we suddenly find
                        // out we have no IPTs, it means our built `hsdesc` has an outdated set of
                        // IPTs, so we need to go back to the main loop to wait for IPT changes,
                        // and generate a fresh descriptor anyway.
                        //
                        // Ideally, this shouldn't happen very often (if at all).
                        let Some(ipts) = ipt_set.ipts.as_mut() else {
                            // TODO HSS: maybe it's worth defining an separate error type for this.
                            return Err(FatalError::Bug(internal!(
                                "no introduction points; skipping upload"
                            )));
                        };

                        let hsdesc = {
                            trace!(
                                nickname=%imm.nickname, time_period=?time_period,
                                "building descriptor"
                            );
                            let mut rng = imm.mockable.thread_rng();

                            // We're about to generate a new version of the descriptor,
                            // so let's generate a new revision counter.
                            let now = imm.runtime.wallclock();
                            let revision_counter =
                                imm.generate_revision_counter(time_period, now)?;

                            build_sign(
                                &imm.keymgr,
                                &config,
                                ipts,
                                time_period,
                                revision_counter,
                                &mut rng,
                                imm.runtime.wallclock(),
                            )?
                        };

                        if let Err(e) =
                            ipt_set.note_publication_attempt(&imm.runtime, worst_case_end)
                        {
                            let wait = e.log_retry_max(&imm.nickname)?;
                            // TODO HSS retry instead of this
                            return Err(internal!(
                                "ought to retry after {wait:?}, crashing instead"
                            )
                            .into());
                        }

                        hsdesc
                    };

                    let VersionedDescriptor {
                        desc,
                        revision_counter,
                    } = hsdesc;

                    trace!(
                        nickname=%imm.nickname, time_period=?time_period,
                        revision_counter=?revision_counter,
                        "generated new descriptor for time period",
                    );

                    let upload_res = match imm
                        .runtime
                        .timeout(UPLOAD_TIMEOUT, run_upload(desc.clone()))
                        .await
                    {
                        Ok(res) => res,
                        Err(_e) => {
                            warn!(
                                nickname=%imm.nickname, hsdir_id=%ed_id, hsdir_rsa_id=%rsa_id,
                                "descriptor upload timed out",
                            );

                            UploadStatus::Failure
                        }
                    };

                    // TODO HSS: add a mechanism for rescheduling uploads that have
                    // UploadStatus::Failure.
                    //
                    // Note: UploadStatus::Failure is only returned when
                    // upload_descriptor_with_retries fails, i.e. if all our retry
                    // attempts have failed
                    Ok(HsDirUploadStatus {
                        relay_ids,
                        upload_res,
                        revision_counter,
                    })
                }
            })
            // This fails to compile unless the stream is boxed. See https://github.com/rust-lang/rust/issues/104382
            .boxed()
            .buffer_unordered(MAX_CONCURRENT_UPLOADS)
            .try_collect::<Vec<_>>()
            .await?;

        let (succeeded, _failed): (Vec<_>, Vec<_>) = upload_results
            .iter()
            .partition(|res| res.upload_res == UploadStatus::Success);

        debug!(
            nickname=%imm.nickname, time_period=?time_period,
            "descriptor uploaded successfully to {}/{} HSDirs",
            succeeded.len(), hsdir_count
        );

        if let Err(e) = upload_task_complete_tx
            .send(TimePeriodUploadResult {
                time_period,
                hsdir_result: upload_results,
            })
            .await
        {
            return Err(internal!(
                "failed to notify reactor of upload completion (reactor shut down)"
            )
            .into());
        }

        Ok(())
    }

    /// Upload a descriptor to the specified HSDir.
    ///
    /// If an upload fails, this returns an `Err`. This function does not handle retries. It is up
    /// to the caller to retry on failure.
    async fn upload_descriptor(
        hsdesc: String,
        netdir: &Arc<NetDir>,
        hsdir: &Relay<'_>,
        imm: Arc<Immutable<R, M>>,
    ) -> Result<(), UploadError> {
        let request = HsDescUploadRequest::new(hsdesc);

        trace!(nickname=%imm.nickname, hsdir_id=%hsdir.id(), hsdir_rsa_id=%hsdir.rsa_id(),
            "starting descriptor upload",
        );

        let circuit = imm
            .mockable
            .get_or_launch_specific(
                netdir,
                HsCircKind::SvcHsDir,
                OwnedCircTarget::from_circ_target(hsdir),
            )
            .await?;

        let mut stream = circuit
            .begin_dir_stream()
            .await
            .map_err(UploadError::Stream)?;

        let response = send_request(&imm.runtime, &request, &mut stream, None)
            .await
            .map_err(|dir_error| -> UploadError {
                match dir_error {
                    DirClientError::RequestFailed(e) => e.into(),
                    DirClientError::CircMgr(e) => into_internal!(
                        "tor-dirclient complains about circmgr going wrong but we gave it a stream"
                    )(e)
                    .into(),
                    e => into_internal!("unexpected error")(e).into(),
                }
            })?
            .into_output_string()?; // This returns an error if we received an error response

        Ok(())
    }

    /// Upload a descriptor to the specified HSDir, retrying if appropriate.
    ///
    /// TODO HSS: document the retry logic when we implement it.
    async fn upload_descriptor_with_retries(
        hsdesc: String,
        netdir: &Arc<NetDir>,
        hsdir: &Relay<'_>,
        ed_id: &str,
        rsa_id: &str,
        imm: Arc<Immutable<R, M>>,
    ) -> UploadStatus {
        /// The base delay to use for the backoff schedule.
        const BASE_DELAY_MSEC: u32 = 1000;

        let runner = {
            let schedule = PublisherBackoffSchedule {
                retry_delay: RetryDelay::from_msec(BASE_DELAY_MSEC),
                mockable: imm.mockable.clone(),
            };
            Runner::new(
                "upload a hidden service descriptor".into(),
                schedule,
                imm.runtime.clone(),
            )
        };

        let fallible_op = || async {
            Self::upload_descriptor(hsdesc.clone(), netdir, hsdir, Arc::clone(&imm)).await
        };

        match runner.run(fallible_op).await {
            Ok(res) => {
                debug!(
                    nickname=%imm.nickname, hsdir_id=%ed_id, hsdir_rsa_id=%rsa_id,
                    "successfully uploaded descriptor to HSDir",
                );

                UploadStatus::Success
            }
            Err(e) => {
                warn_report!(
                    e,
                    "failed to upload descriptor for service {} (hsdir_id={}, hsdir_rsa_id={})",
                    imm.nickname,
                    ed_id,
                    rsa_id
                );

                UploadStatus::Failure
            }
        }
    }
}

/// Try to read the blinded identity key for a given `TimePeriod`.
///
/// Returns `None` if the service is running in "offline" mode.
///
// TODO HSS: we don't currently have support for "offline" mode so this can never return
// `Ok(None)`.
pub(super) fn read_blind_id_keypair(
    keymgr: &Arc<KeyMgr>,
    nickname: &HsNickname,
    period: TimePeriod,
) -> Result<Option<HsBlindIdKeypair>, FatalError> {
    let svc_key_spec = HsIdKeypairSpecifier::new(nickname.clone());
    let hsid_kp = keymgr
        .get::<HsIdKeypair>(&svc_key_spec)?
        .ok_or_else(|| FatalError::MissingHsIdKeypair(nickname.clone()))?;

    let blind_id_key_spec = BlindIdKeypairSpecifier::new(nickname.clone(), period);

    // TODO: make the keystore selector configurable
    let keystore_selector = Default::default();
    let blind_id_kp = keymgr.get_or_generate_with_derived::<HsBlindIdKeypair>(
        &blind_id_key_spec,
        keystore_selector,
        || {
            let (_hs_blind_id_key, hs_blind_id_kp, _subcredential) = hsid_kp
                .compute_blinded_key(period)
                .map_err(|_| internal!("failed to compute blinded key"))?;

            Ok(hs_blind_id_kp)
        },
    )?;

    Ok(Some(blind_id_kp))
}

/// Whether the reactor should initiate an upload.
#[derive(Copy, Clone, Debug, Default, PartialEq)]
enum PublishStatus {
    /// We need to call upload_all.
    UploadScheduled,
    /// We are idle and waiting for external events.
    ///
    /// We have enough information to build the descriptor, but since we have already called
    /// upload_all to upload it to all relevant HSDirs, there is nothing for us to do right nbow.
    Idle,
    /// We are waiting for the IPT manager to establish some introduction points.
    ///
    /// No descriptors will be published until the `PublishStatus` of the reactor is changed to
    /// `UploadScheduled`.
    #[default]
    AwaitingIpts,
}

/// The backoff schedule for the task that publishes descriptors.
#[derive(Clone, Debug)]
struct PublisherBackoffSchedule<M: Mockable> {
    /// The delays
    retry_delay: RetryDelay,
    /// The mockable reactor state, needed for obtaining an rng.
    mockable: M,
}

impl<M: Mockable> BackoffSchedule for PublisherBackoffSchedule<M> {
    fn max_retries(&self) -> Option<usize> {
        None
    }

    fn timeout(&self) -> Option<Duration> {
        // TODO HSS: pick a less arbitrary timeout
        Some(Duration::from_secs(30))
    }

    fn next_delay<E: RetriableError>(&mut self, _error: &E) -> Option<Duration> {
        Some(self.retry_delay.next_delay(&mut self.mockable.thread_rng()))
    }
}

impl RetriableError for UploadError {
    fn should_retry(&self) -> bool {
        match self {
            UploadError::Request(_) | UploadError::Circuit(_) | UploadError::Stream(_) => true,
            UploadError::Bug(_) => false,
        }
    }
}

/// The outcome of uploading a descriptor to the HSDirs from a particular time period.
#[derive(Debug, Clone)]
struct TimePeriodUploadResult {
    /// The time period.
    time_period: TimePeriod,
    /// The upload results.
    hsdir_result: Vec<HsDirUploadStatus>,
}

/// The outcome of uploading a descriptor to a particular HsDir.
#[derive(Clone, Debug, PartialEq)]
struct HsDirUploadStatus {
    /// The identity of the HsDir we attempted to upload the descriptor to.
    relay_ids: RelayIds,
    /// The outcome of this attempt.
    upload_res: UploadStatus,
    /// The revision counter of the descriptor we tried to upload.
    revision_counter: RevisionCounter,
}

/// The outcome of uploading a descriptor.
//
// TODO: consider making this a type alias for Result<(), ()>
#[derive(Copy, Clone, Debug, PartialEq)]
enum UploadStatus {
    /// The descriptor upload succeeded.
    Success,
    /// The descriptor upload failed.
    Failure,
}

impl<T, E> From<Result<T, E>> for UploadStatus {
    fn from(res: Result<T, E>) -> Self {
        if res.is_ok() {
            Self::Success
        } else {
            Self::Failure
        }
    }
}
