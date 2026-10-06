# Installation

## Prerequisites

- An x86-64 Linux host with Internet access and kernel OverlayFS support.
- Bash, `curl`, `tar`, `xz`, `awk`, `sed`, `jq`,
  [Nickel](https://nickel-lang.org/), GNU coreutils, and GNU findutils on `PATH`.
- A writable filesystem supporting hardlinks, Unix ownership/permissions, and
  extended attributes (xattrs).

Builds normally run as a regular user. This requires permitted unprivileged
user namespaces and sandbox mounts, `newuidmap` and `newgidmap` on `PATH` with
their distribution-provided privileges (`uidmap` / `shadow` packages), and
UID/GID ranges assigned to your account in `/etc/subuid` and `/etc/subgid`.

Alternatively, run Bobr through an already configured `podman unshare`
environment, or as root. Rootless Podman still needs working user namespaces
and UID/GID mappings; root does not need the mapping helpers.

The release is prebuilt: Rust, Cargo, and a host compiler are not required.

## Install bobr

The shortest installation command is:

```sh
curl -fsSL https://bobr.build/install.sh | bash
```

If you prefer to inspect code before running it, download the same script
first:

```sh
curl -fsSLo bobr-install.sh https://bobr.build/install.sh
less bobr-install.sh
bash bobr-install.sh
rm bobr-install.sh
```

The installer downloads the latest x86-64 Linux release, verifies its entry in
the release's `SHA256SUMS`, validates all four commands and their build
provenance, and only then replaces the installed files. By default it installs
static `bobr`, `bobr-repo`, `bobr-fsobj-hash`, and
`bobr-sandbox-launcher` binaries into `~/.local/bin`. It neither invokes
`sudo` nor edits shell startup files.

Use `--bin-dir` for one invocation, or `BOBR_INSTALL_DIR` for the environment:

```sh
bash bobr-install.sh --bin-dir /path/to/bin
BOBR_INSTALL_DIR=/path/to/bin bash bobr-install.sh
```

There is no installation database. To uninstall the default installation,
remove exactly the four installed commands:

```sh
rm -- \
  "${HOME}/.local/bin/bobr" \
  "${HOME}/.local/bin/bobr-repo" \
  "${HOME}/.local/bin/bobr-fsobj-hash" \
  "${HOME}/.local/bin/bobr-sandbox-launcher"
```

For a custom destination, use that directory instead.
