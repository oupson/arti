//! IPT Manager
//!
//! Maintains introduction points and publishes descriptors.
//! Provides a stream of rendezvous requests.
//!
//! See [`IptManager::run_once`] for discussion of the implementation approach.

use std::any::Any;
use std::collections::{HashMap, VecDeque};
use std::fmt::{self, Debug};
use std::hash::Hash;
use std::io;
use std::marker::PhantomData;
use std::panic::AssertUnwindSafe;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use futures::channel::mpsc;
use futures::task::SpawnExt as _;
use futures::{future, select_biased};
use futures::{FutureExt as _, SinkExt as _, StreamExt as _};

use educe::Educe;
use fslock::LockFile;
use itertools::Itertools as _;
use postage::{broadcast, watch};
use rand::Rng;
use serde::{Deserialize, Serialize};
use thiserror::Error;
use tor_keymgr::{KeyMgr, KeySpecifier as _};
use tracing::{debug, error, info, trace, warn};
use void::Void;

use tor_basic_utils::RngExt as _;
use tor_circmgr::hspool::HsCircPool;
use tor_error::{error_report, info_report};
use tor_error::{internal, into_internal, Bug, ErrorKind, HasKind};
use tor_hscrypto::pk::{HsIntroPtSessionIdKeypair, HsSvcNtorKeypair};
use tor_linkspec::{HasRelayIds as _, RelayIds};
use tor_llcrypto::pk::ed25519;
use tor_netdir::NetDirProvider;
use tor_rtcompat::Runtime;

use crate::ipt_set::{self, IptsManagerView, PublishIptSet};
use crate::keys::{IptKeyRole, IptKeySpecifier};
use crate::replay::ReplayLog;
use crate::svc::{ipt_establish, ShutdownStatus};
use crate::timeout_track::{TrackingInstantOffsetNow, TrackingNow, Update as _};
use crate::{FatalError, IptStoreError, StartupError};
use crate::{HsNickname, IptLocalId, OnionServiceConfig, RendRequest};
use ipt_establish::{IptEstablisher, IptParameters, IptStatus, IptStatusStatus, IptWantsToRetire};

use IptStatusStatus as ISS;
use TrackedStatus as TS;

mod persist;
use persist::IptStorageHandle;

/// Expiry time to put on an interim descriptor (IPT publication set Uncertain)
// TODO HSS IPT_PUBLISH_UNCERTAIN configure? get from netdir?
const IPT_PUBLISH_UNCERTAIN: Duration = Duration::from_secs(30 * 60); // 30 mins
/// Expiry time to put on a final descriptor (IPT publication set Certain
// TODO HSS IPT_PUBLISH_CERTAIN configure? get from netdir?
const IPT_PUBLISH_CERTAIN: Duration = Duration::from_secs(12 * 3600); // 12 hours

/// IPT Manager (for one hidden service)
#[derive(Educe)]
#[educe(Debug(bound))]
pub(crate) struct IptManager<R, M> {
    /// Immutable contents
    imm: Immutable<R>,

    /// Mutable state
    state: State<R, M>,
}

/// Immutable contents of an IPT Manager
///
/// Contains things inherent to our identity, and
/// handles to services that we'll be using.
#[derive(Educe)]
#[educe(Debug(bound))]
pub(crate) struct Immutable<R> {
    /// Runtime
    #[educe(Debug(ignore))]
    runtime: R,

    /// Netdir provider
    #[educe(Debug(ignore))]
    dirprovider: Arc<dyn NetDirProvider>,

    /// Nickname
    nick: HsNickname,

    /// Output MPSC for rendezvous requests
    ///
    /// Passed to IPT Establishers we create
    output_rend_reqs: mpsc::Sender<RendRequest>,

    /// Internal channel for updates from IPT Establishers (sender)
    ///
    /// When we make a new `IptEstablisher` we use this arrange for
    /// its status updates to arrive, appropriately tagged, via `status_recv`
    status_send: mpsc::Sender<(IptLocalId, IptStatus)>,

    /// The on-disk state storage handle.
    #[educe(Debug(ignore))]
    storage: Arc<IptStorageHandle>,

    /// The key manager.
    #[educe(Debug(ignore))]
    keymgr: Arc<KeyMgr>,

    /// Replay log directory
    ///
    /// Files are named after the (bare) IptLocalId
    #[educe(Debug(ignore))]
    replay_log_dir: fs_mistrust::CheckedDir,

    /// Lockfile on the replay log directory
    ///
    /// `lock` in `replay_log_dir`.
    ///
    /// **Must have been locked** and this cannot be assured by the type system.
    #[educe(Debug(ignore))]
    replay_log_lock: Arc<LockFile>,
}

/// State of an IPT Manager
#[derive(Educe)]
#[educe(Debug(bound))]
pub(crate) struct State<R, M> {
    /// Source of configuration updates
    //
    // TODO HSS reject reconfigurations we can't cope with
    // for example, state dir changes will go quite wrong
    new_configs: watch::Receiver<Arc<OnionServiceConfig>>,

    /// Last configuration update we received
    ///
    /// This is the snapshot of the config we are currently using.
    /// (Doing it this way avoids running our algorithms
    /// with a mixture of old and new config.)
    current_config: Arc<OnionServiceConfig>,

    /// Channel for updates from IPT Establishers (receiver)
    ///
    /// We arrange for all the updates to be multiplexed,
    /// as that makes handling them easy in our event loop.
    status_recv: mpsc::Receiver<(IptLocalId, IptStatus)>,

    /// State: selected relays
    ///
    /// We append to this, and call `retain` on it,
    /// so these are in chronological order of selection.
    irelays: Vec<IptRelay>,

    /// Did we fail to select a relay last time?
    ///
    /// This can only be caused (or triggered) by a busted netdir or config.
    last_irelay_selection_outcome: Result<(), ()>,

    /// Signal for us to shut down
    shutdown: broadcast::Receiver<Void>,

    /// Mockable state, normally [`Real`]
    ///
    /// This is in `State` so it can be passed mutably to tests,
    /// even though the main code doesn't need `mut`
    /// since `HsCircPool` is a service with interior mutability.
    mockable: M,

    /// Runtime (to placate compiler)
    runtime: PhantomData<R>,
}

/// Mockable state in an IPT Manager - real version
#[derive(Educe)]
#[educe(Debug)]
pub(crate) struct Real<R: Runtime> {
    /// Circuit pool for circuits we need to make
    ///
    /// Passed to the each new Establisher
    #[educe(Debug(ignore))]
    pub(crate) circ_pool: Arc<HsCircPool<R>>,
}

/// One selected relay, at which we are establishing (or relavantly advertised) IPTs
struct IptRelay {
    /// The actual relay
    relay: RelayIds,

    /// The retirement time we selected for this relay
    planned_retirement: Instant,

    /// IPTs at this relay
    ///
    /// At most one will have [`IsCurrent`].
    ///
    /// We append to this, and call `retain` on it,
    /// so these are in chronological order of selection.
    ipts: Vec<Ipt>,
}

/// Type-erased version of `Box<IptEstablisher>`
///
/// The real type is `M::IptEstablisher`.
/// We use `Box<dyn Any>` to avoid propagating the `M` type parameter to `Ipt` etc.
type ErasedIptEstablisher = dyn Any + Send + Sync + 'static;

/// One introduction point, representation in memory
#[derive(Debug)]
struct Ipt {
    /// Local persistent identifier
    lid: IptLocalId,

    /// Handle for the establisher; we keep this here just for its `Drop` action
    establisher: Box<ErasedIptEstablisher>,

    /// `KS_hs_ipt_sid`, `KP_hs_ipt_sid`
    ///
    /// This is an `Arc` because:
    ///  * The manager needs a copy so that it can save it to disk.
    ///  * The establisher needs a copy to actually use.
    ///  * The underlying secret key type is not `Clone`.
    k_sid: Arc<HsIntroPtSessionIdKeypair>,

    /// `KS_hss_ntor`, `KP_hss_ntor`
    k_hss_ntor: Arc<HsSvcNtorKeypair>,

    /// Last information about how it's doing including timing info
    status_last: TrackedStatus,

    /// Until when ought we to try to maintain it
    ///
    /// For introduction points we are publishing,
    /// this is a copy of the value set by the publisher
    /// in the `IptSet` we share with the publisher,
    ///
    /// (`None` means the IPT has not been advertised at all yet.)
    ///
    /// We must duplicate the information because:
    ///
    ///  * We can't have it just live in the shared `IptSet`
    ///    because we need to retain it for no-longer-being published IPTs.
    ///
    ///  * We can't have it just live here because the publisher needs to update it.
    ///
    /// (An alternative would be to more seriously entangle the manager and publisher.)
    last_descriptor_expiry_including_slop: Option<Instant>,

