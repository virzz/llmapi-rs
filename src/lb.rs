//! Round-robin scheduling for `type = "lb"` providers.
//!
//! The state is immutable after it is built from one config version except for
//! the per-group cursor, which is a single atomic increment. Network requests
//! are never performed while holding any shared state.

use std::{
    collections::BTreeMap,
    sync::atomic::{AtomicUsize, Ordering},
    time::Duration,
};

use axum::http::StatusCode;

use crate::config::Config;

/// Upper bound for waiting on upstream response headers per member attempt.
pub const ATTEMPT_HEADER_TIMEOUT: Duration = Duration::from_secs(30);

#[derive(Debug)]
struct Pool {
    members: Vec<String>,
    cursor: AtomicUsize,
}

/// Concurrency-safe cursors for every load-balancer group in one config version.
#[derive(Debug, Default)]
pub struct State {
    pools: BTreeMap<String, Pool>,
}

impl State {
    pub fn new(config: &Config) -> Self {
        let pools = config
            .providers
            .iter()
            .filter_map(|(name, provider)| {
                let lb = provider.as_lb()?;
                Some((
                    name.clone(),
                    Pool {
                        members: lb.members.clone(),
                        cursor: AtomicUsize::new(0),
                    },
                ))
            })
            .collect();
        Self { pools }
    }

    /// Ordered members for the next request, starting at the shared cursor.
    ///
    /// Every request claims its starting position exactly once; failures within
    /// that request never advance the cursor again.
    pub fn candidates(&self, name: &str) -> Option<Vec<String>> {
        let pool = self.pools.get(name)?;
        if pool.members.is_empty() {
            return None;
        }
        let start = pool.cursor.fetch_add(1, Ordering::Relaxed) % pool.members.len();
        Some(
            pool.members
                .iter()
                .cycle()
                .skip(start)
                .take(pool.members.len())
                .cloned()
                .collect(),
        )
    }
}

/// Upstream statuses that make the selected member unavailable for this request.
pub fn is_retryable_status(status: StatusCode) -> bool {
    matches!(status.as_u16(), 401 | 403 | 408 | 429 | 500..=599)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{Protocol, ProviderConfig};

    fn config(groups: &[(&str, &[&str])]) -> Config {
        let mut config = Config::default();
        let mut names = Vec::new();
        for (lb, members) in groups {
            for member in *members {
                if !names.contains(member) {
                    names.push(*member);
                    config.providers.insert(
                        (*member).into(),
                        ProviderConfig::upstream(
                            Protocol::OpenAiChat,
                            format!("https://{member}.test"),
                            None,
                        ),
                    );
                }
            }
            config.providers.insert(
                (*lb).into(),
                ProviderConfig::lb(members.iter().map(|m| (*m).to_string()).collect()),
            );
        }
        config
    }

    #[test]
    fn rotates_starting_member_per_request() {
        let state = State::new(&config(&[("pool", &["a", "b", "c"])]));
        for expected in [
            ["a", "b", "c"],
            ["b", "c", "a"],
            ["c", "a", "b"],
            ["a", "b", "c"],
        ] {
            assert_eq!(state.candidates("pool").unwrap(), expected);
        }
    }

    #[test]
    fn groups_have_independent_cursors() {
        let state = State::new(&config(&[("one", &["a", "b"]), ("two", &["c", "d"])]));
        assert_eq!(state.candidates("one").unwrap(), ["a", "b"]);
        assert_eq!(state.candidates("two").unwrap(), ["c", "d"]);
        assert_eq!(state.candidates("one").unwrap(), ["b", "a"]);
        assert_eq!(state.candidates("two").unwrap(), ["d", "c"]);
        assert_eq!(state.candidates("missing"), None);
    }

    #[test]
    fn claims_start_position_once_per_request() {
        let state = State::new(&config(&[("pool", &["a", "b", "c"])]));
        let candidates = state.candidates("pool").unwrap();
        assert_eq!(candidates, ["a", "b", "c"]);
        assert_eq!(state.candidates("pool").unwrap()[0], "b");
        // A failed request does not push the cursor again.
        assert_eq!(state.candidates("pool").unwrap()[0], "c");
    }

    #[test]
    fn concurrent_requests_spread_members_within_one() {
        let state = State::new(&config(&[("pool", &["a", "b", "c"])]));
        let starts = std::thread::scope(|scope| {
            let handles: Vec<_> = (0..64)
                .map(|_| scope.spawn(|| state.candidates("pool").unwrap()[0].clone()))
                .collect();
            handles
                .into_iter()
                .map(|handle| handle.join().unwrap())
                .collect::<Vec<_>>()
        });
        let mut counts = BTreeMap::new();
        for member in starts {
            *counts.entry(member).or_insert(0usize) += 1;
        }
        for member in ["a", "b", "c"] {
            assert_eq!(counts[member], 64 / 3 + usize::from(member == "a"));
        }
    }

    #[test]
    fn classifies_switchable_statuses() {
        for status in [401, 403, 408, 429, 500, 502, 503, 599] {
            assert!(
                is_retryable_status(StatusCode::from_u16(status).unwrap()),
                "{status}"
            );
        }
        for status in [200, 204, 400, 404, 409, 422] {
            assert!(
                !is_retryable_status(StatusCode::from_u16(status).unwrap()),
                "{status}"
            );
        }
    }
}
