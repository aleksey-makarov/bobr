//! Concurrency limits for Source acquisition.

use serde::Deserialize;
use std::collections::BTreeMap;

/// How many downloads one host is asked to serve at once, unless the request
/// says otherwise. Browsers hold six HTTP/1.1 connections per host and servers
/// are tuned for that; measurement agrees -- of 33 simultaneous fetches from
/// one GNU mirror, the first handful succeeded and everything later was cut.
pub(crate) const DEFAULT_PER_HOST: u32 = 6;

/// Ceiling for the derived total-connection limit.
pub(crate) const MAX_CONNECTIONS_CAP: u32 = 64;

/// How many local sources are read, hashed and copied at once, unless the
/// request says otherwise.
///
/// Unlike the connection limits this one is about a disk, and no default can be
/// right for every disk: a spindle wants one (concurrent walks turn sequential
/// reads into seeks and finish slower than doing them in turn), an NVMe wants
/// many. Four is the compromise -- enough to keep hashing busy while the next
/// read is in flight on anything solid-state, few enough that a spindle
/// degrades gracefully rather than thrashing.
pub(crate) const DEFAULT_MAX_LOCAL_JOBS: u32 = 4;

/// Connection limits as the request states them; all optional.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Limits {
    /// Downloads one host serves at once, where `per_host` is silent.
    pub per_host_default: Option<u32>,
    /// Per-host overrides, keyed by host name.
    #[serde(default)]
    pub per_host: BTreeMap<String, u32>,
    /// Cap on downloads in flight across all hosts.
    pub max_connections: Option<u32>,
    /// Cap on local sources being materialized at once. Nothing to do with the
    /// connection limits -- this one bounds a disk, not a network -- so it gets
    /// its own number rather than sharing theirs.
    pub max_local_jobs: Option<u32>,
}

impl Limits {
    pub(crate) fn resolved_max_local_jobs(&self) -> usize {
        self.max_local_jobs.unwrap_or(DEFAULT_MAX_LOCAL_JOBS).max(1) as usize
    }
}

/// The limits with every default filled in, ready for the engine.
#[derive(Debug, Clone)]
pub(crate) struct ResolvedLimits {
    pub(crate) per_host_default: u32,
    pub(crate) per_host: BTreeMap<String, u32>,
    pub(crate) max_connections: u32,
    pub(crate) max_local_jobs: u32,
}

impl ResolvedLimits {
    pub(crate) fn from_request(limits: &Limits) -> Self {
        Self {
            per_host_default: limits.per_host_default.unwrap_or(DEFAULT_PER_HOST).max(1),
            per_host: limits.per_host.clone(),
            max_connections: limits
                .max_connections
                .unwrap_or_else(|| default_max_connections(nofile_limit()))
                .max(1),
            max_local_jobs: limits
                .max_local_jobs
                .unwrap_or(DEFAULT_MAX_LOCAL_JOBS)
                .max(1),
        }
    }

    pub(crate) fn for_host(&self, host: &str) -> u32 {
        self.per_host
            .get(host)
            .copied()
            .unwrap_or(self.per_host_default)
            .max(1)
    }
}

/// The default total cap comes from the file-descriptor limit, not politeness:
/// every download in flight is a socket plus a file, and starting six hundred
/// at once against a 1024-descriptor limit is EMFILE, not throughput. A
/// quarter of the limit leaves room for logs, the store, and everything else
/// the process holds open.
fn default_max_connections(nofile: Option<u64>) -> u32 {
    let quarter = nofile
        .map(|n| n / 4)
        .unwrap_or(u64::from(MAX_CONNECTIONS_CAP));
    quarter.clamp(1, u64::from(MAX_CONNECTIONS_CAP)) as u32
}

fn nofile_limit() -> Option<u64> {
    let mut limit = libc::rlimit {
        rlim_cur: 0,
        rlim_max: 0,
    };
    // SAFETY: getrlimit writes into the struct we hand it and touches nothing
    // else; a failure leaves it untouched and is reported by the return value.
    let rc = unsafe { libc::getrlimit(libc::RLIMIT_NOFILE, &mut limit) };
    if rc == 0 {
        // rlim_t is u64 on Linux, but the cast is load-bearing on other libc
        // definitions; silence the lint locally rather than assume.
        #[allow(clippy::unnecessary_cast)]
        Some(limit.rlim_cur as u64)
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn limits_fill_defaults_and_overrides() {
        let limits = Limits {
            per_host_default: None,
            per_host: BTreeMap::from([("ftp.gnu.org".to_string(), 3)]),
            max_connections: Some(10),
            max_local_jobs: None,
        };
        let resolved = ResolvedLimits::from_request(&limits);
        assert_eq!(resolved.per_host_default, DEFAULT_PER_HOST);
        assert_eq!(resolved.for_host("ftp.gnu.org"), 3);
        assert_eq!(resolved.for_host("crates.io"), DEFAULT_PER_HOST);
        assert_eq!(resolved.max_connections, 10);
        assert_eq!(resolved.max_local_jobs, DEFAULT_MAX_LOCAL_JOBS);
    }

    #[test]
    fn local_jobs_are_taken_from_the_request_when_it_says() {
        let limits = Limits {
            max_local_jobs: Some(1),
            ..Default::default()
        };
        assert_eq!(ResolvedLimits::from_request(&limits).max_local_jobs, 1);
    }

    #[test]
    fn total_cap_follows_the_descriptor_limit() {
        assert_eq!(default_max_connections(Some(1024)), 64);
        assert_eq!(default_max_connections(Some(120)), 30);
        assert_eq!(default_max_connections(Some(0)), 1);
        assert_eq!(default_max_connections(None), MAX_CONNECTIONS_CAP);
    }
}