    /// Is this IPT current - should we include it in descriptors ?
    ///
    /// `None` might mean:
    ///  * WantsToRetire
    ///  * We have >N IPTs and we have been using this IPT so long we want to rotate it out
    ///    (the [`IptRelay`] has reached its `planned_retirement` time)
    is_current: Option<IsCurrent>,
}

/// Last information from establisher about an IPT, with timing info added by us
#[derive(Debug)]
enum TrackedStatus {
    /// Corresponds to [`IptStatusStatus::Faulty`]
    Faulty {
        /// When we were first told this started to establish, if we know it
        ///
        /// This might be an early estimate, which would give an overestimate
        /// of the establishment time, which is fine.
        /// Or it might be `Err` meaning we don't know.
        started: Result<Instant, ()>,
    },

    /// Corresponds to [`IptStatusStatus::Establishing`]
    Establishing {
        /// When we were told we started to establish, for calculating `time_to_establish`
        started: Instant,
    },

    /// Corresponds to [`IptStatusStatus::Good`]
    Good {
        /// How long it took to establish (if we could determine that information)
        ///
        /// Can only be `Err` in strange situations.
        time_to_establish: Result<Duration, ()>,

        /// Details, from the Establisher
        details: ipt_establish::GoodIptDetails,
    },
}

/// Token indicating that this introduction point is current (not Retiring)
#[derive(Copy, Clone, Debug, Eq, PartialEq, Hash, Ord, PartialOrd)]
struct IsCurrent;

impl rand::distributions::Distribution<IptLocalId> for rand::distributions::Standard {
    fn sample<R: rand::Rng + ?Sized>(&self, rng: &mut R) -> IptLocalId {
        IptLocalId(rng.gen())
    }
}

impl IptRelay {
    /// Get a reference to this IPT relay's current intro point state (if any)
    ///
    /// `None` means this IPT has no current introduction points.
    /// That might be, briefly, because a new intro point needs to be created;
    /// or it might be because we are retiring the relay.
    fn current_ipt(&self) -> Option<&Ipt> {
        self.ipts
            .iter()
            .find(|ipt| ipt.is_current == Some(IsCurrent))
    }

    /// Get a mutable reference to this IPT relay's current intro point state (if any)
    fn current_ipt_mut(&mut self) -> Option<&mut Ipt> {
        self.ipts
            .iter_mut()
            .find(|ipt| ipt.is_current == Some(IsCurrent))
    }

    /// Should this IPT Relay be retired ?
    ///
    /// This is determined by our IPT relay rotation time.
    fn should_retire(&self, now: &TrackingNow) -> bool {
        now > &self.planned_retirement
    }

    /// Make a new introduction point at this relay
    ///
    /// It becomes the current IPT.
    fn make_new_ipt<R: Runtime, M: Mockable<R>>(
        &mut self,
        imm: &Immutable<R>,
        new_configs: &watch::Receiver<Arc<OnionServiceConfig>>,
        mockable: &mut M,
    ) -> Result<(), CreateIptError> {
        let lid: IptLocalId = mockable.thread_rng().gen();

        let ipt = Ipt::start_establisher(
            imm,
            new_configs,
            mockable,
            &self.relay,
            lid,
            Some(IsCurrent),
            None::<IptExpectExistingKeys>,
            // None is precisely right: the descriptor hasn't been published.
            PromiseLastDescriptorExpiryNoneIsGood {},
        )?;

        self.ipts.push(ipt);

        Ok(())
    }
}

/// Token, representing promise by caller of `start_establisher`
///
/// Caller who makes one of these structs promises that it is OK for `start_establisher`
/// to set `last_descriptor_expiry_including_slop` to `None`.
struct PromiseLastDescriptorExpiryNoneIsGood {}

/// Token telling [`Ipt::start_establisher`] to expect existing keys in the keystore
#[derive(Debug, Clone, Copy)]
struct IptExpectExistingKeys;

impl Ipt {
    /// Start a new IPT establisher, and create and return an `Ipt`
    #[allow(clippy::too_many_arguments)] // There's only two call sites
    fn start_establisher<R: Runtime, M: Mockable<R>>(
        imm: &Immutable<R>,
        new_configs: &watch::Receiver<Arc<OnionServiceConfig>>,
        mockable: &mut M,
        relay: &RelayIds,
        lid: IptLocalId,
        is_current: Option<IsCurrent>,
        expect_existing_keys: Option<IptExpectExistingKeys>,
        _: PromiseLastDescriptorExpiryNoneIsGood,
    ) -> Result<Ipt, CreateIptError> {
        let mut rng = mockable.thread_rng();

        /// Load (from disk) or generate an IPT key with role IptKeyRole::$role
        ///
        /// Ideally this would be a closure, but it has to be generic over the
        /// returned key type.  So it's a macro.  (A proper function would have
        /// many type parameters and arguments and be quite annoying.)
        macro_rules! get_or_gen_key { { $Keypair:ty, $role:ident } => { (||{
            let spec = IptKeySpecifier {
                nick: imm.nick.clone(),
                role: IptKeyRole::$role,
                lid,
            };
            // Our desired behaviour:
            //  expect_existing_keys == None
            //     The keys shouldn't exist.  Generate and insert.
            //     If they do exist then things are badly messed up
            //     (we're creating a new IPT with a fres lid).
            //     So, then, crash.
            //  expect_existing_keys == Some(IptExpectExistingKeys)
            //     The key is supposed to exist.  Load them.
            //     We ought to have stored them before storing in our on-disk records that
            //     this IPT exists.  But this could happen due to file deletion or something.
            //     And we could recover by creating fresh keys, although maybe some clients
            //     would find the previous keys in old descriptors.
            //     So if the keys are missing, make and store new ones, logging an error msg.
            // TODO HSS See #1074: The current keymgr API doesn't make this easy
            // Tidy this code up when the API is better.
            let k: Option<$Keypair> = imm.keymgr.get(&spec)?;
            let arti_path = || {
                spec
                    .arti_path()
                    .map_err(|e| {
                        CreateIptError::Fatal(
                            into_internal!("bad ArtiPath from IPT key spec")(e).into()
                        )
                    })
            };
            match (expect_existing_keys, &k) {
                (None, None) | (Some(_), Some(_)) => {}
                (None, Some(_)) => {
                    return Err(FatalError::IptKeysFoundUnexpectedly(arti_path()?).into())
                },
                (Some(_), None) => {
                    error!("HS service {} missing previous key {:?}, regenerating",
                           &imm.nick, arti_path()?);
                }
            }
            let k = k.map(Ok).unwrap_or_else(|| {
                // TODO HSS get_or_generate is strictly speaking a bit wrong here, see above
                imm.keymgr.get_or_generate(
                    &spec,
                    tor_keymgr::KeystoreSelector::Default,
                    &mut rng,
                )
            })?;
            Ok::<_, CreateIptError>(Arc::new(k))
        })() } }

        let k_hss_ntor = get_or_gen_key!(HsSvcNtorKeypair, KHssNtor)?;
        let k_sid = get_or_gen_key!(HsIntroPtSessionIdKeypair, KSid)?;
        drop(rng);

        // we'll treat it as Establishing until we find otherwise
        let status_last = TS::Establishing {
            started: imm.runtime.now(),
        };

        // TODO HSS: Support ephemeral services (without persistent replay log)
        let replay_log = {
            let replay_log = imm.replay_log_dir.as_path().join(format!("{lid}.bin"));

            ReplayLog::new_logged(&replay_log, imm.replay_log_lock.clone()).map_err(|error| {
                CreateIptError::OpenReplayLog {
                    file: replay_log,
                    error: error.into(),
                }
            })?
        };

        let params = IptParameters {
            replay_log,
            config_rx: new_configs.clone(),
            netdir_provider: imm.dirprovider.clone(),
            introduce_tx: imm.output_rend_reqs.clone(),
            lid,
            target: relay.clone(),
            k_sid: k_sid.clone(),
            k_ntor: Arc::clone(&k_hss_ntor),
            accepting_requests: ipt_establish::RequestDisposition::NotAdvertised,
        };
        let (establisher, mut watch_rx) = mockable.make_new_ipt(imm, params)?;

        imm.runtime
            .spawn({
                let mut status_send = imm.status_send.clone();
                async move {
                    loop {
                        let Some(status) = watch_rx.next().await else {
                            trace!("HS service IPT status task: establisher went away");
                            break;
                        };
                        match status_send.send((lid, status)).await {
                            Ok(()) => {}
                            Err::<_, mpsc::SendError>(e) => {
                                // Not using trace_report because SendError isn't HasKind
                                trace!("HS service IPT status task: manager went away: {e}");
                                break;
                            }
                        }
                    }
                }
            })
            .map_err(|cause| FatalError::Spawn {
                spawning: "IPT establisher watch status task",
                cause: cause.into(),
            })?;

        let ipt = Ipt {
            lid,
            establisher: Box::new(establisher),
            k_hss_ntor,
            k_sid,
            status_last,
            is_current,
            last_descriptor_expiry_including_slop: None,
        };

        debug!(
            "Hs service {}: {lid:?} establishing {} IPT at relay {}",
            &imm.nick,
            match expect_existing_keys {
                None => "new",
                Some(_) => "previous",
            },
            &relay,
        );

        Ok(ipt)
    }

