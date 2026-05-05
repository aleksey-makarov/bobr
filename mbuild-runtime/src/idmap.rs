use crate::IdmapError;
use mbuild_core::{FsTreeObjectError, FsTreeOwnerMap};
use nix::unistd::{Uid, User, getegid, geteuid};
use std::fs;
use std::path::Path;
use std::sync::{Arc, OnceLock};

const SUBUID_PATH: &str = "/etc/subuid";
const SUBGID_PATH: &str = "/etc/subgid";

static HOST_IDMAP: OnceLock<Result<Arc<MbuildIdmap>, Arc<IdmapError>>> = OnceLock::new();

#[derive(Debug, Clone)]
pub struct MbuildIdmap {
    current_uid: u32,
    current_gid: u32,
    subuid: SubidRange,
    subgid: SubidRange,
}

impl MbuildIdmap {
    #[cfg(test)]
    pub(crate) fn for_tests(
        current_uid: u32,
        current_gid: u32,
        subuid_base: u32,
        subuid_count: u32,
        subgid_base: u32,
        subgid_count: u32,
    ) -> Self {
        Self {
            current_uid,
            current_gid,
            subuid: SubidRange {
                base: subuid_base,
                count: subuid_count,
            },
            subgid: SubidRange {
                base: subgid_base,
                count: subgid_count,
            },
        }
    }

    pub fn from_host_environment() -> Result<Self, IdmapError> {
        let current_uid = geteuid().as_raw();
        let current_gid = getegid().as_raw();
        let username = current_username(current_uid)?;
        let subuid = read_first_subid_range(Path::new(SUBUID_PATH), &username, SubidKind::Uid)?;
        let subgid = read_first_subid_range(Path::new(SUBGID_PATH), &username, SubidKind::Gid)?;

        Ok(Self {
            current_uid,
            current_gid,
            subuid,
            subgid,
        })
    }

    pub fn current_uid(&self) -> u32 {
        self.current_uid
    }

    pub fn current_gid(&self) -> u32 {
        self.current_gid
    }

    pub fn subuid_base(&self) -> u32 {
        self.subuid.base
    }

    pub fn subuid_count(&self) -> u32 {
        self.subuid.count
    }

    pub fn subgid_base(&self) -> u32 {
        self.subgid.base
    }

    pub fn subgid_count(&self) -> u32 {
        self.subgid.count
    }

    pub fn physical_uid(&self, logical: u32) -> Result<u32, IdmapError> {
        translate_logical_id(logical, self.current_uid, self.subuid, LogicalIdKind::Uid)
    }

    pub fn physical_gid(&self, logical: u32) -> Result<u32, IdmapError> {
        translate_logical_id(logical, self.current_gid, self.subgid, LogicalIdKind::Gid)
    }
}

impl FsTreeOwnerMap for MbuildIdmap {
    fn physical_uid(&self, logical_uid: u32) -> Result<u32, FsTreeObjectError> {
        MbuildIdmap::physical_uid(self, logical_uid)
            .map_err(|error| FsTreeObjectError::Invalid(error.to_string()))
    }

    fn physical_gid(&self, logical_gid: u32) -> Result<u32, FsTreeObjectError> {
        MbuildIdmap::physical_gid(self, logical_gid)
            .map_err(|error| FsTreeObjectError::Invalid(error.to_string()))
    }
}

pub fn cached_host_idmap() -> Result<Arc<MbuildIdmap>, Arc<IdmapError>> {
    let result = HOST_IDMAP.get_or_init(|| {
        MbuildIdmap::from_host_environment()
            .map(Arc::new)
            .map_err(Arc::new)
    });
    match result {
        Ok(idmap) => Ok(Arc::clone(idmap)),
        Err(error) => Err(Arc::clone(error)),
    }
}

fn current_username(current_uid: u32) -> Result<String, IdmapError> {
    let user = User::from_uid(Uid::from_raw(current_uid)).map_err(|error| {
        IdmapError::CurrentUser(format!(
            "failed to look up passwd entry for euid {current_uid}: {error}"
        ))
    })?;
    user.map(|user| user.name).ok_or_else(|| {
        IdmapError::CurrentUser(format!("current euid {current_uid} has no passwd entry"))
    })
}

