//! `#[psync::graph]` keeps results and errors identical while overlapping independent waits.
//! Simulated I/O is pending for a number of executor polls; elapsed time is counted in polls.
use psync::{Executor, graph};
use std::{
    cell::{Cell, RefCell},
    future::{Future, poll_fn},
    pin::pin,
    rc::Rc,
    task::Poll,
};

thread_local! {
    static ACTIVE: Cell<usize> = const { Cell::new(0) };
    static PEAK: Cell<usize> = const { Cell::new(0) };
}

/// Pending for `ticks` polls, then returns `value`. Tracks how many are in flight.
async fn io<T>(ticks: u32, value: T) -> T {
    let mut left = ticks;
    let mut started = false;
    let mut value = Some(value);
    poll_fn(move |cx| {
        if !started {
            started = true;
            ACTIVE.set(ACTIVE.get() + 1);
            PEAK.set(PEAK.get().max(ACTIVE.get()));
        }
        if left == 0 {
            ACTIVE.set(ACTIVE.get() - 1);
            return Poll::Ready(value.take().expect("polled after completion"));
        }
        left -= 1;
        cx.waker().wake_by_ref();
        Poll::Pending
    })
    .await
}

/// Run on psync's executor; returns the output, polls taken, and peak in-flight I/O.
fn run<T: 'static>(future: impl Future<Output = T> + 'static) -> (T, usize, usize) {
    PEAK.set(0);
    let executor = Executor::default();
    let out = Rc::new(RefCell::new(None));
    let polls = Rc::new(Cell::new(0));
    let (o, p) = (out.clone(), polls.clone());
    executor.spawn(1, async move {
        let mut future = pin!(future);
        let value = poll_fn(|cx| {
            p.set(p.get() + 1);
            future.as_mut().poll(cx)
        })
        .await;
        *o.borrow_mut() = Some(value);
    });
    executor.run_until_stalled();
    let value = out.borrow_mut().take().expect("finished");
    (value, polls.get(), PEAK.get())
}

const T: u32 = 10;

// The proposal's graph_test: four leaves, two joins, one root (7 waits, depth 3).
async fn sequential_tree() -> u64 {
    let a = io(T, 1 * 2 + 1).await;
    let b = io(T, 2 * 2 + 1).await;
    let c = io(T, 3 * 2 + 1).await;
    let d = io(T, 4 * 2 + 1).await;
    let e = io(T, (a + b) * 2 + 1).await;
    let f = io(T, (c + d) * 2 + 1).await;
    io(T, (e + f) * 2 + 1).await
}

#[graph]
async fn graph_tree() -> u64 {
    let a = io(T, 1 * 2 + 1).await;
    let b = io(T, 2 * 2 + 1).await;
    let c = io(T, 3 * 2 + 1).await;
    let d = io(T, 4 * 2 + 1).await;
    let e = io(T, (a + b) * 2 + 1).await;
    let f = io(T, (c + d) * 2 + 1).await;
    io(T, (e + f) * 2 + 1).await
}

#[test]
fn tree_overlaps_independent_waits() {
    let (seq, seq_polls, seq_peak) = run(sequential_tree());
    let (par, par_polls, par_peak) = run(graph_tree());
    assert_eq!(seq, par);
    assert_eq!((seq_peak, par_peak), (1, 4));
    // A wait of T polls hands over to the next in the same poll: 7 waits in sequence vs. 3 levels.
    assert_eq!(seq_polls, 7 * T as usize + 1);
    assert_eq!(par_polls, 3 * T as usize + 1);
}

// Per-item chains fuse: each a_i -> b_i runs as soon as its own a_i finishes.
#[graph]
async fn chains() -> (u32, u32) {
    let a1 = io(T, 1).await;
    let a2 = io(2 * T, 2).await;
    let b1 = io(2 * T, a1 + 10).await;
    let b2 = io(T, a2 + 10).await;
    (b1, b2)
}

#[test]
fn chains_do_not_wait_for_each_other() {
    let ((b1, b2), polls, peak) = run(chains());
    assert_eq!((b1, b2), (11, 12));
    assert_eq!(peak, 2);
    // Both chains take 3T; level barriers (a1 || a2, then b1 || b2) would take 2T + 2T.
    assert_eq!(polls, 3 * T as usize + 1);
}