    /// Returns `true` if this IPT has status Good (and should perhaps be published)
    fn is_good(&self) -> bool {
        match self.status_last {
            TS::Good { .. } => true,
            TS::Establishing { .. } | TS::Faulty { .. } => false,
        }
    }

    /// Construct the information needed by the publisher for this intro point
    fn for_publish(&self, details: &ipt_establish::GoodIptDetails) -> Result<ipt_set::Ipt, Bug> {
        let k_sid: &ed25519::Keypair = (*self.k_sid).as_ref();
        tor_netdoc::doc::hsdesc::IntroPointDesc::builder()
            .link_specifiers(details.link_specifiers.clone())
            .ipt_kp_ntor(details.ipt_kp_ntor)
            .kp_hs_ipt_sid(k_sid.verifying_key().into())
            .kp_hss_ntor(self.k_hss_ntor.public().clone())
            .build()
            .map_err(into_internal!("failed to construct IntroPointDesc"))
    }
}

impl<R: Runtime, M: Mockable<R>> IptManager<R, M> {
    /// Create a new IptManager
    #[allow(clippy::unnecessary_wraps)] // TODO HSS remove
    #[allow(clippy::too_many_arguments)] // TODO HSS
    pub(crate) fn new(
        runtime: R,
        dirprovider: Arc<dyn NetDirProvider>,
        nick: HsNickname,
        config: watch::Receiver<Arc<OnionServiceConfig>>,
        output_rend_reqs: mpsc::Sender<RendRequest>,
        shutdown: broadcast::Receiver<Void>,
        storage: impl tor_persist::StateMgr + Send + Sync + 'static,
        mockable: M,
        keymgr: Arc<KeyMgr>,
        state_dir: &Path,
        state_mistrust: &fs_mistrust::Mistrust,
    ) -> Result<Self, StartupError> {
        let irelays = vec![]; // See TODO near persist::load call, in launch_background_tasks

        // We don't need buffering; since this is written to by dedicated tasks which
        // are reading watches.
        let (status_send, status_recv) = mpsc::channel(0);

        let storage = storage.create_handle(format!("hs_ipts_{nick}"));

        let (replay_log_dir, replay_log_lock) = {
            // TODO HSS something should expire these! (and our keys too, obviously)
            let dir = state_dir.join(format!("hss_iptreplay/{nick}"));
            let dir = state_mistrust
                .verifier()
                .make_secure_dir(dir)
                .map_err(StartupError::StateDirectoryInaccessible)?;
            let lock_path = dir.as_path().join("lock");
            let handle_lockfile_io_error = |action| {
                let lock_path = lock_path.clone();
                move |error| {
                    StartupError::StateDirectoryInaccessible(fs_mistrust::Error::Io {
                        action,
                        filename: lock_path,
                        err: Arc::new(error),
                    })
                }
            };
            let mut lock =
                LockFile::open(&lock_path).map_err(handle_lockfile_io_error("opening lockfile"))?;
            // Lockfile::try_lock is a beartrap which returns Result<bool, ..>
            let () = lock
                .try_lock()
                .map_err(handle_lockfile_io_error("locking lockfile"))?
                .then_some(())
                .ok_or_else(|| StartupError::StateLocked)?;

            let lock = Arc::new(lock);

            (dir, lock)
        };

        let imm = Immutable {
            runtime,
            dirprovider,
            nick,
            status_send,
            output_rend_reqs,
            keymgr,
            storage,
            replay_log_dir,
            replay_log_lock,
        };
        let current_config = config.borrow().clone();

        let state = State {
            current_config,
            new_configs: config,
            status_recv,
            mockable,
            shutdown,
            irelays,
            last_irelay_selection_outcome: Ok(()),
            runtime: PhantomData,
        };
        let mgr = IptManager { imm, state };

        Ok(mgr)
    }

    /// Send the IPT manager off to run and establish intro points
    pub(crate) fn launch_background_tasks(
        mut self,
        mut publisher: IptsManagerView,
    ) -> Result<(), StartupError> {
        // TODO maybe this should be done in new(), so we don't have this dummy irelays
        // but then new() would need the IptsManagerView
        assert!(self.state.irelays.is_empty());
        self.state.irelays = persist::load(
            &self.imm,
            &self.state.new_configs,
            &mut self.state.mockable,
            &publisher.borrow_for_read(),
        )?;

        let runtime = self.imm.runtime.clone();
        runtime
            .spawn(self.main_loop_task(publisher))
            .map_err(|cause| StartupError::Spawn {
                spawning: "ipt manager",
                cause: cause.into(),
            })?;
        Ok(())
    }

    /// Iterate over the current IPTs
    ///
    /// Yields each `IptRelay` at most once.
    fn current_ipts(&self) -> impl Iterator<Item = (&IptRelay, &Ipt)> {
        self.state
            .irelays
            .iter()
            .filter_map(|ir| Some((ir, ir.current_ipt()?)))
    }

    /// Iterate over the current IPTs in `Good` state
    fn good_ipts(&self) -> impl Iterator<Item = (&IptRelay, &Ipt)> {
        self.current_ipts().filter(|(_ir, ipt)| ipt.is_good())
    }
}

