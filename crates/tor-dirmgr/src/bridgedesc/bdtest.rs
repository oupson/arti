//! Tests for bridge descriptor downloading

// @@ begin test lint list maintained by maint/add_warning @@
#![allow(clippy::bool_assert_comparison)]
#![allow(clippy::clone_on_copy)]
#![allow(clippy::dbg_macro)]
#![allow(clippy::print_stderr)]
#![allow(clippy::print_stdout)]
#![allow(clippy::single_char_pattern)]
#![allow(clippy::unwrap_used)]
//! <!-- @@ end test lint list maintained by maint/add_warning @@ -->

#![allow(unused_variables)] // XXX
#![allow(dead_code)] // XXX

use std::future::Future;
use std::iter;
use std::ops::Bound;

use futures::select_biased;
use futures::stream::FusedStream;
use futures::Stream;
use itertools::{chain, Itertools};
use tempfile::TempDir;
use time::OffsetDateTime;
use tracing_test::traced_test;

use tor_linkspec::HasAddrs;
use tor_rtcompat::SleepProvider;
use tor_rtmock::time::MockSleepProvider;
use tor_rtmock::MockSleepRuntime;

use super::*;

const EXAMPLE_DESCRIPTOR: &str = include_str!("../../testdata/routerdesc1.txt");
const EXAMPLE_PORT: u16 = 9001;

fn example_validity() -> (SystemTime, SystemTime) {
    let (_, (t, u)) = RouterDesc::parse(EXAMPLE_DESCRIPTOR)
        .unwrap()
        .dangerously_assume_wellsigned()
        .dangerously_into_parts();
    let ret = |tb| match tb {
        Bound::Included(t) | Bound::Excluded(t) => t,
        _ => panic!(),
    };
    (ret(t), ret(u))
}
fn example_wallclock() -> SystemTime {
    example_validity().0 + Duration::from_secs(10)
}

type RealRuntime = tor_rtcompat::tokio::TokioNativeTlsRuntime;
type R = MockSleepRuntime<RealRuntime>;
type M = Mock;
type Bdm = BridgeDescMgr<R, M>;
type RT = RetryTime;
use Error::TestError as TE;

#[derive(Debug, Clone)]
struct Mock {
    sleep: MockSleepProvider,

    // Using an async mutex lets us block a call to `download`
    // so we can see what the state is mid-download.
    mstate: Arc<futures::lock::Mutex<MockState>>,
}

const MOCK_NOT_MODIFIED: &str = "IF-MODIFIED-SINCE ";

struct MockState {
    /// Maps the port number for a download, to what we should return
    ///
    /// If the Ok string starts with `MOCK_NOT_MODIFIED` then the rest is the Debug
    /// output from a SystemTime.   In this case the manager is supposed to pass
    /// `if_modified_since` as `Some(that SystemTime)`, and we will actually return `None`.
    ///
    /// Otherwise the `if_modified_since` from the manager will be ignored
    /// and we always give it Some.
    docs: HashMap<u16, Result<String, Error>>,

    download_calls: usize,
}

impl Mockable<R> for Mock {}

#[async_trait]
impl mockable::MockableAPI<R> for Mock {
    type CircMgr = ();

    async fn download(
        self,
        _runtime: &R,
        _circmgr: &Self::CircMgr,
        bridge: &BridgeConfig,
        if_modified_since: Option<SystemTime>,
    ) -> Result<Option<String>, Error> {
        eprint!("download ...");
        let mut mstate = self.mstate.lock().await;
        mstate.download_calls += 1;
        eprintln!("#{} {:?}", mstate.download_calls, bridge);
        let addr = bridge
            .addrs()
            .get(0)
            .ok_or(TE("bridge has no error", RT::Never))?;
        let doc = mstate
            .docs
            .get(&addr.port())
            .ok_or(TE("no document", RT::AfterWaiting))?;
        doc.clone().map(|text| {
            if let Some(expect_ims) = text.strip_prefix(MOCK_NOT_MODIFIED) {
                eprintln!("#{} {:?}", mstate.download_calls, text);
                assert_eq!(format!("{:?}", if_modified_since.unwrap()), expect_ims,);
                None
            } else {
                Some(text)
            }
        })
    }
}

impl Mock {
    async fn expect_download_calls(&self, expected: usize) {
        let mut mstate = self.mstate.lock().await;
        assert_eq!(mstate.download_calls, expected);
        mstate.download_calls = 0;
    }
}

