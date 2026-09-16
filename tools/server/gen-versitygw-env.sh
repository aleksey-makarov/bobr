#!/usr/bin/env bash
# Generate out/versitygw.env and out/aws-credentials relative to the current
# directory, with a fresh ROOT_ACCESS_KEY_ID / ROOT_SECRET_ACCESS_KEY pair for
# the versitygw systemd service and AWS clients.
#
# Usage:
#   ./gen-versitygw-env.sh [-f|--force]
#
# Both files contain the same credentials and have mode 0600. The script
# refuses to overwrite either file unless -f/--force is given.

set -euo pipefail

output_dir="$PWD/out"
versitygw_env="$output_dir/versitygw.env"
aws_credentials="$output_dir/aws-credentials"
force=0

for arg in "$@"; do
    case "$arg" in
        -f|--force)
            force=1
            ;;
        -h|--help)
            echo "Usage: $0 [-f|--force]" >&2
            exit 0
            ;;
        *)
            echo "error: unexpected argument: $arg" >&2
            echo "Usage: $0 [-f|--force]" >&2
            exit 2
            ;;
    esac
done

umask 077
install -d -m 0700 "$output_dir"

if [[ "$force" -ne 1 ]]; then
    for output in "$versitygw_env" "$aws_credentials"; do
        if [[ -e "$output" ]]; then
            echo "error: $output already exists, pass -f/--force to overwrite" >&2
            exit 1
        fi
    done
fi

openssl_bin="$(command -v openssl || true)"
if [[ -n "$openssl_bin" ]]; then
    openssl() { "$openssl_bin" "$@"; }
else
    # Fetch OpenSSL from nixpkgs when it isn't installed system-wide.
    nix_bin="$(command -v nix || true)"
    nix_bin="${nix_bin:-/run/current-system/sw/bin/nix}"
    openssl() { "$nix_bin" run nixpkgs#openssl -- "$@"; }
fi

# Access key: SigV4 splits the Authorization header's Credential field on
# '/' and ',' and '=', so keep it plain alphanumeric (AWS-style).
access_key="AKIA$(openssl rand -hex 8 | tr 'a-f' 'A-F')"
secret_key="$(openssl rand -hex 32)"

cat > "$versitygw_env" <<EOF
ROOT_ACCESS_KEY_ID=$access_key
ROOT_SECRET_ACCESS_KEY=$secret_key
EOF

cat > "$aws_credentials" <<EOF
[bobr-potato]
aws_access_key_id = $access_key
aws_secret_access_key = $secret_key
EOF

chmod 600 "$versitygw_env" "$aws_credentials"

echo "Wrote $versitygw_env (mode 600)"
echo "Wrote $aws_credentials (mode 600, profile bobr-potato)"