/// An error that happened while trying to select a relay
///
/// Used only within the IPT manager.
/// Can only be caused by bad netdir or maybe bad config.
#[derive(Debug, Error)]
enum ChooseIptError {
    /// Bad or insufficient netdir
    #[error("bad or insufficient netdir")]
    NetDir(#[from] tor_netdir::Error),
    /// Too few suitable relays
    #[error("too few suitable relays")]
    TooFewUsableRelays,
    /// Time overflow
    #[error("time overflow (system clock set wrong?)")]
    TimeOverflow,
    /// Internal error
    #[error("internal error")]
    Bug(#[from] Bug),
}

impl HasKind for ChooseIptError {
    fn kind(&self) -> ErrorKind {
        use ChooseIptError as E;
        use ErrorKind as EK;
        match self {
            E::NetDir(e) => e.kind(),
            E::TooFewUsableRelays => EK::TorDirectoryUnusable,
            E::TimeOverflow => EK::ClockSkew,
            E::Bug(e) => e.kind(),
        }
    }
}

/// An error that happened while trying to crate an IPT (at a selected relay)
///
/// Used only within the IPT manager.
#[derive(Debug, Error)]
enum CreateIptError {
    /// Fatal error
    #[error("fatal error")]
    Fatal(#[from] FatalError),

    /// Error accessing keystore
    #[error("problems with keystores")]
    Keystore(#[from] tor_keymgr::Error),

    /// Error opening the intro request replay log
    #[error("unable to open the intro req replay log: {file:?}")]
    OpenReplayLog {
        /// What filesystem object we tried to do it to
        file: PathBuf,
        /// What happened
        #[source]
        error: Arc<io::Error>,
    },
}

impl<R: Runtime, M: Mockable<R>> State<R, M> {
    /// Find the `Ipt` with persistent local id `lid`
    fn ipt_by_lid_mut(&mut self, needle: IptLocalId) -> Option<&mut Ipt> {
        self.irelays
            .iter_mut()
            .find_map(|ir| ir.ipts.iter_mut().find(|ipt| ipt.lid == needle))
    }

    /// Choose a new relay to use for IPTs
    fn choose_new_ipt_relay(
        &mut self,
        imm: &Immutable<R>,
        now: Instant,
    ) -> Result<(), ChooseIptError> {
        let netdir = imm.dirprovider.timely_netdir()?;

        let mut rng = self.mockable.thread_rng();

        let relay = netdir
            .pick_relay(
                &mut rng,
                tor_netdir::WeightRole::HsIntro,
                // TODO HSS should we apply any other conditions to the selected IPT?
                |new| {
                    new.is_hs_intro_point()
                        && !self
                            .irelays
                            .iter()
                            .any(|existing| new.has_any_relay_id_from(&existing.relay))
                },
            )
            .ok_or(ChooseIptError::TooFewUsableRelays)?;

        let retirement = rng
            .gen_range_checked(self.current_config.ipt_relay_rotation_time())
            .ok_or_else(|| internal!("IPT_RELAY_ROTATION_TIME range was empty!"))?;
        let retirement = now
            .checked_add(retirement)
            .ok_or(ChooseIptError::TimeOverflow)?;

        let new_irelay = IptRelay {
            relay: RelayIds::from_relay_ids(&relay),
            planned_retirement: retirement,
            ipts: vec![],
        };
        self.irelays.push(new_irelay);

        debug!(
            "HS service {}: choosing new IPT relay {}",
            &imm.nick,
            relay.display_relay_ids()
        );

        Ok(())
    }

    /// Update `self`'s status tracking for one introduction point
    fn handle_ipt_status_update(&mut self, imm: &Immutable<R>, lid: IptLocalId, update: IptStatus) {
        let Some(ipt) = self.ipt_by_lid_mut(lid) else {
            // update from now-withdrawn IPT, ignore it (can happen due to the IPT being a task)
            return;
        };

        debug!("HS service {}: {lid:?} status update {update:?}", &imm.nick);

        let IptStatus {
            status: update,
            wants_to_retire,
            n_faults: _,
        } = update;

        #[allow(clippy::single_match)] // want to be explicit about the Ok type
        match wants_to_retire {
            Err(IptWantsToRetire) => ipt.is_current = None,
            Ok(()) => {}
        }

        let now = || imm.runtime.now();

        let started = match &ipt.status_last {
            TS::Establishing { started, .. } => Ok(*started),
            TS::Faulty { started, .. } => *started,
            TS::Good { .. } => Err(()),
        };

        ipt.status_last = match update {
            ISS::Establishing => TS::Establishing {
                started: started.unwrap_or_else(|()| now()),
            },
            ISS::Good(details) => {
                let time_to_establish = started.and_then(|started| {
                    // return () at end of ok_or_else closure, for clarity
                    #[allow(clippy::unused_unit, clippy::semicolon_if_nothing_returned)]
                    now().checked_duration_since(started).ok_or_else(|| {
                        warn!("monotonic clock went backwards! (HS IPT)");
                        ()
                    })
                });
                TS::Good {
                    time_to_establish,
                    details,
                }
            }
            ISS::Faulty => TS::Faulty { started },
        };
    }
}

// TODO HSS: Combine this block with the other impl IptManager<R, M>
// We probably want to make sure this whole file is in a sensible order.
impl<R: Runtime, M: Mockable<R>> IptManager<R, M> {
    /// Make some progress, if possible, and say when to wake up again
    ///
    /// Examines the current state and attempts to improve it.
    ///
    /// If `idempotently_progress_things_now` makes any changes,
    /// it will return `None`.
    /// It should then be called again immediately.
    ///
    /// Otherwise, it returns the time in the future when further work ought to be done:
    /// i.e., the time of the earliest timeout or planned future state change -
    /// as a [`TrackingNow`].
    ///
    /// In that case, the caller must call `compute_iptsetstatus_publish`,
    /// since the IPT set etc. may have changed.
    ///
    /// ### Goals and algorithms
    ///
    /// We attempt to maintain a pool of N established and verified IPTs,
    /// at N IPT Relays.
    ///
    /// When we have fewer than N IPT Relays
    /// that have `Establishing` or `Good` IPTs (see below)
    /// and fewer than k*N IPT Relays overall,
    /// we choose a new IPT Relay at random from the consensus
    /// and try to establish an IPT on it.
    ///
    /// (Rationale for the k*N limit:
    /// we do want to try to replace faulty IPTs, but
    /// we don't want an attacker to be able to provoke us into
    /// rapidly churning through IPT candidates.)
    ///
    /// When we select a new IPT Relay, we randomly choose a planned replacement time,
    /// after which it becomes `Retiring`.
    ///
    /// Additionally, any IPT becomes `Retiring`
    /// after it has been used for a certain number of introductions
    /// (c.f. C Tor `#define INTRO_POINT_MIN_LIFETIME_INTRODUCTIONS 16384`.)
    /// When this happens we retain the IPT Relay,
    /// and make new parameters to make a new IPT at the same Relay.
    ///
    /// An IPT is removed from our records, and we give up on it,
    /// when it is no longer `Good` or `Establishing`
    /// and all descriptors that mentioned it have expired.
    ///
    /// (Until all published descriptors mentioning an IPT expire,
    /// we consider ourselves bound by those previously-published descriptors,
    /// and try to maintain the IPT.
    /// TODO: Allegedly this is unnecessary, but I don't see how it could be.)
    ///
    /// ### Performance
    ///
    /// This function is at worst O(N) where N is the number of IPTs.
    /// When handling state changes relating to a particular IPT (or IPT relay)
    /// it needs at most O(1) calls to progress that one IPT to its proper new state.
    ///
    /// See the performance note on [`run_once()`](Self::run_once).
    fn idempotently_progress_things_now(&mut self) -> Result<Option<TrackingNow>, FatalError> {
        /// Return value which means "we changed something, please run me again"
        ///
        /// In each case, if we make any changes which indicate we might
        /// want to restart, , we `return CONTINUE`, and
        /// our caller will just call us again.
        ///
        /// This approach simplifies the logic: everything here is idempotent.
        /// (It does mean the algorithm can be quadratic in the number of intro points,
        /// but that number is reasonably small for a modern computer and the constant
        /// factor is small too.)
        const CONTINUE: Result<Option<TrackingNow>, FatalError> = Ok(None);

        // This tracks everything we compare it to, using interior mutability,
        // so that if there is no work to do and no timeouts have expired,
        // we know when we will want to wake up.
        let now = TrackingNow::now(&self.imm.runtime);

        // ---------- collect garbage ----------

        // Rotate out an old IPT if we have >N good IPTs
        if self.good_ipts().count() >= self.target_n_intro_points() {
            for ir in &mut self.state.irelays {
                if ir.should_retire(&now) {
                    if let Some(ipt) = ir.current_ipt_mut() {
                        ipt.is_current = None;
                        return CONTINUE;
                    }
                }
            }
        }

        // Forget old IPTs (after the last descriptor mentioning them has expired)
        for ir in &mut self.state.irelays {
            // When we drop the Ipt we drop the IptEstablisher, withdrawing the intro point
            ir.ipts.retain(|ipt| {
                ipt.is_current.is_some()
                    || match ipt.last_descriptor_expiry_including_slop {
                        None => false,
                        Some(last) => now < last,
                    }
            });
            // No need to return CONTINUE, since there is no other future work implied
            // by discarding a non-current IPT.
        }

        // Forget retired IPT relays (all their IPTs are gone)
        self.state
            .irelays
            .retain(|ir| !(ir.should_retire(&now) && ir.ipts.is_empty()));
        // If we deleted relays, we might want to select new ones.  That happens below.

        // ---------- make progress ----------
        //
        // Consider selecting new relays and setting up new IPTs.

        // Create new IPTs at already-chosen relays
        for ir in &mut self.state.irelays {
            if !ir.should_retire(&now) && ir.current_ipt_mut().is_none() {
                // We don't have a current IPT at this relay, but we should.
                match ir.make_new_ipt(&self.imm, &self.state.new_configs, &mut self.state.mockable)
                {
                    Ok(()) => return CONTINUE,
                    Err(CreateIptError::Fatal(fatal)) => return Err(fatal),
                    Err(
                        e @ (CreateIptError::Keystore(_) | CreateIptError::OpenReplayLog { .. }),
                    ) => {
                        error_report!(e, "HS {}: failed to prepare new IPT", &self.imm.nick);
                        // Let's not try any more of this.
                        // We'll run the rest of our "make progress" algorithms,
                        // presenting them with possibly-suboptimal state.  That's fine.
                        // At some point we'll be poked to run again and then we'll retry.
                        /// Retry no later than this:
                        const STORAGE_RETRY: Duration = Duration::from_secs(60);
                        now.update(STORAGE_RETRY);
                        break;
                    }
                }
            }
        }

        // Consider choosing a new IPT relay
        {
            // block {} prevents use of `n_good_ish_relays` for other (wrong) purposes

            // We optimistically count an Establishing IPT as good-ish;
            // specifically, for the purposes of deciding whether to select a new
            // relay because we don't have enough good-looking ones.
            let n_good_ish_relays = self
                .current_ipts()
                .filter(|(_ir, ipt)| match ipt.status_last {
                    TS::Good { .. } | TS::Establishing { .. } => true,
                    TS::Faulty { .. } => false,
                })
                .count();

            #[allow(clippy::unused_unit, clippy::semicolon_if_nothing_returned)] // in map_err
            if n_good_ish_relays < self.target_n_intro_points()
                && self.state.irelays.len() < self.max_n_intro_relays()
                && self.state.last_irelay_selection_outcome.is_ok()
            {
                self.state.last_irelay_selection_outcome = self
                    .state
                    .choose_new_ipt_relay(&self.imm, now.instant().get_now_untracked())
                    .map_err(|error| {
                        /// Call $report! with the message.
                        // The macros are annoying and want a cost argument.
                        macro_rules! report { { $report:ident } => {
                            $report!(
                                error,
                                "HS service {} failed to select IPT relay",
                                &self.imm.nick,
                            )
                        }}
                        use ChooseIptError as E;
                        match &error {
                            E::NetDir(_) => report!(info_report),
                            _ => report!(error_report),
                        };
                        ()
                    });
                return CONTINUE;
            }
        }

        //---------- caller (run_once) will update publisher, and wait ----------

        Ok(Some(now))
    }

    /// Import publisher's updates to latest descriptor expiry times
    ///
    /// Copies the `last_descriptor_expiry_including_slop` field
    /// from each ipt in `publish_set` to the corresponding ipt in `self`.
    ///
    /// ### Performance
    ///
    /// This function is at worst O(N) where N is the number of IPTs.
    /// See the performance note on [`run_once()`](Self::run_once).
    fn import_new_expiry_times(irelays: &mut [IptRelay], publish_set: &PublishIptSet) {
        // Every entry in the PublishIptSet ought to correspond to an ipt in self.
        //
        // If there are IPTs in publish_set.last_descriptor_expiry_including_slop
        // that aren't in self, those are IPTs that we know were published,
        // but can't establish since we have forgotten their details.
        //
        // We are not supposed to allow that to happen:
        // we save IPTs to disk before we allow them to be published.
        //
        // (This invariant is across two data structures:
        // `ipt_mgr::State` (specifically, `Ipt`) which is modified only here,
        // and `ipt_set::PublishIptSet` which is shared with the publisher.
        // See the comments in PublishIptSet.)
        //
        // TODO HSS-IPT-PERSIST well, actually we don't save anything at all, but we will do.

        let all_ours = irelays.iter_mut().flat_map(|ir| ir.ipts.iter_mut());

        for ours in all_ours {
            if let Some(theirs) = publish_set
                .last_descriptor_expiry_including_slop
                .get(&ours.lid)
            {
                ours.last_descriptor_expiry_including_slop = Some(*theirs);
            }
        }
    }

    /// Expire old entries in publish_set.last_descriptor_expiry_including_slop
    ///
    /// Deletes entries where `now` > `last_descriptor_expiry_including_slop`,
    /// ie, entries where the publication's validity time has expired,
    /// meaning we don't need to maintain that IPT any more,
    /// at least, not just because we've published it.
    ///
    /// We may expire even entries for IPTs that we, the manager, still want to maintain.
    /// That's fine: this is (just) the information about what we have previously published.
    ///
    /// ### Performance
    ///
    /// This function is at worst O(N) where N is the number of IPTs.
    /// See the performance note on [`run_once()`](Self::run_once).
    fn expire_old_expiry_times(&self, publish_set: &mut PublishIptSet, now: &TrackingNow) {
        // We don't want to bother waking up just to expire things,
        // so use an untracked comparison.
        let now = now.instant().get_now_untracked();

        publish_set
            .last_descriptor_expiry_including_slop
            .retain(|_lid, expiry| *expiry <= now);
    }

    /// Compute the IPT set to publish, and update the data shared with the publisher
    ///
    /// `now` is current time and also the earliest wakeup,
    /// which we are in the process of planning.
    /// The noted earliest wakeup can be updated by this function,
    /// for example, with a future time at which the IPT set ought to be published
    /// (eg, the status goes from Unknown to Uncertain).
    ///
    /// ## IPT sets and lifetimes
    ///
    /// We remember every IPT we have published that is still valid.
    ///
    /// At each point in time we have an idea of set of IPTs we want to publish.
    /// The possibilities are:
    ///
    ///  * `Certain`:
    ///    We are sure of which IPTs we want to publish.
    ///    We try to do so, talking to hsdirs as necessary,
    ///    updating any existing information.
    ///    (We also republish to an hsdir if its descriptor will expire soon,
    ///    or we haven't published there since Arti was restarted.)
    ///
    ///  * `Unknown`:
    ///    We have no idea which IPTs to publish.
    ///    We leave whatever is on the hsdirs as-is.
    ///
    ///  * `Uncertain`:
    ///    We have some IPTs we could publish,
    ///    but we're not confident about them.
    ///    We publish these to a particular hsdir if:
    ///     - our last-published descriptor has expired
    ///     - or it will expire soon
    ///     - or if we haven't published since Arti was restarted.
    ///
    /// The idea of what to publish is calculated as follows:
    ///
    ///  * If we have at least N `Good` IPTs: `Certain`.
    ///    (We publish the "best" N IPTs for some definition of "best".
    ///    TODO: should we use the fault count?  recency?)
    ///
    ///  * Unless we have at least one `Good` IPT: `Unknown`.
    ///
    ///  * Otherwise: if there are IPTs in `Establishing`,
    ///    and they have been in `Establishing` only a short time \[1\]:
    ///    `Unknown`; otherwise `Uncertain`.
    ///
    /// The effect is that we delay publishing an initial descriptor
    /// by at most 1x the fastest IPT setup time,
    /// at most doubling the initial setup time.
    ///
    /// Each update to the IPT set that isn't `Unknown` comes with a
    /// proposed descriptor expiry time,
    /// which is used if the descriptor is to be actually published.
    /// The proposed descriptor lifetime for `Uncertain`
    /// is the minimum (30 minutes).
    /// Otherwise, we double the lifetime each time,
    /// unless any IPT in the previous descriptor was declared `Faulty`,
    /// in which case we reset it back to the minimum.
    /// TODO: Perhaps we should just pick fixed short and long lifetimes instead,
    /// to limit distinguishability.
    ///
    /// (Rationale: if IPTs are regularly misbehaving,
    /// we should be cautious and limit our exposure to the damage.)
    ///
    /// \[1\] NOTE: We wait a "short time" between establishing our first IPT,
    /// and publishing an incomplete (<N) descriptor -
    /// this is a compromise between
    /// availability (publishing as soon as we have any working IPT)
    /// and
    /// exposure and hsdir load
    /// (which would suggest publishing only when our IPT set is stable).
    /// One possible strategy is to wait as long again
    /// as the time it took to establish our first IPT.
    /// Another is to somehow use our circuit timing estimator.
    ///
    /// ### Performance
    ///
    /// This function is at worst O(N) where N is the number of IPTs.
    /// See the performance note on [`run_once()`](Self::run_once).
    #[allow(clippy::unnecessary_wraps)] // for regularity
    #[allow(clippy::cognitive_complexity)] // TODO HSS consider whether to split this up somehow
    fn compute_iptsetstatus_publish(
        &mut self,
        now: &TrackingNow,
        publish_set: &mut PublishIptSet,
    ) -> Result<(), IptStoreError> {
        //---------- tell the publisher what to announce ----------

        let very_recently: Option<(TrackingInstantOffsetNow, Duration)> = (|| {
            // on time overflow, don't treat any as started establishing very recently

            let fastest_good_establish_time = self
                .current_ipts()
                .filter_map(|(_ir, ipt)| match ipt.status_last {
                    TS::Good {
                        time_to_establish, ..
                    } => Some(time_to_establish.ok()?),
                    TS::Establishing { .. } | TS::Faulty { .. } => None,
                })
                .min()?;

            // TODO HSS is this the right guess for IPT establishment?
            // we could use circuit timings etc., but arguably the actual time to establish
            // our fastest IPT is a better estimator here (and we want an optimistic,
            // rather than pessimistic estimate).
            //
            // TODO HSS fastest_good_establish_time factor 1 should be tuneable
            let wait_more = fastest_good_establish_time;
            let very_recently = fastest_good_establish_time.checked_add(wait_more)?;

            let very_recently = now.checked_sub(very_recently)?;
            Some((very_recently, wait_more))
        })();

        let started_establishing_very_recently = || {
            let (very_recently, wait_more) = very_recently?;
            let lid = self
                .current_ipts()
                .filter_map(|(_ir, ipt)| {
                    let started = match ipt.status_last {
                        TS::Establishing { started } => Some(started),
                        TS::Good { .. } | TS::Faulty { .. } => None,
                    }?;

                    (started > very_recently).then_some(ipt.lid)
                })
                .next()?;
            Some((lid, wait_more))
        };

        let n_good_ipts = self.good_ipts().count();
        let publish_lifetime = if n_good_ipts >= self.target_n_intro_points() {
            // "Certain" - we are sure of which IPTs we want to publish
            debug!(
                "HS service {}: {} good IPTs, >= target {}, publishing",
                &self.imm.nick,
                n_good_ipts,
                self.target_n_intro_points()
            );
            Some(IPT_PUBLISH_CERTAIN)
        } else if self.good_ipts().next().is_none()
        /* !... .is_empty() */
        {
            // "Unknown" - we have no idea which IPTs to publish.
            debug!("HS service {}: no good IPTs", &self.imm.nick);
            None
        } else if let Some((wait_for, wait_more)) = started_establishing_very_recently() {
            // "Unknown" - we say have no idea which IPTs to publish:
            // although we have *some* idea, we hold off a bit to see if things improve.
            // The wait_more period started counting when the fastest IPT became ready,
            // so the printed value isn't an offset from the message timestamp.
            debug!(
                "HS service {}: {} good IPTs, < target {}, waiting up to {}ms for {:?}",
                &self.imm.nick,
                n_good_ipts,
                self.target_n_intro_points(),
                wait_more.as_millis(),
                wait_for
            );
            None
        } else {
            // "Uncertain" - we have some IPTs we could publish, but we're not confident
            debug!(
                "HS service {}: {} good IPTs, < target {}, publishing what we have",
                &self.imm.nick,
                n_good_ipts,
                self.target_n_intro_points()
            );
            Some(IPT_PUBLISH_UNCERTAIN)
        };

        publish_set.ipts = if let Some(lifetime) = publish_lifetime {
            let selected = self.publish_set_select();
            for ipt in &selected {
                self.state.mockable.start_accepting(&*ipt.establisher);
            }
            Some(Self::make_publish_set(selected, lifetime)?)
        } else {
            None
        };

        //---------- store persistent state ----------

        persist::store(&self.imm, &self.state)?;

        Ok(())
    }

    /// Select IPTs to publish, given that we have decided to publish *something*
    ///
    /// Calculates set of ipts to publish, selecting up to the target `N`
    /// from the available good current IPTs.
    /// (Old, non-current IPTs, that we are trying to retire, are never published.)
    ///
    /// The returned list is in the same order as our data structure:
    /// firstly, by the ordering in `State.irelays`, and then within each relay,
    /// by the ordering in `IptRelay.ipts`.  Both of these are stable.
    ///
    /// ### Performance
    ///
    /// This function is at worst O(N) where N is the number of IPTs.
    /// See the performance note on [`run_once()`](Self::run_once).
    fn publish_set_select(&self) -> VecDeque<&Ipt> {
        /// Good candidate introduction point for publication
        type Candidate<'i> = &'i Ipt;

        let target_n = self.target_n_intro_points();

        let mut candidates: VecDeque<_> = self
            .state
            .irelays
            .iter()
            .filter_map(|ir: &_| -> Option<Candidate<'_>> {
                let current_ipt = ir.current_ipt()?;
                if !current_ipt.is_good() {
                    return None;
                }
                Some(current_ipt)
            })
            .collect();

        // Take the last N good IPT relays
        //
        // The way we manage irelays means that this is always
        // the ones we selected most recently.
        //
        // TODO SPEC  Publication strategy when we have more than >N IPTs
        //
        // We could have a number of strategies here.  We could take some timing
        // measurements, or use the establishment time, or something; but we don't
        // want to add distinguishability.
        //
        // Another concern is manipulability, but
        // We can't be forced to churn because we don't remove relays
        // from our list of relays to try to use, other than on our own schedule.
        // But we probably won't want to be too reactive to the network environment.
        //
        // Since we only choose new relays when old ones are to retire, or are faulty,
        // choosing the most recently selected, rather than the least recently,
        // has the effect of preferring relays we don't know to be faulty,
        // to ones we have considered faulty least once.
        //
        // That's better than the opposite.  Also, choosing more recently selected relays
        // for publication may slightly bring forward the time at which all descriptors
        // mentioning that relay have expired, and then we can forget about it.
        while candidates.len() > target_n {
            // WTB: VecDeque::truncate_front
            let _: Candidate = candidates.pop_front().expect("empty?!");
        }

        candidates
    }

    /// Produce a `publish::IptSet`, from a list of IPT selected for publication
    ///
    /// Updates each chosen `Ipt`'s `last_descriptor_expiry_including_slop`
    ///
    /// The returned `IptSet` set is in the same order as `selected`.
    ///
    /// ### Performance
    ///
    /// This function is at worst O(N) where N is the number of IPTs.
    /// See the performance note on [`run_once()`](Self::run_once).
    fn make_publish_set<'i>(
        selected: impl IntoIterator<Item = &'i Ipt>,
        lifetime: Duration,
    ) -> Result<ipt_set::IptSet, FatalError> {
        let ipts = selected
            .into_iter()
            .map(|current_ipt| {
                let TS::Good { details, .. } = &current_ipt.status_last else {
                    return Err(internal!("was good but now isn't?!").into());
                };

                let publish = current_ipt.for_publish(details)?;

                // last_descriptor_expiry_including_slop was earlier merged in from
                // the previous IptSet, and here we copy it back
                let publish = ipt_set::IptInSet {
                    ipt: publish,
                    lid: current_ipt.lid,
                };

                Ok::<_, FatalError>(publish)
            })
            .collect::<Result<_, _>>()?;

        Ok(ipt_set::IptSet { ipts, lifetime })
    }

    /// Run one iteration of the loop
    ///
    /// Either do some work, making changes to our state,
    /// or, if there's nothing to be done, wait until there *is* something to do.
    ///
    /// ### Implementation approach
    ///
    /// Every time we wake up we idempotently make progress
    /// by searching our whole state machine, looking for something to do.
    /// If we find something to do, we do that one thing, and search again.
    /// When we're done, we unconditionally recalculate the IPTs to publish, and sleep.
    ///
    /// This approach avoids the need for complicated reasoning about
    /// which state updates need to trigger other state updates,
    /// and thereby avoids several classes of potential bugs.
    /// However, it has some performance implications:
    ///
    /// ### Performance
    ///
    /// Events relating to an IPT occur, at worst,
    /// at a rate proportional to the current number of IPTs,
    /// times the maximum flap rate of any one IPT.
    ///
    /// [`idempotently_progress_things_now`](Self::idempotently_progress_things_now)
    /// can be called more than once for each such event,
    /// but only a finite number of times per IPT.
    ///
    /// Therefore, overall, our work rate is O(N^2) where N is the number of IPTs.
    /// We think this is tolerable,
    /// but it does mean that the principal functions should be written
    /// with an eye to avoiding "accidentally quadratic" algorithms,
    /// because that would make the whole manager cubic.
    /// Ideally we would avoid O(N.log(N)) algorithms.
    ///
    /// (Note that the number of IPTs can be significantly larger than
    /// the maximum target of 20, if the service is very busy so the intro points
    /// are cycling rapidly due to the need to replace the replay database.)
    async fn run_once(
        &mut self,
        // This is a separate argument for borrowck reasons
        publisher: &mut IptsManagerView,
    ) -> Result<ShutdownStatus, FatalError> {
        let now = {
            // Block to persuade borrow checker that publish_set isn't
            // held over an await point.

            let mut publish_set = publisher.borrow_for_update(self.imm.runtime.clone());

            Self::import_new_expiry_times(&mut self.state.irelays, &publish_set);

            let mut loop_limit = 0..(
                // Work we do might be O(number of intro points),
                // but we might also have cycled the intro points due to many requests.
                // 10K is a guess at a stupid upper bound on the number of times we
                // might cycle ipts during a descriptor lifetime.
                // We don't need a tight bound; if we're going to crash. we can spin a bit first.
                (self.target_n_intro_points() + 1) * 10_000
            );
            let now = loop {
                let _: usize = loop_limit.next().expect("IPT manager is looping");

                if let Some(now) = self.idempotently_progress_things_now()? {
                    break now;
                }
            };

            // TODO HSS: Maybe something at level Error or Info, for example
            // Log an error if everything is terrilbe
            //   - we have >=N Faulty IPTs ?
            //    we have only Faulty IPTs and can't select another due to 2N limit ?
            // Log at info if and when we publish?  Maybe the publisher should do that?

            if let Err(operr) = self.compute_iptsetstatus_publish(&now, &mut publish_set) {
                // This is not good, is it.
                publish_set.ipts = None;
                let wait = operr.log_retry_max(&self.imm.nick)?;
                now.update(wait);
            };

            self.expire_old_expiry_times(&mut publish_set, &now);

            drop(publish_set); // release lock, and notify publisher of any changes

            now
        };

        assert_ne!(
            now.clone().shortest(),
            Some(Duration::ZERO),
            "IPT manager zero timeout, would loop"
        );

        let mut new_configs = self.state.new_configs.next().fuse();

        select_biased! {
            () = now.wait_for_earliest(&self.imm.runtime).fuse() => {},
            shutdown = self.state.shutdown.next().fuse() => {
                info!("HS service {}: terminating due to shutdown signal", &self.imm.nick);
                // We shouldn't be receiving anything on thisi channel.
                assert!(shutdown.is_none());
                return Ok(ShutdownStatus::Terminate)
            },

            update = self.state.status_recv.next() => {
                let (lid, update) = update.ok_or_else(|| internal!("update mpsc ended!"))?;
                self.state.handle_ipt_status_update(&self.imm, lid, update);
            }

            _dir_event = async {
                match self.state.last_irelay_selection_outcome {
                    Ok(()) => future::pending().await,
                    // This boxes needlessly but it shouldn't really happen
                    Err(()) => self.imm.dirprovider.events().next().await,
                }
            }.fuse() => {
                self.state.last_irelay_selection_outcome = Ok(());
            }

            new_config = new_configs => {
                let Some(new_config) = new_config else {
                    trace!("HS service {}: terminating due to EOF on config updates stream",
                           &self.imm.nick);
                    return Ok(ShutdownStatus::Terminate);
                };
                self.state.current_config = new_config;
                self.state.last_irelay_selection_outcome = Ok(());
            }
        }

        Ok(ShutdownStatus::Continue)
    }

    /// IPT Manager main loop, runs as a task
    ///
    /// Contains the error handling, including catching panics.
    async fn main_loop_task(mut self, mut publisher: IptsManagerView) {
        loop {
            match async {
                AssertUnwindSafe(self.run_once(&mut publisher))
                    .catch_unwind()
                    .await
                    .map_err(|_: Box<dyn Any + Send>| internal!("IPT manager crashed"))?
            }
            .await
            {
                Err(crash) => {
                    error!("HS service {} crashed! {}", &self.imm.nick, crash);
                    break;
                }
                Ok(ShutdownStatus::Continue) => continue,
                Ok(ShutdownStatus::Terminate) => break,
            }
        }
        // TODO HSS: Set status to Shutdown.
    }

    /// Target number of intro points
    pub(crate) fn target_n_intro_points(&self) -> usize {
        self.state.current_config.num_intro_points.into()
    }

    /// Maximum number of concurrent intro point relays
    pub(crate) fn max_n_intro_relays(&self) -> usize {
        // TODO HSS max_n_intro_relays should be configurable
        // TODO HSS consider default, in context of intro point forcing attacks
        self.target_n_intro_points() * 2
    }
}

// This is somewhat abbreviated but it is legible and enough for most purposes.
impl Debug for IptRelay {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        writeln!(f, "IptRelay {}", self.relay)?;
        write!(
            f,
            "          planned_retirement: {:?}",
            self.planned_retirement
        )?;
        for ipt in &self.ipts {
            write!(
                f,
                "\n          ipt {} {} {:?} ldeis={:?}",
                match ipt.is_current {
                    Some(IsCurrent) => "cur",
                    None => "old",
                },
                &ipt.lid,
                &ipt.status_last,
                &ipt.last_descriptor_expiry_including_slop,
            )?;
        }
        Ok(())
    }
}

/// Mockable state for the IPT Manager
///
/// This allows us to use a fake IPT Establisher and IPT Publisher,
/// so that we can unit test the Manager.
pub(crate) trait Mockable<R>: Debug + Send + Sync + Sized + 'static {
    /// IPT establisher type
    type IptEstablisher: Send + Sync + 'static;