fn setup() -> (TempDir, Bdm, R, M, BridgeKey, rusqlite::Connection) {
    let runtime = RealRuntime::current().unwrap();
    let runtime = MockSleepRuntime::new(runtime);
    let sleep = runtime.mock_sleep().clone();

    sleep.jump_to(example_wallclock());

    let mut docs = HashMap::new();
    docs.insert(EXAMPLE_PORT, Ok(EXAMPLE_DESCRIPTOR.into()));

    let mstate = Arc::new(futures::lock::Mutex::new(MockState {
        docs,
        download_calls: 0,
    }));

    let mock = Mock { sleep, mstate };

    let (db_tmp_dir, store) = crate::storage::sqlite::test::new_empty().unwrap();
    let store = Arc::new(Mutex::new(Box::new(store) as _));

    let sql_path = db_tmp_dir.path().join("db.sql");
    let conn = rusqlite::Connection::open(&sql_path).unwrap();

    let bdm = BridgeDescMgr::<R, M>::new_internal(
        runtime.clone(),
        (),
        store,
        &Default::default(),
        mock.clone(),
    )
    .unwrap();

    let bridge = "51.68.172.83:9001 EB6EFB27F29AC9511A4246D7ABE1AFABFB416FF1"
        .parse()
        .unwrap();
    let bridge = Arc::new(bridge);

    (db_tmp_dir, bdm, runtime, mock, bridge, conn)
}

async fn stream_drain_ready<S: Stream + Unpin + FusedStream>(s: &mut S) -> usize {
    let mut count = 0;
    while select_biased! {
        _ = s.next() => true,
        () = future::ready(()) => false,
    } {
        tor_rtcompat::task::yield_now().await;
        count += 1;
    }
    count
}

async fn stream_drain_until<S, F, FF, Y>(attempts: usize, s: &mut S, mut f: F) -> Y
where
    S: Stream + Unpin + FusedStream,
    S::Item: Debug,
    F: FnMut() -> FF,
    FF: Future<Output = Option<Y>>,
{
    for _ in 0..attempts {
        let event = s.next().await;
        eprintln!("stream_drain_until, got {:?}", event);

        if let Some(y) = f().await {
            return y;
        }
    }
    panic!("untilness didn't occur");
}

fn queues_are_empty(bdm: &Bdm) -> Option<()> {
    let state = bdm.mgr.lock_only();
    (state.running.is_empty() && state.queued.is_empty()).then(|| ())
}

fn in_results(bdm: &Bdm, bridge: &BridgeKey, wanted: Option<Result<(), ()>>) -> Option<()> {
    let bridges = bdm.bridges();
    let got = bridges.get(bridge);
    let got = got.map(|got| got.as_ref().map(|_| ()).map_err(|_| ()));
    (got == wanted).then(|| ())
}

async fn clear_and_re_request<S>(bdm: &Bdm, events: &mut S, bridge: &BridgeKey)
where
    S: Stream + Unpin + FusedStream,
    S::Item: Debug,
{
    bdm.set_bridges(&[]);
    stream_drain_until(3, events, || async {
        in_results(bdm, bridge, None)
            .and_then(|()| bdm.mgr.lock_only().running.is_empty().then(|| ()))
    })
    .await;
    bdm.set_bridges(&[bridge.clone()]);
}

fn bad_bridge(i: usize) -> BridgeKey {
    let bad = format!("192.126.0.1:{} EB6EFB27F29AC9511A4246D7ABE1AFABFB416FF1", i);
    let bad: BridgeConfig = bad.parse().unwrap();
    Arc::new(bad)
}

