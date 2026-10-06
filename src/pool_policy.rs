//! The pool's decisions, free of V8, tokio, logging and the clock.
//!
//! `pool.rs` is the shell: it reports what it observes as a [`PoolLoad`] and
//! performs the [`Acquire`] it is told. Nothing here can fail or block, so
//! every rule is a property a test can state over any snapshot.

use std::time::Instant;

/// What the shell observes about one isolate.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct IsolateLoad<'a> {
    pub owner: &'a str,
    /// A request can take a slot on it now.
    pub has_capacity: bool,
    /// When its last request ended, if no request runs on it. `None` while
    /// a request runs, so an idle isolate is also an evictable one.
    pub idle_since: Option<Instant>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Limits {
    pub max_isolates: usize,
    /// `None` leaves an owner bounded by `max_isolates` alone.
    pub max_per_owner: Option<usize>,
    /// Build past `max_isolates` when every isolate is in use. Off, the pool
    /// refuses instead.
    pub overcommit: bool,
}

/// The pool as the shell sees it, indexed like its isolate list.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PoolLoad<'a> {
    pub isolates: Vec<IsolateLoad<'a>>,
    pub limits: Limits,
}

/// What to do for an owner that asks for an isolate.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Acquire {
    /// Take a slot on this isolate of the owner.
    Reuse(usize),
    /// Build one: the pool is under its ceiling.
    Build,
    /// Drop this idle isolate, then build one in its place.
    Evict(usize),
    /// Build one past the ceiling: every isolate is in use.
    Overcommit,
    Refuse(Refusal),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Refusal {
    OwnerAtLimit,
    /// Every isolate is in use and the pool must not overcommit.
    PoolFull,
}

impl Refusal {
    pub fn message(self) -> &'static str {
        match self {
            Refusal::OwnerAtLimit => "Pool at capacity: the owner is at its isolate limit",
            Refusal::PoolFull => "Pool at capacity: every isolate is in use",
        }
    }
}

/// Choose for `owner`. An isolate of the owner with capacity comes first; then
/// a new one under the ceiling; then the idle isolate unused for longest; then
/// overcommit, if allowed.
pub fn acquire(load: &PoolLoad, owner: &str) -> Acquire {
    let limits = load.limits;

    if let Some(index) = load
        .isolates
        .iter()
        .position(|isolate| isolate.owner == owner && isolate.has_capacity)
    {
        return Acquire::Reuse(index);
    }

    let owned = load
        .isolates
        .iter()
        .filter(|isolate| isolate.owner == owner)
        .count();

    if limits.max_per_owner.is_some_and(|limit| owned >= limit) {
        return Acquire::Refuse(Refusal::OwnerAtLimit);
    }

    if load.isolates.len() < limits.max_isolates {
        return Acquire::Build;
    }

    let idle = load
        .isolates
        .iter()
        .enumerate()
        .filter_map(|(index, isolate)| isolate.idle_since.map(|since| (index, since)))
        .min_by_key(|(_, since)| *since);

    match idle {
        Some((index, _)) => Acquire::Evict(index),
        None if limits.overcommit => Acquire::Overcommit,
        None => Acquire::Refuse(Refusal::PoolFull),
    }
}

/// The isolates to drop once the pool stands past its ceiling: the first idle
/// ones, in index order, as many as the excess allows.
pub fn reclaim(load: &PoolLoad) -> Vec<usize> {
    let excess = load.isolates.len().saturating_sub(load.limits.max_isolates);

    load.isolates
        .iter()
        .enumerate()
        .filter(|(_, isolate)| isolate.idle_since.is_some())
        .map(|(index, _)| index)
        .take(excess)
        .collect()
}

/// What a cached context was built for. A request reuses a context only
/// when all three match, because each one changes what the script sees.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ContextKey<'a> {
    pub worker_id: &'a str,
    pub version: i32,
    pub env_updated_at: Option<i64>,
}

/// The cached context to reuse for `wanted`: the first match that has not
/// served `max_reuses` requests. A context past that count is left to age out,
/// so a leak in a script cannot grow one context without bound.
pub fn warm_hit(
    cached: &[(ContextKey, u32)],
    wanted: ContextKey,
    max_reuses: u32,
) -> Option<usize> {
    cached
        .iter()
        .position(|(key, reuses)| *key == wanted && *reuses < max_reuses)
}