    /// A random number generator
    type Rng<'m>: rand::Rng + rand::CryptoRng + 'm;

    /// Return a random number generator
    fn thread_rng(&mut self) -> Self::Rng<'_>;

    /// Call `IptEstablisher::new`
    fn make_new_ipt(
        &mut self,
        imm: &Immutable<R>,
        params: IptParameters,
    ) -> Result<(Self::IptEstablisher, watch::Receiver<IptStatus>), FatalError>;

    /// Call `IptEstablisher::start_accepting`
    fn start_accepting(&self, establisher: &ErasedIptEstablisher);
}

impl<R: Runtime> Mockable<R> for Real<R> {
    type IptEstablisher = IptEstablisher;

    /// A random number generator
    type Rng<'m> = rand::rngs::ThreadRng;

    /// Return a random number generator
    fn thread_rng(&mut self) -> Self::Rng<'_> {
        rand::thread_rng()
    }

    fn make_new_ipt(
        &mut self,
        imm: &Immutable<R>,
        params: IptParameters,
    ) -> Result<(Self::IptEstablisher, watch::Receiver<IptStatus>), FatalError> {
        IptEstablisher::new(&imm.runtime, params, self.circ_pool.clone(), &imm.keymgr)
    }

    fn start_accepting(&self, establisher: &ErasedIptEstablisher) {
        let establisher: &IptEstablisher = <dyn Any>::downcast_ref(establisher)
            .expect("upcast failure, ErasedIptEstablisher is not IptEstablisher!");
        establisher.start_accepting();
    }
}

