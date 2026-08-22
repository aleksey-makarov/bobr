//! The fetch request: what to download, where the store and the run live, and
//! how hard the network may be leaned on.
//!
//! This is deliberately not the build request. The contract between the fetcher
//! and the build is the store, not the request -- an object either carries the
//! declared hash or it does not -- so the two formats are free to differ, and
//! they do: the build needs the whole node graph, the fetcher needs a flat list
//! of sources and the connection limits.

use serde::Deserialize;
use std::collections::BTreeMap;
use std::path::PathBuf;

/// The one request schema this binary accepts, also printed by `--version` so
/// a wrapper can refuse a mismatched pair before lowering anything.
pub const FETCH_REQUEST_SCHEMA: &str = "bobr-fetch-request-v1";

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

/// A whole fetch request, as lowered from the recipes.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FetchRequest {
    /// Must be [`FETCH_REQUEST_SCHEMA`].
    pub schema: String,
    /// The store downloads are imported into.
    pub store: PathBuf,
    /// This run's log directory; must already exist.
    pub logs: PathBuf,
    /// This run's work directory; must already exist, on the store's
    /// filesystem.
    pub work: PathBuf,
    /// Name of this run, recorded in the objects it produces.
    pub run_id: String,
    /// Connection limits; every field optional.
    #[serde(default)]
    pub limits: Limits,
    /// Keep only warnings and errors on screen, as `bobr`'s own `quiet` does:
    /// the live block is dropped even on a terminal, and the routine chatter
    /// that replaces it off one goes too. The full record stays in the log
    /// either way. Carried in the request rather than as a flag because that is
    /// where `bobr` takes it from, and one profile drives both.
    #[serde(default)]
    pub quiet: Option<bool>,
    /// The sources to ensure present.
    pub sources: Vec<SourceEntry>,
}

/// Connection limits as the request states them; all optional.
#[derive(Debug, Default, Deserialize)]
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

/// One source to ensure present: its name, the hash the recipe declares, and
/// the origin object exactly as the recipes spell it.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SourceEntry {
    /// The source's name, used for refs and logs.
    pub name: String,
    /// The object hash the recipe declares.
    pub object_hash: String,
    /// The origin object exactly as the recipes spell it; absent for sources
    /// that must already be in the store.
    #[serde(default)]
    pub origin: Option<serde_json::Value>,
}

impl FetchRequest {
    /// Parses and validates a request; the schema line is checked first so a
    /// build request handed over by mistake fails with one clear sentence.
    pub fn parse_json(bytes: &[u8]) -> Result<Self, String> {
        let request: FetchRequest = serde_json::from_slice(bytes)
            .map_err(|error| format!("invalid fetch request: {error}"))?;
        if request.schema != FETCH_REQUEST_SCHEMA {
            return Err(format!(
                "unsupported request schema '{}'; this bobr-fetch accepts {FETCH_REQUEST_SCHEMA}",
                request.schema
            ));
        }
        Ok(request)
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

    fn minimal(schema: &str) -> String {
        format!(
            r#"{{"schema":"{schema}","store":"/s","logs":"/l","work":"/w","run_id":"r1","sources":[]}}"#
        )
    }

    #[test]
    fn parses_a_minimal_request() {
        let request = FetchRequest::parse_json(minimal(FETCH_REQUEST_SCHEMA).as_bytes()).unwrap();
        assert_eq!(request.run_id, "r1");
        assert!(request.sources.is_empty());
        assert!(request.limits.max_connections.is_none());
    }

    #[test]
    fn rejects_a_foreign_schema_by_name() {
        // The likeliest mistake is handing over the build request; the error
        // must name the schema, not drown in field-level noise.
        let error = FetchRequest::parse_json(minimal("bobr-request-v2").as_bytes()).unwrap_err();
        assert!(error.contains("bobr-request-v2"), "{error}");
        assert!(error.contains(FETCH_REQUEST_SCHEMA), "{error}");
    }

    #[test]
    fn rejects_unknown_fields() {
        let error = FetchRequest::parse_json(
            br#"{"schema":"bobr-fetch-request-v1","store":"/s","logs":"/l","work":"/w","run_id":"r","sources":[],"jobs":4}"#,
        )
        .unwrap_err();
        assert!(error.contains("jobs"), "{error}");
    }

    #[test]
    fn quiet_is_optional_and_defaults_to_speaking() {
        let request = FetchRequest::parse_json(minimal(FETCH_REQUEST_SCHEMA).as_bytes()).unwrap();
        assert_eq!(request.quiet, None);
        let loud = FetchRequest::parse_json(
            br#"{"schema":"bobr-fetch-request-v1","store":"/s","logs":"/l","work":"/w","run_id":"r","sources":[],"quiet":true}"#,
        )
        .unwrap();
        assert_eq!(loud.quiet, Some(true));
    }

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
        // Optional, and absent from what the recipes lower today: a request
        // written before this field existed still parses, and gets the default.
        let request = FetchRequest::parse_json(
            br#"{"schema":"bobr-fetch-request-v1","store":"/s","logs":"/l","work":"/w","run_id":"r","sources":[],"limits":{"max_local_jobs":1}}"#,
        )
        .unwrap();
        assert_eq!(
            ResolvedLimits::from_request(&request.limits).max_local_jobs,
            1
        );
    }

    #[test]
    fn total_cap_follows_the_descriptor_limit() {
        assert_eq!(default_max_connections(Some(1024)), 64);
        assert_eq!(default_max_connections(Some(120)), 30);
        assert_eq!(default_max_connections(Some(0)), 1);
        assert_eq!(default_max_connections(None), MAX_CONNECTIONS_CAP);
    }
}
