//! OCI spec construction for ownership materialization.

use crate::{MbuildIdmap, RuntimeError};
use libcontainer::oci_spec::runtime::{
    Capabilities, Capability, LinuxBuilder, LinuxCapabilities, LinuxCapabilitiesBuilder,
    LinuxIdMapping, LinuxIdMappingBuilder, LinuxNamespaceBuilder, LinuxNamespaceType,
    LinuxResources, MountBuilder, ProcessBuilder, RootBuilder, Spec, SpecBuilder, UserBuilder,
};
use std::fmt::Display;
use std::path::Path;

pub fn build_ownership_spec(
    idmap: &MbuildIdmap,
    target_host_path: &Path,
) -> Result<Spec, RuntimeError> {
    let uid_mappings = vec![
        linux_id_mapping(0, idmap.current_uid(), 1)?,
        linux_id_mapping(1, idmap.subuid_base(), idmap.subuid_count())?,
    ];
    let gid_mappings = vec![
        linux_id_mapping(0, idmap.current_gid(), 1)?,
        linux_id_mapping(1, idmap.subgid_base(), idmap.subgid_count())?,
    ];

    let linux = build_oci(
        LinuxBuilder::default()
            .namespaces(vec![
                build_oci(
                    LinuxNamespaceBuilder::default()
                        .typ(LinuxNamespaceType::User)
                        .build(),
                )?,
                build_oci(
                    LinuxNamespaceBuilder::default()
                        .typ(LinuxNamespaceType::Mount)
                        .build(),
                )?,
                build_oci(
                    LinuxNamespaceBuilder::default()
                        .typ(LinuxNamespaceType::Pid)
                        .build(),
                )?,
            ])
            .uid_mappings(uid_mappings)
            .gid_mappings(gid_mappings)
            .resources(LinuxResources::default())
            .masked_paths(Vec::<String>::new())
            .readonly_paths(Vec::<String>::new())
            .build(),
    )?;

    build_oci(
        SpecBuilder::default()
            .version("1.0.2")
            .root(build_oci(
                RootBuilder::default()
                    .path("rootfs")
                    .readonly(false)
                    .build(),
            )?)
            .process(build_oci(
                ProcessBuilder::default()
                    .terminal(false)
                    .user(build_oci(
                        UserBuilder::default().uid(0_u32).gid(0_u32).build(),
                    )?)
                    .args(vec!["/dev/null".to_string()])
                    .cwd("/")
                    .capabilities(helper_capabilities()?)
                    .no_new_privileges(false)
                    .build(),
            )?)
            .mounts(vec![
                build_oci(
                    MountBuilder::default()
                        .destination("/target")
                        .typ("bind")
                        .source(target_host_path)
                        .options(vec!["rbind".to_string(), "rw".to_string()])
                        .build(),
                )?,
                build_oci(
                    MountBuilder::default()
                        .destination("/proc")
                        .typ("proc")
                        .source("proc")
                        .build(),
                )?,
            ])
            .linux(linux)
            .build(),
    )
}

fn linux_id_mapping(
    container_id: u32,
    host_id: u32,
    size: u32,
) -> Result<LinuxIdMapping, RuntimeError> {
    build_oci(
        LinuxIdMappingBuilder::default()
            .container_id(container_id)
            .host_id(host_id)
            .size(size)
            .build(),
    )
}

fn helper_capabilities() -> Result<LinuxCapabilities, RuntimeError> {
    let caps = [
        Capability::Chown,
        Capability::DacOverride,
        Capability::Fowner,
        Capability::Fsetid,
    ]
    .into_iter()
    .collect::<Capabilities>();

    build_oci(
        LinuxCapabilitiesBuilder::default()
            .bounding(caps.clone())
            .effective(caps.clone())
            .inheritable(caps.clone())
            .permitted(caps.clone())
            .ambient(caps)
            .build(),
    )
}