/// The cached context to drop before another is added: the one unused for
/// longest, and only when the cache is full.
pub fn cache_evict(last_used: &[Instant], max_cached: usize) -> Option<usize> {
    if last_used.len() < max_cached {
        return None;
    }

    last_used
        .iter()
        .enumerate()
        .min_by_key(|(_, used)| **used)
        .map(|(index, _)| index)
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;
    use std::time::Duration;

    /// Owners come from a small alphabet so a pool holds several of each.
    fn owner() -> impl Strategy<Value = &'static str> {
        prop_oneof![Just("a"), Just("b"), Just("c")]
    }

    fn isolate() -> impl Strategy<Value = (&'static str, bool, Option<u64>)> {
        (owner(), any::<bool>(), proptest::option::of(0u64..1000))
    }

    fn limits() -> impl Strategy<Value = Limits> {
        (1usize..6, proptest::option::of(1usize..4), any::<bool>()).prop_map(
            |(max_isolates, max_per_owner, overcommit)| Limits {
                max_isolates,
                max_per_owner,
                overcommit,
            },
        )
    }

    /// Builds a load from plain values; `idle` is an age in milliseconds.
    fn load(
        epoch: Instant,
        isolates: &[(&'static str, bool, Option<u64>)],
        limits: Limits,
    ) -> PoolLoad<'static> {
        PoolLoad {
            isolates: isolates
                .iter()
                .map(|(owner, has_capacity, idle)| IsolateLoad {
                    owner,
                    has_capacity: *has_capacity,
                    idle_since: idle.map(|ms| epoch + Duration::from_millis(ms)),
                })
                .collect(),
            limits,
        }
    }

    proptest! {
        #[test]
        fn reuse_names_an_isolate_of_the_owner_with_capacity(
            isolates in proptest::collection::vec(isolate(), 0..8),
            limits in limits(),
            owner in owner(),
        ) {
            let load = load(Instant::now(), &isolates, limits);

            if let Acquire::Reuse(index) = acquire(&load, owner) {
                prop_assert_eq!(load.isolates[index].owner, owner);
                prop_assert!(load.isolates[index].has_capacity);
            }
        }

        #[test]
        fn an_owner_at_its_limit_gets_no_new_isolate(
            isolates in proptest::collection::vec(isolate(), 0..8),
            limits in limits(),
            owner in owner(),
        ) {
            let load = load(Instant::now(), &isolates, limits);
            let owned = load.isolates.iter().filter(|i| i.owner == owner).count();
            let at_limit = limits.max_per_owner.is_some_and(|limit| owned >= limit);

            match acquire(&load, owner) {
                Acquire::Build | Acquire::Evict(_) | Acquire::Overcommit => {
                    prop_assert!(!at_limit)
                }
                Acquire::Refuse(Refusal::OwnerAtLimit) => prop_assert!(at_limit),
                Acquire::Reuse(_) | Acquire::Refuse(Refusal::PoolFull) => {}
            }
        }

        #[test]
        fn build_stays_under_the_ceiling(
            isolates in proptest::collection::vec(isolate(), 0..8),
            limits in limits(),
            owner in owner(),
        ) {
            let load = load(Instant::now(), &isolates, limits);

            if acquire(&load, owner) == Acquire::Build {
                prop_assert!(load.isolates.len() < limits.max_isolates);
            }
        }

        #[test]
        fn evict_names_the_idle_isolate_unused_for_longest(
            isolates in proptest::collection::vec(isolate(), 0..8),
            limits in limits(),
            owner in owner(),
        ) {
            let load = load(Instant::now(), &isolates, limits);

            if let Acquire::Evict(index) = acquire(&load, owner) {
                let since = load.isolates[index].idle_since;
                prop_assert!(since.is_some());
                prop_assert!(load.isolates.len() >= limits.max_isolates);

                for other in load.isolates.iter().filter_map(|i| i.idle_since) {
                    prop_assert!(since.unwrap() <= other);
                }
            }
        }

        #[test]
        fn overcommit_only_when_allowed_and_nothing_is_idle(
            isolates in proptest::collection::vec(isolate(), 0..8),
            limits in limits(),
            owner in owner(),
        ) {
            let load = load(Instant::now(), &isolates, limits);
            let any_idle = load.isolates.iter().any(|i| i.idle_since.is_some());

            match acquire(&load, owner) {
                Acquire::Overcommit => prop_assert!(limits.overcommit && !any_idle),
                Acquire::Refuse(Refusal::PoolFull) => {
                    prop_assert!(!limits.overcommit && !any_idle);
                    prop_assert!(load.isolates.len() >= limits.max_isolates);
                }
                _ => {}
            }
        }

        #[test]
        fn reclaim_drops_only_idle_isolates_down_to_the_ceiling(
            isolates in proptest::collection::vec(isolate(), 0..8),
            limits in limits(),
        ) {
            let load = load(Instant::now(), &isolates, limits);
            let dropped = reclaim(&load);
            let excess = load.isolates.len().saturating_sub(limits.max_isolates);

            prop_assert!(dropped.len() <= excess);
            prop_assert!(dropped.windows(2).all(|pair| pair[0] < pair[1]));

            for index in &dropped {
                prop_assert!(load.isolates[*index].idle_since.is_some());
            }

            let idle = load.isolates.iter().filter(|i| i.idle_since.is_some()).count();
            prop_assert_eq!(dropped.len(), excess.min(idle));
        }

        #[test]
        fn warm_hit_matches_the_whole_key_under_the_reuse_cap(
            cached in proptest::collection::vec((owner(), 0i32..3, proptest::option::of(0i64..3), 0u32..4), 0..6),
            worker in owner(),
            version in 0i32..3,
            env in proptest::option::of(0i64..3),
            max_reuses in 1u32..4,
        ) {
            let keys: Vec<(ContextKey, u32)> = cached
                .iter()
                .map(|(worker_id, version, env_updated_at, reuses)| {
                    (ContextKey { worker_id, version: *version, env_updated_at: *env_updated_at }, *reuses)
                })
                .collect();
            let wanted = ContextKey { worker_id: worker, version, env_updated_at: env };

            match warm_hit(&keys, wanted, max_reuses) {
                Some(index) => {
                    prop_assert_eq!(keys[index].0, wanted);
                    prop_assert!(keys[index].1 < max_reuses);
                }
                None => {
                    prop_assert!(!keys.iter().any(|(key, reuses)| *key == wanted && *reuses < max_reuses));
                }
            }
        }

        #[test]
        fn cache_evict_names_the_oldest_only_when_full(
            ages in proptest::collection::vec(0u64..1000, 0..6),
            max_cached in 1usize..6,
        ) {
            let epoch = Instant::now();
            let last_used: Vec<Instant> = ages.iter().map(|ms| epoch + Duration::from_millis(*ms)).collect();

            match cache_evict(&last_used, max_cached) {
                Some(index) => {
                    prop_assert!(last_used.len() >= max_cached);
                    prop_assert!(last_used.iter().all(|used| last_used[index] <= *used));
                }
                None => prop_assert!(last_used.len() < max_cached),
            }
        }
    }
}