/// Joins two iterators, by keys, one of which is a subset of the other
///
/// `bigger` and `smaller` are iterators yielding `BI` and `SI`.
///
/// The key `K`, which can be extracted from each element of either iterator,
/// is `PartialEq` and says whether a `BI` is "the same as" an `SI`.
///
/// `call` is called for each `K` which appears in both lists, in that same order.
/// Nothing is done about elements which are only in `bigger`.
///
/// (The behaviour with duplicate entries is unspecified.)
///
/// The algorithm has complexity `O(N_bigger)`,
/// and also a working set of `O(N_bigger)`.
#[allow(dead_code)] // TODO HSS remove
fn merge_join_subset_by<'out, K, BI, SI>(
    bigger: impl IntoIterator<Item = BI> + 'out,
    bigger_keyf: impl Fn(&BI) -> K + 'out,
    smaller: impl IntoIterator<Item = SI> + 'out,
    smaller_keyf: impl Fn(&SI) -> K + 'out,
) -> impl Iterator<Item = (K, BI, SI)> + 'out
where
    K: Eq + Hash + Clone + 'out,
    BI: 'out,
    SI: 'out,
{
    let mut smaller: HashMap<K, SI> = smaller
        .into_iter()
        .map(|si| (smaller_keyf(&si), si))
        .collect();

    bigger.into_iter().filter_map(move |bi| {
        let k = bigger_keyf(&bi);
        let si = smaller.remove(&k)?;
        Some((k, bi, si))
    })
}