fn build_oci<T>(result: Result<T, impl Display>) -> Result<T, RuntimeError> {
    result.map_err(|error| RuntimeError::Libcontainer(error.to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use libcontainer::oci_spec::runtime::LinuxNamespaceType;
    use std::path::Path;

    #[test]
    fn ownership_spec_serializes() {
        let spec = test_spec();
        serde_json::to_string(&spec).expect("spec should serialize");
    }

    #[test]
    fn ownership_spec_sets_root() {
        let spec = test_spec();
        let root = spec.root().as_ref().expect("root should be set");

        assert_eq!(root.path(), Path::new("rootfs"));
        assert_eq!(root.readonly(), Some(false));
    }

    #[test]
    fn ownership_spec_sets_process() {
        let spec = test_spec();
        let process = spec.process().as_ref().expect("process should be set");

        assert_eq!(process.user().uid(), 0);
        assert_eq!(process.user().gid(), 0);
        assert_eq!(
            process.args().as_deref(),
            Some(&["/dev/null".to_string()][..])
        );
        assert_eq!(process.cwd(), Path::new("/"));
        assert_eq!(process.no_new_privileges(), Some(false));
        let capabilities = process
            .capabilities()
            .as_ref()
            .expect("capabilities should be set");
        assert_capability_set(capabilities.bounding().as_ref());
        assert_capability_set(capabilities.effective().as_ref());
        assert_capability_set(capabilities.inheritable().as_ref());
        assert_capability_set(capabilities.permitted().as_ref());
        assert_capability_set(capabilities.ambient().as_ref());
    }

    #[test]
    fn ownership_spec_sets_mounts() {
        let spec = test_spec();
        let mounts = spec.mounts().as_ref().expect("mounts should be set");
        assert_eq!(mounts.len(), 2);

        let target = &mounts[0];
        assert_eq!(target.destination(), Path::new("/target"));
        assert_eq!(target.typ().as_deref(), Some("bind"));
        assert_eq!(
            target.source().as_deref(),
            Some(Path::new("/tmp/mbuild-runtime-target"))
        );
        assert_eq!(
            target.options().as_deref(),
            Some(&["rbind".to_string(), "rw".to_string()][..])
        );

        let proc_mount = &mounts[1];
        assert_eq!(proc_mount.destination(), Path::new("/proc"));
        assert_eq!(proc_mount.typ().as_deref(), Some("proc"));
        assert_eq!(proc_mount.source().as_deref(), Some(Path::new("proc")));
        assert_eq!(proc_mount.options(), &None);
    }

    #[test]
    fn ownership_spec_sets_linux_namespaces() {
        let spec = test_spec();
        let linux = spec.linux().as_ref().expect("linux should be set");
        let namespaces = linux
            .namespaces()
            .as_ref()
            .expect("namespaces should be set");

        assert_eq!(namespaces.len(), 3);
        assert_eq!(namespaces[0].typ(), LinuxNamespaceType::User);
        assert_eq!(namespaces[1].typ(), LinuxNamespaceType::Mount);
        assert_eq!(namespaces[2].typ(), LinuxNamespaceType::Pid);
    }

    #[test]
    fn ownership_spec_sets_linux_id_mappings() {
        let idmap = test_idmap();
        let spec = build_ownership_spec(&idmap, Path::new("/tmp/mbuild-runtime-target")).unwrap();
        let linux = spec.linux().as_ref().expect("linux should be set");

        assert_id_mapping(
            linux.uid_mappings().as_ref().expect("uid mappings"),
            idmap.current_uid(),
            idmap.subuid_base(),
            idmap.subuid_count(),
        );
        assert_id_mapping(
            linux.gid_mappings().as_ref().expect("gid mappings"),
            idmap.current_gid(),
            idmap.subgid_base(),
            idmap.subgid_count(),
        );
    }

    #[test]
    fn ownership_spec_sets_linux_resources_and_paths() {
        let spec = test_spec();
        let linux = spec.linux().as_ref().expect("linux should be set");

        assert_eq!(linux.resources(), &Some(LinuxResources::default()));
        assert_eq!(linux.masked_paths().as_deref(), Some(&[][..]));
        assert_eq!(linux.readonly_paths().as_deref(), Some(&[][..]));
    }

    fn test_spec() -> Spec {
        build_ownership_spec(&test_idmap(), Path::new("/tmp/mbuild-runtime-target")).unwrap()
    }

    fn test_idmap() -> MbuildIdmap {
        MbuildIdmap::for_tests(1000, 1001, 100000, 65536, 200000, 65536)
    }

    fn assert_capability_set(caps: Option<&Capabilities>) {
        let expected = [
            Capability::Chown,
            Capability::DacOverride,
            Capability::Fowner,
            Capability::Fsetid,
        ]
        .into_iter()
        .collect::<Capabilities>();

        assert_eq!(caps, Some(&expected));
    }

    fn assert_id_mapping(
        mappings: &[LinuxIdMapping],
        current_host_id: u32,
        subid_base: u32,
        subid_count: u32,
    ) {
        assert_eq!(mappings.len(), 2);
        assert_eq!(mappings[0].container_id(), 0);
        assert_eq!(mappings[0].host_id(), current_host_id);
        assert_eq!(mappings[0].size(), 1);
        assert_eq!(mappings[1].container_id(), 1);
        assert_eq!(mappings[1].host_id(), subid_base);
        assert_eq!(mappings[1].size(), subid_count);
    }
}