#[derive(Debug, PartialEq)]
struct Error(&'static str);

#[graph]
async fn first_error_wins(fail: [bool; 3]) -> Result<u32, Error> {
    let x = io(T, if fail[0] { Err(Error("x")) } else { Ok(1) }).await?;
    let y = io(T, if fail[1] { Err(Error("y")) } else { Ok(2) }).await?;
    io(T, if fail[2] { Err(Error("z")) } else { Ok(()) }).await?;
    Ok(x + y)
}

#[test]
fn errors_match_sequential_order() {
    assert_eq!(run(first_error_wins([false; 3])).0, Ok(3));
    assert_eq!(
        run(first_error_wins([false, true, true])).0,
        Err(Error("y"))
    );
    assert_eq!(
        run(first_error_wins([true, false, true])).0,
        Err(Error("x"))
    );
    assert_eq!(
        run(first_error_wins([false, false, true])).0,
        Err(Error("z"))
    );
    assert_eq!(run(first_error_wins([false; 3])).2, 3);
}

#[graph]
async fn options(a: Option<u32>, b: Option<u32>) -> Option<u32> {
    let a = io(T, a).await?;
    let b = io(T, b).await?;
    Some(a + b)
}

#[test]
fn option_question_mark() {
    assert_eq!(run(options(Some(1), Some(2))).0, Some(3));
    assert_eq!(run(options(Some(1), None)).0, None);
    assert_eq!(run(options(Some(1), Some(2))).2, 2);
}

#[graph]
async fn shadowing_and_mut() -> Vec<u32> {
    let x = io(T, 1).await;
    let mut v = io(T, vec![0]).await;
    let x = io(T, x + 1).await; // rebinds x: must follow the first
    v.push(x);
    v
}

#[test]
fn shadowing_and_mut_bindings() {
    let (v, _, peak) = run(shadowing_and_mut());
    assert_eq!(v, [0, 2]);
    assert_eq!(peak, 2);
}

#[graph(bound = 3)]
async fn fan_out(items: Vec<u32>) -> Result<Vec<u32>, Error> {
    let mut out = Vec::new();
    for item in items {
        let doubled = io(
            T,
            if item == 99 {
                Err(Error("bad item"))
            } else {
                Ok(item * 2)
            },
        )
        .await?;
        out.push(doubled);
    }
    Ok(out)
}

#[test]
fn loops_prefetch_in_order_with_a_bound() {
    let (out, polls, peak) = run(fan_out((1..=6).collect()));
    assert_eq!(out, Ok(vec![2, 4, 6, 8, 10, 12]));
    assert_eq!(peak, 3);
    assert!(
        polls < 3 * (T as usize + 1),
        "6 items in 2 waves, got {polls} polls"
    );
    assert_eq!(run(fan_out(vec![1, 99, 3])).0, Err(Error("bad item")));
}

#[graph]
async fn carried(items: Vec<u32>) -> u32 {
    let mut total = 0;
    for item in items {
        let next = io(T, total + item).await; // reads what the previous iteration wrote
        total = next;
    }
    total
}

#[test]
fn loop_carried_dependency_stays_sequential() {
    let (total, _, peak) = run(carried(vec![1, 2, 3]));
    assert_eq!((total, peak), (6, 1));
}

#[graph]
async fn pinned() -> u32 {
    let a = io(T, 1).await;
    #[psync::seq]
    let b = io(T, 2).await;
    a + b
}

#[test]
fn seq_marker_keeps_order() {
    assert_eq!(run(pinned()).2, 1);
}

struct Counter {
    hits: u32,
}
impl Counter {
    async fn hit(&mut self) -> u32 {
        io(T, ()).await;
        self.hits += 1;
        self.hits
    }
    // `&mut self` is a unique handle, so these stay ordered (and still compile).
    #[graph]
    async fn twice(&mut self) -> (u32, u32) {
        let a = self.hit().await;
        let b = self.hit().await;
        (a, b)
    }
}

#[test]
fn mutable_receiver_stays_sequential() {
    let ((a, b), _, peak) = run(async { Counter { hits: 0 }.twice().await });
    assert_eq!(((a, b), peak), ((1, 2), 1));
}

// ---- early exits inside concurrent nodes ----

#[graph]
async fn early_return(stop: bool) -> Result<u32, Error> {
    let a = io(T, 1).await;
    if stop {
        return Ok(0);
    }
    let b = io(T, 2).await;
    Ok(a + b)
}

#[test]
fn return_is_performed_in_program_order() {
    assert_eq!(run(early_return(true)).0, Ok(0));
    assert_eq!(run(early_return(false)).0, Ok(3));
}

#[graph]
async fn let_else_and_continue(items: Vec<Option<u32>>) -> Vec<u32> {
    let mut out = Vec::new();
    for item in items {
        let Some(found) = io(T, item).await else {
            continue;
        };
        if found == 0 {
            continue;
        }
        out.push(found);
    }
    out
}

#[test]
fn loop_exits_are_replayed_per_iteration() {
    let (out, _, peak) = run(let_else_and_continue(vec![Some(1), None, Some(0), Some(4)]));
    assert_eq!((out, peak), (vec![1, 4], 4));
}

// The kube discovery shape: a guarded await per item, writes to one field, reads of others.
// (A call on `self` itself would count as touching all of `self`; kube calls through a field.)
struct Mode {
    skip: u32,
}
impl Mode {
    fn is_queryable(&self, g: u32) -> bool {
        g != self.skip
    }
}
struct Discovery {
    client: u32,
    groups: Vec<(u32, u32)>,
    mode: Mode,
}
impl Discovery {
    #[graph(bound = 8)]
    async fn run(mut self, all: Vec<u32>) -> Result<Self, Error> {
        self.groups.clear();
        for g in all {
            let key = g * 10;
            if self.mode.is_queryable(g) {
                let versions = io(
                    T,
                    if g == 99 {
                        Err(Error("bad group"))
                    } else {
                        Ok(self.client + g)
                    },
                )
                .await?;
                self.groups.push((key, versions));
            }
        }
        Ok(self)
    }
}

#[test]
fn guarded_loop_bodies_prefetch_with_field_sensitivity() {
    let new = || Discovery {
        client: 100,
        groups: vec![(0, 0)],
        mode: Mode { skip: 2 },
    };
    let (d, _, peak) = run(new().run(vec![1, 2, 3]));
    assert_eq!(d.unwrap().groups, vec![(10, 101), (30, 103)]);
    assert_eq!(peak, 2);
    assert_eq!(
        run(new().run(vec![1, 99, 3])).0.err(),
        Some(Error("bad group"))
    );
}

#[graph]
async fn awaited_arguments(names: Vec<&'static str>) -> Vec<usize> {
    let mut lens = Vec::new();
    lens.push(io(T, names[0].len()).await);
    lens.push(io(T, names[1].len()).await);
    lens
}

#[test]
fn awaits_in_arguments_are_hoisted() {
    let (lens, _, peak) = run(awaited_arguments(vec!["a", "bcd"]));
    assert_eq!((lens, peak), (vec![1, 3], 2));
}

#[graph]
async fn effects_keep_order() -> Vec<u32> {
    let mut log = Vec::new();
    log.push(1);
    let a = io(T, 2).await;
    log.push(a);
    let b = io(T, 3).await;
    log.push(b);
    log
}

#[test]
fn ordinary_statements_keep_their_order() {
    let (log, _, peak) = run(effects_keep_order());
    assert_eq!((log, peak), (vec![1, 2, 3], 2));
}

/// Pending once, like acquiring a contended lock.
async fn lock<'a>(
    state: &'a RefCell<Vec<&'static str>>,
) -> std::cell::RefMut<'a, Vec<&'static str>> {
    io(T, ()).await;
    state.borrow_mut()
}

