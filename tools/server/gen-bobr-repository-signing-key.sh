#!/usr/bin/env bash
# Create an Ed25519 signing key and its public key for a Bobr remote repository
# under out/ in the current directory.
#
# Usage:
#   ./gen-bobr-repository-signing-key.sh [-f|--force]

set -euo pipefail

output_dir="$PWD/out"
private_key="$output_dir/bobr-repository-signing.key.pem"
public_key="$output_dir/bobr-repository-signing.pub.pem"
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
    for output in "$private_key" "$public_key"; do
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

openssl genpkey \
    -algorithm Ed25519 \
    -out "$private_key"

openssl pkey \
    -in "$private_key" \
    -pubout \
    -out "$public_key"

openssl pkey -in "$private_key" -check -noout >/dev/null
openssl pkey -pubin -in "$public_key" -pubcheck -noout >/dev/null

chmod 0600 "$private_key"
chmod 0644 "$public_key"

echo "Wrote Bobr repository signing keys to $output_dir"
echo "  Private key: $private_key"
echo "  Public key:  $public_key"