#[tokio::test]
#[traced_test]
async fn success() -> Result<(), anyhow::Error> {
    let (_db_tmp_dir, bdm, runtime, mock, bridge, ..) = setup();

    bdm.check_consistency(Some([]));

    let mut events = bdm.events().fuse();

    eprintln!("----- test downloading one descriptor -----");

    stream_drain_ready(&mut events).await;

    let hold = mock.mstate.lock().await;

    bdm.set_bridges(&[bridge.clone()]);
    bdm.check_consistency(Some([&bridge]));

    drop(hold);

    let got = stream_drain_until(3, &mut events, || async {
        bdm.bridges().get(&bridge).cloned()
    })
    .await;

    dbg!(runtime.wallclock(), example_validity(),);

    eprintln!("got: {:?}", got.unwrap());

    bdm.check_consistency(Some([&bridge]));
    mock.expect_download_calls(1).await;

    eprintln!("----- add a number of failing descriptors -----");

    const NFAIL: usize = 6;

    let bad = (1..=NFAIL).map(bad_bridge).collect_vec();

    let mut bridges = chain!(iter::once(bridge.clone()), bad.iter().cloned(),).collect_vec();

    let hold = mock.mstate.lock().await;

    bdm.set_bridges(&bridges);
    bdm.check_consistency(Some(&bridges));

    drop(hold);

    let () = stream_drain_until(13, &mut events, || async {
        bdm.check_consistency(Some(&bridges));
        bridges
            .iter()
            .all(|b| bdm.bridges().contains_key(b))
            .then(|| ())
    })
    .await;

    for b in &bad {
        bdm.bridges().get(b).unwrap().as_ref().unwrap_err();
    }

    bdm.check_consistency(Some(&bridges));
    mock.expect_download_calls(NFAIL).await;

    eprintln!("----- move the clock forward to do some retries ----------");

    mock.sleep.advance(Duration::from_secs(5000)).await;

    bdm.check_consistency(Some(&bridges));

    let () = stream_drain_until(13, &mut events, || async {
        bdm.check_consistency(Some(&bridges));
        (mock.mstate.lock().await.download_calls == NFAIL).then(|| ())
    })
    .await;

    stream_drain_ready(&mut events).await;

    bdm.check_consistency(Some(&bridges));
    mock.expect_download_calls(NFAIL).await;

    eprintln!("----- set the bridges to the ones we have already ----------");

    let hold = mock.mstate.lock().await;

    bdm.set_bridges(&bridges);
    bdm.check_consistency(Some(&bridges));

    drop(hold);

    let events_counted = stream_drain_ready(&mut events).await;
    assert_eq!(events_counted, 0);
    bdm.check_consistency(Some(&bridges));
    mock.expect_download_calls(0).await;

    eprintln!("----- set the bridges to one fewer than we have already ----------");

    let _ = bridges.pop().unwrap();

    let hold = mock.mstate.lock().await;

    bdm.set_bridges(&bridges);
    bdm.check_consistency(Some(&bridges));

    drop(hold);

    let events_counted = stream_drain_ready(&mut events).await;
    assert_eq!(events_counted, 1);
    bdm.check_consistency(Some(&bridges));
    mock.expect_download_calls(0).await;

    eprintln!("----- remove a bridge while we have some requeued ----------");

    let hold = mock.mstate.lock().await;

    mock.sleep.advance(Duration::from_secs(8000)).await;
    bdm.check_consistency(Some(&bridges));

    // should yield, but not produce any events yet
    let count = stream_drain_ready(&mut events).await;
    assert_eq!(count, 0);
    bdm.check_consistency(Some(&bridges));

    let removed = bridges.pop().unwrap();
    bdm.set_bridges(&bridges);

    // should produce a removed bridge event
    let () = stream_drain_until(1, &mut events, || async {
        bdm.check_consistency(Some(&bridges));
        (!bdm.bridges().contains_key(&removed)).then(|| ())
    })
    .await;

    drop(hold);

    // should produce a removed bridge event
    let () = stream_drain_until(1, &mut events, || async {
        bdm.check_consistency(Some(&bridges));
        queues_are_empty(&bdm)
    })
    .await;

    {
        // When we cancel the download, we race with the manager.
        // Maybe the download for the one we removed was started, or maybe not.
        let mut mstate = mock.mstate.lock().await;
        assert!(
            ((NFAIL - 1)..=NFAIL).contains(&mstate.download_calls),
            "{:?}",
            mstate.download_calls
        );
        mstate.download_calls = 0;
    }

    Ok(())
}

#[tokio::test]
#[traced_test]
async fn cache() -> Result<(), anyhow::Error> {
    let (_db_tmp_path, bdm, runtime, mock, bridge, sql_conn, ..) = setup();
    let mut events = bdm.events().fuse();

    let in_results = |wanted| in_results(&bdm, &bridge, wanted);

    eprintln!("----- test that a downloaded descriptor goes into the cache -----");

    bdm.set_bridges(&[bridge.clone()]);
    stream_drain_until(3, &mut events, || async { in_results(Some(Ok(()))) }).await;

    mock.expect_download_calls(1).await;

    sql_conn
        .query_row("SELECT * FROM BridgeDescs", [], |row| {
            let get_time = |f| -> SystemTime { row.get_unwrap::<&str, OffsetDateTime>(f).into() };
            let bline: String = row.get_unwrap("bridge_line");
            let fetched: SystemTime = get_time("fetched");
            let until: SystemTime = get_time("until");
            let contents: String = row.get_unwrap("contents");
            let now = runtime.wallclock();
            assert_eq!(bline, bridge.to_string());
            assert!(fetched <= now);
            assert!(now < until);
            assert_eq!(contents, EXAMPLE_DESCRIPTOR);
            Ok(())
        })
        .unwrap();

    eprintln!("----- forget the descriptor and try to reload it from the cache -----");

    clear_and_re_request(&bdm, &mut events, &bridge).await;
    stream_drain_until(3, &mut events, || async { in_results(Some(Ok(()))) }).await;

    // Should not have been re-downloaded, since the fetch time is great.
    mock.expect_download_calls(0).await;

    eprintln!("----- corrupt the cache and check we re-download -----");

    sql_conn
        .execute_batch("UPDATE BridgeDescs SET contents = 'garbage'")
        .unwrap();

    clear_and_re_request(&bdm, &mut events, &bridge).await;
    stream_drain_until(3, &mut events, || async { in_results(Some(Ok(()))) }).await;

    mock.expect_download_calls(1).await;

    eprintln!("----- advance the lock and check that we do an if-modified-since -----");

    let published = bdm
        .bridges()
        .get(&bridge)
        .unwrap()
        .as_ref()
        .unwrap()
        .as_ref()
        .published();

    mock.mstate.lock().await.docs.insert(
        EXAMPLE_PORT,
        Ok(format!("{}{:?}", MOCK_NOT_MODIFIED, published)),
    );

    // Exceeds default max_refetch
    mock.sleep.advance(Duration::from_secs(20000)).await;

    stream_drain_until(3, &mut events, || async {
        (mock.mstate.lock().await.download_calls > 0).then(|| ())
    })
    .await;

    mock.expect_download_calls(1).await;

    Ok(())
}