#!/usr/bin/env bash
# Create a private TLS CA and a TLS certificate for VersityGW under out/ in
# the current directory.
#
# Usage:
#   ./gen-versitygw-tls.sh [-f|--force] <server-dns-name-or-ip>

set -euo pipefail

force=0
server_identity=""

usage() {
    echo "Usage: $0 [-f|--force] <server-dns-name-or-ip>" >&2
}

for arg in "$@"; do
    case "$arg" in
        -f|--force)
            force=1
            ;;
        -h|--help)
            usage
            exit 0
            ;;
        -*)
            echo "error: unexpected option: $arg" >&2
            usage
            exit 2
            ;;
        *)
            if [[ -n "$server_identity" ]]; then
                echo "error: more than one server identity was provided" >&2
                usage
                exit 2
            fi
            server_identity="$arg"
            ;;
    esac
done

if [[ -z "$server_identity" ]]; then
    echo "error: a server DNS name or IP address is required" >&2
    usage
    exit 2
fi

# Keep the identity safe for both the OpenSSL subject string and SAN syntax.
if [[ ! "$server_identity" =~ ^[A-Za-z0-9._:-]+$ ]]; then
    echo "error: invalid server DNS name or IP address: $server_identity" >&2
    exit 2
fi

if [[ "$server_identity" == *:* || \
      "$server_identity" =~ ^[0-9]+\.[0-9]+\.[0-9]+\.[0-9]+$ ]]; then
    subject_alt_name="IP:$server_identity"
else
    subject_alt_name="DNS:$server_identity"
fi

output_dir="$PWD/out"
ca_key="$output_dir/local-repository-ca.key.pem"
ca_cert="$output_dir/local-repository-ca.cert.pem"
ca_serial="$output_dir/local-repository-ca.cert.srl"
server_key="$output_dir/versitygw-server.key.pem"
server_cert="$output_dir/versitygw-server.cert.pem"
outputs=(
    "$ca_key"
    "$ca_cert"
    "$ca_serial"
    "$server_key"
    "$server_cert"
)

umask 077
install -d -m 0700 "$output_dir"

if [[ "$force" -ne 1 ]]; then
    for output in "${outputs[@]}"; do
        if [[ -e "$output" ]]; then
            echo "error: $output already exists, pass -f/--force to overwrite" >&2
            exit 1
        fi
    done
fi

server_csr="$(mktemp "$output_dir/.versitygw-server.csr.XXXXXX")"
cleanup() {
    rm -f -- "$server_csr"
}
trap cleanup EXIT

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
    -algorithm EC \
    -pkeyopt ec_paramgen_curve:P-256 \
    -out "$ca_key"

openssl req \
    -x509 \
    -new \
    -key "$ca_key" \
    -sha256 \
    -days 3650 \
    -subj "/CN=Bobr Local Repository CA" \
    -addext "basicConstraints=critical,CA:TRUE,pathlen:0" \
    -addext "keyUsage=critical,keyCertSign,cRLSign" \
    -addext "subjectKeyIdentifier=hash" \
    -out "$ca_cert"

openssl genpkey \
    -algorithm EC \
    -pkeyopt ec_paramgen_curve:P-256 \
    -out "$server_key"

openssl req \
    -new \
    -key "$server_key" \
    -subj "/CN=$server_identity" \
    -addext "subjectAltName=$subject_alt_name" \
    -addext "basicConstraints=critical,CA:FALSE" \
    -addext "keyUsage=critical,digitalSignature" \
    -addext "extendedKeyUsage=serverAuth" \
    -out "$server_csr"

openssl x509 \
    -req \
    -in "$server_csr" \
    -CA "$ca_cert" \
    -CAkey "$ca_key" \
    -CAserial "$ca_serial" \
    -CAcreateserial \
    -days 825 \
    -sha256 \
    -copy_extensions copy \
    -out "$server_cert"

openssl verify -CAfile "$ca_cert" "$server_cert"

chmod 0600 "$ca_key" "$ca_serial" "$server_key"
chmod 0644 "$ca_cert" "$server_cert"

echo "Wrote TLS material for $server_identity to $output_dir"
echo "  CA certificate:     $ca_cert"
echo "  CA private key:     $ca_key"
echo "  Server certificate: $server_cert"
echo "  Server private key: $server_key"