#[graph]
async fn lock_is_a_barrier(state: &RefCell<Vec<&'static str>>) -> usize {
    let before = io(T, 1).await;
    let mut guard = lock(state).await;
    guard.push("held");
    drop(guard);
    let after = io(T, 2).await;
    before + after
}

#[test]
fn locks_stay_in_place() {
    let state = Box::leak(Box::new(RefCell::new(Vec::new())));
    let (sum, _, peak) = run(lock_is_a_barrier(state));
    assert_eq!((sum, peak), (3, 1));
}

// ---- hints ----

struct Connection;
struct Context;
impl Context {
    fn pool(&self) -> Connection {
        Connection
    }
}
async fn read_row(_conn: &mut Connection, v: u32) -> Result<u32, Error> {
    io(T, Ok(v)).await
}

#[graph]
async fn fresh_pool(context: &Context) -> Result<u32, Error> {
    #[psync::fresh]
    let pool = &mut context.pool();
    let a = read_row(pool, 1).await?;
    let b = read_row(pool, 2).await?;
    Ok(a + b)
}

#[test]
fn fresh_bindings_give_each_branch_its_own_value() {
    let context: &'static Context = Box::leak(Box::new(Context));
    assert_eq!(run(fresh_pool(context)).0, Ok(3));
    assert_eq!(run(fresh_pool(context)).2, 2);
}

#[graph]
async fn deduplicated(keys: Vec<u32>) -> Vec<(u32, u32)> {
    let mut cache = std::collections::BTreeMap::new();
    #[psync::independent]
    for k in keys {
        if !cache.contains_key(&k) {
            let v = io(T, k * 2).await;
            cache.insert(k, v);
        }
    }
    cache.into_iter().collect()
}

#[test]
fn independent_loops_run_ahead_of_their_cache() {
    let (pairs, _, peak) = run(deduplicated(vec![1, 2, 3]));
    assert_eq!((pairs, peak), (vec![(1, 2), (2, 4), (3, 6)], 3));
}