fn read_first_subid_range(
    path: &Path,
    username: &str,
    kind: SubidKind,
) -> Result<SubidRange, IdmapError> {
    let content = fs::read_to_string(path).map_err(|source| IdmapError::SubidFileRead {
        kind: kind.subid_name(),
        path: path.to_path_buf(),
        source,
    })?;
    parse_first_subid_range(&content, username, kind, &path.display().to_string())
}

fn parse_first_subid_range(
    content: &str,
    username: &str,
    kind: SubidKind,
    source: &str,
) -> Result<SubidRange, IdmapError> {
    let mut first_match = None;

    for (index, line) in content.lines().enumerate() {
        let line_number = index + 1;
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }

        let parts = line.split(':').collect::<Vec<_>>();
        if parts.len() != 3 {
            return Err(IdmapError::MalformedSubidLine {
                kind: kind.subid_name(),
                source_label: source.to_string(),
                line: line_number,
                message: "expected <username>:<base>:<count>".to_string(),
            });
        }

        let base = parse_u32_field(parts[1], "base", kind, source, line_number)?;
        let count = parse_u32_field(parts[2], "count", kind, source, line_number)?;
        if count == 0 {
            return Err(IdmapError::ZeroSubidCount {
                kind: kind.subid_name(),
                source_label: source.to_string(),
                line: line_number,
            });
        }
        base.checked_add(count - 1)
            .ok_or_else(|| IdmapError::SubidRangeOverflow {
                kind: kind.subid_name(),
                source_label: source.to_string(),
                line: line_number,
                base,
                count,
            })?;

        if parts[0] == username && first_match.is_none() {
            first_match = Some(SubidRange { base, count });
        }
    }

    first_match.ok_or_else(|| IdmapError::MissingSubidRange {
        kind: kind.subid_name(),
        username: username.to_string(),
        path: source.to_string(),
    })
}

fn parse_u32_field(
    value: &str,
    field: &str,
    kind: SubidKind,
    source: &str,
    line: usize,
) -> Result<u32, IdmapError> {
    value
        .parse::<u32>()
        .map_err(|error| IdmapError::MalformedSubidLine {
            kind: kind.subid_name(),
            source_label: source.to_string(),
            line,
            message: format!("invalid {field} '{value}': {error}"),
        })
}

fn translate_logical_id(
    logical: u32,
    current: u32,
    range: SubidRange,
    kind: LogicalIdKind,
) -> Result<u32, IdmapError> {
    if logical == 0 {
        return Ok(current);
    }
    if logical > range.count {
        return Err(IdmapError::OutOfRange {
            kind: kind.logical_name(),
            subid_kind: kind.subid_name(),
            logical,
            count: range.count,
        });
    }
    Ok(range.base + logical - 1)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct SubidRange {
    base: u32,
    count: u32,
}

#[derive(Debug, Clone, Copy)]
enum SubidKind {
    Uid,
    Gid,
}

impl SubidKind {
    fn subid_name(self) -> &'static str {
        match self {
            Self::Uid => "subuid",
            Self::Gid => "subgid",
        }
    }
}

#[derive(Debug, Clone, Copy)]
enum LogicalIdKind {
    Uid,
    Gid,
}