// TODO HSS add unit tests for IptManager
// Especially, we want to exercise all code paths in idempotently_progress_things_now

#[cfg(test)]
mod test {
    // @@ begin test lint list maintained by maint/add_warning @@
    #![allow(clippy::bool_assert_comparison)]
    #![allow(clippy::clone_on_copy)]
    #![allow(clippy::dbg_macro)]
    #![allow(clippy::print_stderr)]
    #![allow(clippy::print_stdout)]
    #![allow(clippy::single_char_pattern)]
    #![allow(clippy::unwrap_used)]
    #![allow(clippy::unchecked_duration_subtraction)]
    #![allow(clippy::useless_vec)]
    #![allow(clippy::needless_pass_by_value)]
    //! <!-- @@ end test lint list maintained by maint/add_warning @@ -->
    #![allow(clippy::match_single_binding)] // false positives, need the lifetime extension
    use super::*;

    use crate::config::OnionServiceConfigBuilder;
    use crate::svc::ipt_establish::GoodIptDetails;
    use crate::svc::test::{create_keymgr, create_storage_handles_from_state_mgr};
    use crate::test_temp_dir::TestTempDir;
    use rand::SeedableRng as _;
    use slotmap::DenseSlotMap;
    use std::collections::BTreeMap;
    use std::sync::Mutex;
    use tor_basic_utils::test_rng::TestingRng;
    use tor_netdir::testprovider::TestNetDirProvider;
    use tor_rtmock::MockRuntime;
    use tracing_test::traced_test;

    slotmap::new_key_type! {
        struct MockEstabId;
    }

    type MockEstabs = Arc<Mutex<DenseSlotMap<MockEstabId, MockEstabState>>>;

    fn ms(ms: u64) -> Duration {
        Duration::from_millis(ms)
    }

    #[derive(Debug)]
    struct Mocks {
        rng: TestingRng,
        estabs: MockEstabs,
    }

    #[derive(Debug)]
    struct MockEstabState {
        st_tx: watch::Sender<IptStatus>,
        params: IptParameters,
    }

    #[derive(Debug)]
    struct MockEstab {
        esid: MockEstabId,
        estabs: MockEstabs,
    }