// ---- loop fission ----

// mise's remote task cache: a dedupe set guards each upload.
#[graph]
async fn dedupe_then_upload(digests: Vec<u32>) -> Result<Vec<u32>, Error> {
    let mut published = std::collections::BTreeSet::new();
    let mut uploaded = Vec::new();
    for d in digests {
        if published.insert(d) {
            let receipt = io(T, if d == 99 { Err(Error("upload")) } else { Ok(d) }).await?;
            uploaded.push(receipt);
        }
    }
    Ok(uploaded)
}

#[test]
fn fission_runs_carried_state_first() {
    let (out, _, peak) = run(dedupe_then_upload(vec![1, 2, 1, 3]));
    assert_eq!((out, peak), (Ok(vec![1, 2, 3]), 3));
    assert_eq!(
        run(dedupe_then_upload(vec![1, 99, 3])).0,
        Err(Error("upload"))
    );
}

#[graph]
async fn take_then_write(
    mut pending: std::collections::BTreeMap<u32, u32>,
    tables: Vec<u32>,
) -> Result<u32, Error> {
    let mut written = 0;
    for table in tables {
        let Some(batch) = pending.remove(&table) else {
            continue;
        };
        if batch == 0 {
            return Err(Error("empty batch"));
        }
        written += io(T, batch).await;
    }
    Ok(written)
}

#[test]
fn fission_defers_exits_until_earlier_items_finish() {
    let pending = |v: &[(u32, u32)]| {
        v.iter()
            .copied()
            .collect::<std::collections::BTreeMap<_, _>>()
    };
    let (out, _, peak) = run(take_then_write(
        pending(&[(1, 10), (2, 20), (3, 30)]),
        vec![1, 4, 2, 3],
    ));
    assert_eq!((out, peak), (Ok(60), 3));
    assert_eq!(
        run(take_then_write(pending(&[(1, 10), (2, 0)]), vec![1, 2])).0,
        Err(Error("empty batch"))
    );
}

// sqlx's `try_min_connections`: reserve sequentially, connect concurrently.
#[graph]
async fn fill_pool(min: usize) -> Vec<u32> {
    let mut size = 0;
    let mut idle = Vec::new();
    #[psync::independent]
    while size < min {
        let guard = size as u32;
        size += 1;
        idle.push(io(T, guard).await);
    }
    idle
}

#[test]
fn while_loops_split_with_a_hint() {
    let (idle, _, peak) = run(fill_pool(3));
    assert_eq!((idle, peak), (vec![0, 1, 2], 3));
}

#[graph]
async fn scoped_lock(buffers: Vec<std::rc::Rc<RefCell<Vec<u32>>>>) -> u32 {
    let mut total = 0;
    for buffer in buffers {
        let taken = {
            let mut guard = lock_buffer(&buffer).await;
            std::mem::take(&mut *guard)
        };
        total += io(T, taken.iter().sum::<u32>()).await;
    }
    total
}
async fn lock_buffer(b: &RefCell<Vec<u32>>) -> std::cell::RefMut<'_, Vec<u32>> {
    io(T, ()).await;
    b.borrow_mut()
}

#[test]
fn critical_sections_inside_a_statement_may_move() {
    let buffers = (1..=3)
        .map(|i| std::rc::Rc::new(RefCell::new(vec![i, i])))
        .collect();
    let (total, _, peak) = run(scoped_lock(buffers));
    assert_eq!((total, peak), (12, 3));
}

// quickwit's delete planner: an early-`continue` guard between two per-item awaits.
#[graph]
async fn guard_then_search(splits: Vec<u32>) -> Result<(Vec<u32>, Vec<u32>), Error> {
    let mut with = Vec::new();
    let mut without = Vec::new();
    for split in splits {
        let tasks = io(T, if split % 3 == 0 { 0 } else { split }).await;
        if tasks == 0 {
            without.push(split);
            continue;
        }
        let matches = io(
            T,
            if split == 98 {
                Err(Error("leaf"))
            } else {
                Ok(split % 2 == 0)
            },
        )
        .await?;
        if matches {
            with.push(split);
        } else {
            without.push(split);
        }
    }
    Ok((with, without))
}

#[test]
fn exit_guards_let_later_awaits_run_ahead() {
    let (out, polls, peak) = run(guard_then_search((1..=6).collect()));
    assert_eq!(out, Ok((vec![2, 4], vec![1, 3, 5, 6])));
    assert_eq!(peak, 6);
    assert!(
        polls <= 2 * T as usize + 2,
        "two waits deep, got {polls} polls"
    );
    assert_eq!(run(guard_then_search(vec![1, 98])).0, Err(Error("leaf")));
}