impl LogicalIdKind {
    fn logical_name(self) -> &'static str {
        match self {
            Self::Uid => "uid",
            Self::Gid => "gid",
        }
    }

    fn subid_name(self) -> &'static str {
        match self {
            Self::Uid => "subuid",
            Self::Gid => "subgid",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_idmap() -> MbuildIdmap {
        MbuildIdmap {
            current_uid: 1000,
            current_gid: 1001,
            subuid: SubidRange {
                base: 100_000,
                count: 3,
            },
            subgid: SubidRange {
                base: 200_000,
                count: 4,
            },
        }
    }

    #[test]
    fn parser_skips_comments_and_empty_lines() {
        let range = parse_first_subid_range(
            "\n# comment\nalice:100000:65536\n",
            "alice",
            SubidKind::Uid,
            "/etc/subuid",
        )
        .unwrap();

        assert_eq!(
            range,
            SubidRange {
                base: 100_000,
                count: 65_536
            }
        );
    }

    #[test]
    fn parser_uses_first_matching_entry() {
        let range = parse_first_subid_range(
            "alice:100000:10\nalice:200000:20\n",
            "alice",
            SubidKind::Uid,
            "/etc/subuid",
        )
        .unwrap();

        assert_eq!(
            range,
            SubidRange {
                base: 100_000,
                count: 10
            }
        );
    }

    #[test]
    fn parser_rejects_missing_entry() {
        let error =
            parse_first_subid_range("bob:100000:10\n", "alice", SubidKind::Uid, "/etc/subuid")
                .unwrap_err();

        assert!(
            error
                .to_string()
                .contains("subuid not configured for user alice")
        );
    }

    #[test]
    fn parser_rejects_malformed_line_anywhere() {
        let error = parse_first_subid_range(
            "alice:100000:10\nmalformed\n",
            "alice",
            SubidKind::Uid,
            "/etc/subuid",
        )
        .unwrap_err();

        assert!(error.to_string().contains("malformed subuid line 2"));
    }

    #[test]
    fn parser_rejects_invalid_number_anywhere() {
        let error = parse_first_subid_range(
            "alice:100000:10\nbob:not-a-number:10\n",
            "alice",
            SubidKind::Uid,
            "/etc/subuid",
        )
        .unwrap_err();

        assert!(error.to_string().contains("invalid base 'not-a-number'"));
    }

    #[test]
    fn parser_rejects_zero_count() {
        let error =
            parse_first_subid_range("alice:100000:0\n", "alice", SubidKind::Uid, "/etc/subuid")
                .unwrap_err();

        assert!(error.to_string().contains("has zero count"));
    }

    #[test]
    fn parser_rejects_range_overflow() {
        let error = parse_first_subid_range(
            "alice:4294967295:2\n",
            "alice",
            SubidKind::Uid,
            "/etc/subuid",
        )
        .unwrap_err();

        assert!(error.to_string().contains("overflows u32 range"));
    }

    #[test]
    fn physical_uid_translation_uses_current_user_and_subuid_range() {
        let idmap = test_idmap();

        assert_eq!(idmap.physical_uid(0).unwrap(), 1000);
        assert_eq!(idmap.physical_uid(1).unwrap(), 100_000);
        assert_eq!(idmap.physical_uid(3).unwrap(), 100_002);

        let error = idmap.physical_uid(4).unwrap_err();
        assert_eq!(
            error.to_string(),
            "logical uid 4 exceeds subuid range size 3"
        );
    }

    #[test]
    fn physical_gid_translation_uses_current_group_and_subgid_range() {
        let idmap = test_idmap();

        assert_eq!(idmap.physical_gid(0).unwrap(), 1001);
        assert_eq!(idmap.physical_gid(1).unwrap(), 200_000);
        assert_eq!(idmap.physical_gid(4).unwrap(), 200_003);

        let error = idmap.physical_gid(5).unwrap_err();
        assert_eq!(
            error.to_string(),
            "logical gid 5 exceeds subgid range size 4"
        );
    }

    #[test]
    fn fs_tree_owner_map_converts_out_of_range_to_invalid_object_error() {
        let idmap = test_idmap();

        assert_eq!(
            FsTreeOwnerMap::physical_uid(&idmap, 1).unwrap(),
            idmap.subuid_base()
        );

        let error = FsTreeOwnerMap::physical_uid(&idmap, 4).unwrap_err();
        assert!(matches!(error, FsTreeObjectError::Invalid(_)));
        assert!(
            error
                .to_string()
                .contains("logical uid 4 exceeds subuid range size 3")
        );
    }
}