    impl Mockable<MockRuntime> for Mocks {
        type IptEstablisher = MockEstab;
        type Rng<'m> = &'m mut TestingRng;

        fn thread_rng(&mut self) -> Self::Rng<'_> {
            &mut self.rng
        }

        fn make_new_ipt(
            &mut self,
            _imm: &Immutable<MockRuntime>,
            params: IptParameters,
        ) -> Result<(Self::IptEstablisher, watch::Receiver<IptStatus>), FatalError> {
            let (st_tx, st_rx) = watch::channel();
            let estab = MockEstabState { st_tx, params };
            let esid = self.estabs.lock().unwrap().insert(estab);
            let estab = MockEstab {
                esid,
                estabs: self.estabs.clone(),
            };
            Ok((estab, st_rx))
        }

        fn start_accepting(&self, _establisher: &ErasedIptEstablisher) {}
    }

    impl Drop for MockEstab {
        fn drop(&mut self) {
            let mut estabs = self.estabs.lock().unwrap();
            let _: MockEstabState = estabs
                .remove(self.esid)
                .expect("dropping non-recorded MockEstab");
        }
    }

    struct MockedIptManager<'d> {
        estabs: MockEstabs,
        pub_view: ipt_set::IptsPublisherView,
        shut_tx: broadcast::Sender<Void>,
        #[allow(dead_code)]
        cfg_tx: watch::Sender<Arc<OnionServiceConfig>>,
        #[allow(dead_code)] // ensures temp dir lifetime; paths stored in self
        temp_dir: &'d TestTempDir,
    }

    impl<'d> MockedIptManager<'d> {
        fn startup(runtime: MockRuntime, temp_dir: &'d TestTempDir) -> Self {
            let dir: TestNetDirProvider = tor_netdir::testnet::construct_netdir()
                .unwrap_if_sufficient()
                .unwrap()
                .into();

            let nick: HsNickname = "nick".to_string().try_into().unwrap();

            let cfg = OnionServiceConfigBuilder::default()
                .nickname(nick.clone())
                .build()
                .unwrap();

            let (cfg_tx, cfg_rx) = watch::channel_with(Arc::new(cfg));

            let (rend_tx, _rend_rx) = mpsc::channel(10);
            let (shut_tx, shut_rx) = broadcast::channel::<Void>(0);

            let estabs: MockEstabs = Default::default();

            let mocks = Mocks {
                rng: TestingRng::seed_from_u64(0),
                estabs: estabs.clone(),
            };

            let mistrust = fs_mistrust::Mistrust::new_dangerously_trust_everyone();

            // Don't provide a subdir; the ipt_mgr is supposed to add any needed subdirs
            let state_dir = temp_dir
                // untracked is OK because our return value captures 'd
                .subdir_untracked("state_dir");

            let state_mgr =
                tor_persist::FsStateMgr::from_path_and_mistrust(&state_dir, &mistrust).unwrap();

            let (state_mgr, iptpub_state_handle) =
                create_storage_handles_from_state_mgr(state_mgr, &nick);

            let (mgr_view, pub_view) =
                ipt_set::ipts_channel(&runtime, iptpub_state_handle).unwrap();

            let keymgr = create_keymgr(temp_dir);
            let keymgr = keymgr.into_untracked(); // OK because our return value captures 'd
            let mgr = IptManager::new(
                runtime.clone(),
                Arc::new(dir),
                nick,
                cfg_rx,
                rend_tx,
                shut_rx,
                state_mgr,
                mocks,
                keymgr,
                &state_dir,
                &mistrust,
            )
            .unwrap();

            mgr.launch_background_tasks(mgr_view).unwrap();

            MockedIptManager {
                estabs,
                pub_view,
                shut_tx,
                cfg_tx,
                temp_dir,
            }
        }

        async fn shutdown_check_no_tasks(self, runtime: &MockRuntime) {
            drop(self.shut_tx);
            runtime.progress_until_stalled().await;
            assert_eq!(runtime.mock_task().n_tasks(), 1); // just us
        }

        fn estabs_inventory(&self) -> impl Eq + Debug + 'static {
            let estabs = self.estabs.lock().unwrap();
            let estabs = estabs
                .values()
                .map(|MockEstabState { params: p, .. }| {
                    (
                        p.lid,
                        (
                            p.target.clone(),
                            // We want to check the key values, but they're very hard to get at
                            // in a way we can compare.  Especially the private keys, for which
                            // we can't getting a clone or copy of the private key material out of the Arc.
                            // They're keypairs, we can use the debug rep which shows the public half.
                            // That will have to do.
                            format!("{:?}", p.k_sid),
                            format!("{:?}", p.k_ntor),
                        ),
                    )
                })
                .collect::<BTreeMap<_, _>>();
            estabs
        }
    }

    #[test]
    #[traced_test]
    fn test_mgr_lifecycle() {
        MockRuntime::test_with_various(|runtime| async move {
            let temp_dir = test_temp_dir!();

            let m = MockedIptManager::startup(runtime.clone(), &temp_dir);
            runtime.progress_until_stalled().await;

            // We expect it to try to establish 3 IPTs
            const EXPECT_N_IPTS: usize = 3;
            assert_eq!(m.estabs.lock().unwrap().len(), EXPECT_N_IPTS);
            assert!(m.pub_view.borrow_for_publish().ipts.is_none());

            // Advancing time a bit and it still shouldn't publish anything
            runtime.advance_by(ms(500)).await;
            runtime.progress_until_stalled().await;
            assert!(m.pub_view.borrow_for_publish().ipts.is_none());

            let good = GoodIptDetails {
                link_specifiers: vec![],
                ipt_kp_ntor: [0x55; 32].into(),
            };

            // Imagine that one of our IPTs becomes good
            m.estabs
                .lock()
                .unwrap()
                .values_mut()
                .next()
                .unwrap()
                .st_tx
                .borrow_mut()
                .status = IptStatusStatus::Good(good.clone());

            // TODO HSS test that we havne't called  start_accepting

            // It won't publish until a further fastest establish time
            // Ie, until a further 500ms = 1000ms
            runtime.progress_until_stalled().await;
            assert!(m.pub_view.borrow_for_publish().ipts.is_none());
            runtime.advance_by(ms(499)).await;
            assert!(m.pub_view.borrow_for_publish().ipts.is_none());
            runtime.advance_by(ms(1)).await;
            match m.pub_view.borrow_for_publish().ipts.as_mut().unwrap() {
                pub_view => {
                    assert_eq!(pub_view.ipts.len(), 1);
                    assert_eq!(pub_view.lifetime, ms(30 * 60 * 1000));
                }
            };

            // TODO HSS test that we have called start_accepting on the right IPTs

            // Set the other IPTs to be Good too
            for e in m.estabs.lock().unwrap().values_mut().skip(1) {
                e.st_tx.borrow_mut().status = IptStatusStatus::Good(good.clone());
            }
            runtime.progress_until_stalled().await;
            match m.pub_view.borrow_for_publish().ipts.as_mut().unwrap() {
                pub_view => {
                    assert_eq!(pub_view.ipts.len(), EXPECT_N_IPTS);
                    assert_eq!(pub_view.lifetime, ms(12 * 3600 * 1000));
                }
            };

            // TODO HSS test that we have called start_accepting on the right IPTs

            let estabs_inventory = m.estabs_inventory();

            // Shut down
            m.shutdown_check_no_tasks(&runtime).await;

            // ---------- restart! ----------
            info!("*** Restarting ***");

            let m = MockedIptManager::startup(runtime.clone(), &temp_dir);
            runtime.progress_until_stalled().await;

            assert_eq!(estabs_inventory, m.estabs_inventory());

            // TODO HSS test that we have called start_accepting on all the old IPTs

            // Shut down
            m.shutdown_check_no_tasks(&runtime).await;
        });
    }

    #[test]
    fn test_merge_join_subset_by() {
        fn chk(bigger: &str, smaller: &str, output: &str) {
            let keyf = |c: &char| *c;

            assert_eq!(
                merge_join_subset_by(bigger.chars(), keyf, smaller.chars(), keyf)
                    .map(|(k, b, s)| {
                        assert_eq!(k, b);
                        assert_eq!(k, s);
                        k
                    })
                    .collect::<String>(),
                output,
            );
        }

        chk("abc", "abc", "abc");
        chk("abc", "a", "a");
        chk("abc", "b", "b");
        chk("abc", "c", "c");
        chk("abc", "x", ""); // wrong input, but test it anyway
        chk("b", "abc", "b"); // wrong input, but test it anyway

        chk("abc", "", "");
        chk("", "abc", ""); // wrong input, but test it anyway
    }
}
