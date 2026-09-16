# Self-hosting an S3 endpoint with VersityGW

This guide sets up a small S3-compatible server suitable for a Bobr remote
repository. It uses:

- VersityGW with its POSIX backend;
- a systemd system service;
- a private certificate authority for the HTTPS endpoint;
- a public-read, write-authenticated S3 bucket;
- a separate Ed25519 key for signing the Bobr repository master.

The commands use a DNS name as the server identity and `/srv/versitygw` as
the persistent backend. An IP address can be used instead when it is encoded
as an IP Subject Alternative Name in the TLS certificate.

## Deployment model

A Bobr remote repository has four logical roles. They may run on separate
machines, although a small deployment may combine the *builder* and *hoster*.

### Builder

The *builder* realizes the intended Bobr world into a complete local store and
runs `bobr-repo prepare`. Preparation:

- reads and verifies the currently published master;
- scans and validates the local store;
- uploads missing immutable objects, filesystem files, indexes, and lists
  through the authenticated S3 API;
- produces an unsigned `candidate-master.cbor`.

The *builder* needs S3 credentials which can upload immutable repository data.
It does not need and should not possess the repository signing key.

### Hoster

The *hoster* runs the S3-compatible storage service described by this guide. It
stores `/master` and the immutable repository namespaces and serves them to
*clients* over HTTPS. It neither builds packages nor signs repository state.

A compromised *hoster* can withhold data or return malformed bytes, but
*clients* reject forged content using the signed master and content hashes.
The *hoster* cannot create new authenticated repository state without the
repository signing key.

### Administrator

The *administrator* receives `candidate-master.cbor` from the *builder* on a
trusted machine and runs `bobr-repo publish`. Publication:

- verifies the currently published master;
- verifies that the candidate names it as its predecessor;
- displays the proposed state transition for review;
- signs the candidate with the repository Ed25519 private key;
- conditionally replaces `/master` through the authenticated S3 API.

The *administrator* holds the signing key and S3 credentials which can update
`/master`. The complete local build store is not needed on this machine.

### Client

A *client* has neither S3 credentials nor private keys. It downloads `/master`
and immutable repository data anonymously over HTTPS. It pins the
repository's Ed25519 public key, verifies the master signature, and verifies
all downloaded indexes and content by their hashes before importing them into
its working store.

The complete data flow is:

```text
                         unsigned candidate
Builder --------------------------------------------> Administrator
   |                                                       |
   | authenticated immutable uploads                      | signed /master
   v                                                       v
Hoster <---------------------------------------------------+
   |
   | anonymous verified downloads
   v
Client
```

The protocol does not require the *builder* and *hoster* to share a machine.
Even when they do, `bobr-repo prepare` uses the same authenticated S3
interface as a remote *builder*. Keeping the *administrator* separate prevents
the *builder* or *hoster* from publishing a new signed master without the
*administrator's* key. The signature records repository state approved by the
*administrator*; by itself it does not prove that the uploaded packages were
built correctly or reproducibly.

## Keys and credentials

The setup has three independent kinds of key material:

| Material | Purpose | Where its secret is kept |
|---|---|---|
| S3 credentials | Authorize uploads and repository administration | *Hoster* and authorized *builder* or *administrator* |
| TLS CA and server key | Authenticate and encrypt the HTTPS endpoint | CA key on a trusted machine; server key on the *hoster* |
| Bobr Ed25519 signing key | Sign the repository `/master` | Trusted publishing machine only |

Do not reuse a key between these roles. In particular, TLS authenticates a
network connection, while the Ed25519 signature authenticates repository
state.

Private keys must not be committed to source control, embedded in package or
machine configuration, or copied into a world-readable build artifact. The
systemd unit below refers to private files by path; it does not contain their
contents.

## Prerequisites

The machine used to administer the *hoster* needs:

- OpenSSL;
- AWS CLI v2;
- SSH and SCP access to the *hoster*.

The *hoster* needs a filesystem with extended-attribute support. VersityGW's
POSIX backend uses xattrs for S3 metadata. Do not modify its backend directory
behind the gateway.

Install the VersityGW executable according to its upstream documentation. The
systemd example below expects it at `/usr/local/bin/versitygw`; replace that
path with the result of `command -v versitygw` on the *hoster*.

The repository includes three optional setup helpers. From the directory where
their `out/` results should be created, run
`tools/server/gen-versitygw-env.sh` to generate the S3 credentials and
`tools/server/gen-versitygw-tls.sh <server-name-or-ip>` to generate the private
CA and VersityGW certificate described below. Run
`tools/server/gen-bobr-repository-signing-key.sh` to generate the separate
Ed25519 repository signing key and its public key.

The examples use these values on the machine administering the *hoster*:

```sh
export BOBR_PKI="$HOME/.config/bobr/pki"
export S3_HOST=s3.example.net
export S3_SSH_HOST="$S3_HOST"
export S3_PORT=7070
export S3_ENDPOINT="https://${S3_HOST}:${S3_PORT}"
export S3_REGION=us-east-1
export S3_BUCKET=bobr-repository

install -d -m 0700 "$BOBR_PKI"
```

`install -d -m 0700` creates the key directory, if necessary, and makes it
accessible only to its owner.

## Service account and filesystem layout

The remainder of this guide assumes that the *hoster* provides:

- an unprivileged system user and group named `versitygw`;
- the VersityGW executable at `/usr/local/bin/versitygw`;
- a configuration directory at `/etc/versitygw`;
- a persistent POSIX-backend directory at `/srv/versitygw`.

The service account does not need an interactive login. The directories and
files created throughout this guide have the following final ownership and
permissions:

| Path | Owner | Mode | Purpose |
|---|---|---:|---|
| `/etc/versitygw` | `root:root` | `0755` | Service configuration |
| `/etc/versitygw/versitygw.env` | `root:root` | `0600` | S3 root credentials |
| `/etc/versitygw/tls` | `root:versitygw` | `0750` | TLS material |
| `/etc/versitygw/tls/server.key` | `root:versitygw` | `0640` | TLS private key |
| `/etc/versitygw/tls/server.pem` | `root:root` | `0644` | TLS certificate |
| `/srv/versitygw` | `versitygw:versitygw` | `0700` | POSIX backend |

`/srv/versitygw` may itself be a mount point. Its filesystem must support
extended attributes, be writable by the `versitygw` account, and provide
enough space for the repository.

How the account, group, executable, and directories are provisioned is
distribution-specific and outside this guide.

## Generate S3 root credentials

VersityGW's root S3 account bypasses normal bucket policy and ACL evaluation.
Use it only for initial setup and administration. A production deployment can
later introduce narrower identities for preparation, publication, and garbage
collection.

Generate an AWS-shaped access key and a 256-bit secret:

```sh
access_key="AKIA$(openssl rand -hex 8 | tr 'a-f' 'A-F')"
secret_key="$(openssl rand -hex 32)"
```

`openssl rand` obtains bytes from the operating system cryptographic random
number generator. The first command produces 64 random bits after the `AKIA`
prefix and restricts the result to characters safe in an AWS access-key ID.
The second produces 32 random bytes, encoded as 64 hexadecimal characters.

On the *hoster*, create `/etc/versitygw/versitygw.env`:

```text
ROOT_ACCESS_KEY_ID=AKIA...
ROOT_SECRET_ACCESS_KEY=...
```

Then protect it:

```sh
sudo chown root:root /etc/versitygw/versitygw.env
sudo chmod 0600 /etc/versitygw/versitygw.env
```

The systemd manager reads this file and passes the values to the unprivileged
VersityGW service. The service process does not need permission to open the
file itself. An environment file keeps the secret out of declarative service
configuration and the process command line.

Configure the same credentials on the machine used for initial S3 setup. For
example, create `~/.config/bobr/aws-credentials` with mode `0600`:

```ini
[bobr-server]
aws_access_key_id = AKIA...
aws_secret_access_key = ...
```

Select it without copying credentials into shell history:

```sh
export AWS_SHARED_CREDENTIALS_FILE="$HOME/.config/bobr/aws-credentials"
export AWS_PROFILE=bobr-server
export AWS_REGION="$S3_REGION"
```

This guide uses the root identity to bootstrap and verify a small deployment.
Do not routinely share it between all four roles. A production deployment
should issue narrower S3 identities: the *builder* writes immutable namespaces,
the *administrator* reads repository state and updates `/master`, and garbage
collection has only the additional listing and deletion permissions it needs.
The [`bobr-repo` command-line guide](CLI.md#deployment-and-credentials)
describes those capability boundaries.

## Create a private TLS certificate authority

A private CA is useful for a LAN deployment and for end-to-end testing.

Create an elliptic-curve P-256 private key for the CA:

```sh
umask 077

openssl genpkey \
    -algorithm EC \
    -pkeyopt ec_paramgen_curve:P-256 \
    -out "$BOBR_PKI/local-repository-ca.key.pem"
```

`genpkey` creates a PKCS#8 private key. P-256 is a widely supported curve for
TLS certificate chains. `umask 077` prevents newly created files from gaining
group or other permissions.

Create a self-signed CA certificate valid for ten years:

```sh
openssl req \
    -x509 \
    -new \
    -key "$BOBR_PKI/local-repository-ca.key.pem" \
    -sha256 \
    -days 3650 \
    -subj "/CN=Bobr Local Repository CA" \
    -addext "basicConstraints=critical,CA:TRUE,pathlen:0" \
    -addext "keyUsage=critical,keyCertSign,cRLSign" \
    -addext "subjectKeyIdentifier=hash" \
    -out "$BOBR_PKI/local-repository-ca.cert.pem"
```

`req -x509` emits a certificate rather than a certificate request. Because
this is the trust root, it signs itself. `basicConstraints` marks it as a CA;
`pathlen:0` permits it to sign server certificates but not subordinate CAs.
`keyUsage` restricts the key to certificate and revocation-list signing.

Keep both the CA key and the serial file created during certificate issuance.
The CA certificate is public and may be copied to *clients*; the CA private key
must remain on a trusted machine.

## Issue the VersityGW server certificate

Create a separate P-256 private key for the HTTPS server:

```sh
openssl genpkey \
    -algorithm EC \
    -pkeyopt ec_paramgen_curve:P-256 \
    -out "$BOBR_PKI/versitygw-server.key.pem"
```

Create a certificate signing request. The Subject Alternative Name must cover
the exact address used by *clients*:

```sh
openssl req \
    -new \
    -key "$BOBR_PKI/versitygw-server.key.pem" \
    -subj "/CN=${S3_HOST}" \
    -addext "subjectAltName=DNS:${S3_HOST}" \
    -addext "keyUsage=critical,digitalSignature" \
    -addext "extendedKeyUsage=serverAuth" \
    -out "$BOBR_PKI/versitygw-server.csr.pem"
```

The CSR contains the *hoster's* public key and requested certificate
extensions. Modern TLS clients validate the IP address or hostname against
`subjectAltName`; the Common Name is not a substitute for it.

When *clients* connect by IP address, use an IP SAN instead:

```text
-addext "subjectAltName=IP:<server-address>"
```

An IP address encoded as a `DNS` SAN is not equivalent.

Sign the request with the local CA:

```sh
openssl x509 \
    -req \
    -in "$BOBR_PKI/versitygw-server.csr.pem" \
    -CA "$BOBR_PKI/local-repository-ca.cert.pem" \
    -CAkey "$BOBR_PKI/local-repository-ca.key.pem" \
    -CAcreateserial \
    -days 825 \
    -sha256 \
    -copy_extensions copy \
    -out "$BOBR_PKI/versitygw-server.cert.pem"
```

`x509 -req` turns the CSR into a certificate. `-CA` and `-CAkey` select the
issuer, `-CAcreateserial` creates and maintains its certificate serial file,
and `-copy_extensions copy` preserves the requested SAN and TLS usage
constraints.

Verify the resulting chain and inspect its identity:

```sh
openssl verify \
    -CAfile "$BOBR_PKI/local-repository-ca.cert.pem" \
    "$BOBR_PKI/versitygw-server.cert.pem"

openssl x509 \
    -in "$BOBR_PKI/versitygw-server.cert.pem" \
    -noout -subject -issuer -ext subjectAltName
```

The first command must report `OK`; the second must contain the address by
which *clients* will reach the *hoster*.

## Install the TLS key on the *hoster*

Only the server certificate and its private key are installed on the *hoster*.
Do not copy the CA key or the Bobr repository signing key there.

```sh
ssh "$S3_SSH_HOST" \
    'sudo install -d -o root -g versitygw -m 0750 /etc/versitygw/tls'

scp \
    "$BOBR_PKI/versitygw-server.key.pem" \
    "$BOBR_PKI/versitygw-server.cert.pem" \
    "${S3_SSH_HOST}:/tmp/"

ssh "$S3_SSH_HOST" '
    sudo install -o root -g versitygw -m 0640 \
        /tmp/versitygw-server.key.pem \
        /etc/versitygw/tls/server.key
    sudo install -o root -g root -m 0644 \
        /tmp/versitygw-server.cert.pem \
        /etc/versitygw/tls/server.pem
    rm -f /tmp/versitygw-server.key.pem /tmp/versitygw-server.cert.pem
'
```

The service group can read the private key; other unprivileged users cannot.
The CA private key remains on its trusted issuing machine.

## Configure the systemd service

Create `/etc/systemd/system/versitygw.service`:

```systemd
[Unit]
Description=Versity S3 Gateway
Wants=network-online.target
After=network-online.target
RequiresMountsFor=/srv/versitygw

[Service]
Type=simple
User=versitygw
Group=versitygw
EnvironmentFile=/etc/versitygw/versitygw.env
ExecStart=/usr/local/bin/versitygw \
    --port :7070 \
    --cert /etc/versitygw/tls/server.pem \
    --key /etc/versitygw/tls/server.key \
    posix /srv/versitygw
Restart=on-failure
UMask=0077
NoNewPrivileges=true
PrivateTmp=true
ProtectHome=true
ProtectSystem=strict
ReadWritePaths=/srv/versitygw

[Install]
WantedBy=multi-user.target
```

`RequiresMountsFor` orders the service after the backend mount. `--cert` and
`--key` enable TLS on the S3 listener; omitting either leaves the listener
without a usable TLS configuration. The final `posix /srv/versitygw` selects
the POSIX backend and its root. `ProtectSystem=strict` makes the rest of the
host filesystem read-only to the service, while `ReadWritePaths` grants the
required backend access.

Open TCP port 7070 only to the networks which should reach the service. A
public repository needs public HTTPS reads, but its authenticated S3
administration endpoint can instead be restricted by a firewall or separate
network path if the deployment provides distinct endpoints.

Reload systemd, enable the service, and inspect its initial startup:

```sh
sudo systemctl daemon-reload
sudo systemctl enable --now versitygw
systemctl status versitygw
journalctl -u versitygw -b --no-pager
```

VersityGW terminates TLS itself; no additional HTTP server is involved.

## Verify authenticated S3 access

Tell AWS CLI to trust the private CA:

```sh
export AWS_CA_BUNDLE="$BOBR_PKI/local-repository-ca.cert.pem"
```

List the *hoster's* buckets:

```sh
aws \
    --endpoint-url "$S3_ENDPOINT" \
    s3api list-buckets
```

AWS CLI signs this request using the selected profile. A certificate error
means the CA or SAN is wrong; `InvalidAccessKeyId` or `SignatureDoesNotMatch`
means the AWS CLI profile and `/etc/versitygw/versitygw.env` do not contain
the same S3 credentials.

## Initialize the repository bucket

Export the endpoint for the standard AWS SDK configuration chain, then let
`bobr-repo` create and configure the dedicated bucket:

```sh
export AWS_ENDPOINT_URL_S3="$S3_ENDPOINT"

bobr-repo init \
    --repository "s3://$S3_BUCKET" \
    --ca-bundle "$BOBR_PKI/local-repository-ca.cert.pem"
```

The operation is idempotent. It owns the bucket's complete policy, exposes
anonymous `GetObject` only for Bobr's public namespaces, and configures
bucket-level Public Access Block so public ACLs stay disabled while the
explicit public-read policy remains usable. VersityGW may report Public Access
Block as unsupported; `bobr-repo` reports this and still installs the policy.

Do not use this bucket for unrelated data. Do not enable S3 object versioning
merely for Bobr: immutable repository keys are already content-addressed,
while publication of `/master` uses conditional replacement.

Verify that anonymous listing is denied:

```sh
aws \
    --no-sign-request \
    --ca-bundle "$BOBR_PKI/local-repository-ca.cert.pem" \
    --endpoint-url "$S3_ENDPOINT" \
    s3api list-objects-v2 \
    --bucket "$S3_BUCKET"
```

## Generate the Bobr repository signing key

This final key does not belong to S3 or TLS. Bobr uses it to create the
COSE_Sign1 signature on `/master` described in [`MASTER.md`](MASTER.md).

Generate an Ed25519 private key:

```sh
openssl genpkey \
    -algorithm Ed25519 \
    -out "$BOBR_PKI/bobr-repository-signing.key.pem"
```

Ed25519 is the signature algorithm fixed by repository format version 1.
OpenSSL writes an unencrypted PKCS#8 key, which `bobr-repo publish` accepts.
The file must remain on the trusted publishing machine and must have no group
or other permission bits:

```sh
chmod 0600 "$BOBR_PKI/bobr-repository-signing.key.pem"
```

Derive the corresponding public SubjectPublicKeyInfo document:

```sh
openssl pkey \
    -in "$BOBR_PKI/bobr-repository-signing.key.pem" \
    -pubout \
    -out "$BOBR_PKI/bobr-repository-signing.pub.pem"

chmod 0644 "$BOBR_PKI/bobr-repository-signing.pub.pem"
```

`pkey -pubout` derives only the public key; it does not disclose the private
scalar. Distribute this public key to Bobr *clients* through an authenticated
channel. It is their trust anchor for the repository and must not be accepted
merely because the repository itself supplied it.

Check that the private and public files describe the same key:

```sh
private_public="$BOBR_PKI/bobr-repository-signing.derived.pub.pem"

openssl pkey \
    -in "$BOBR_PKI/bobr-repository-signing.key.pem" \
    -pubout \
    -out "$private_public"

cmp "$private_public" "$BOBR_PKI/bobr-repository-signing.pub.pem"
rm "$private_public"
```

`cmp` produces no output and exits successfully when the files are identical.

The hosting service is now ready for the [`bobr-repo` publication
workflow](CLI.md). For the example bucket, the repository locations are:

```text
S3 administration: s3://bobr-repository
master URL:         https://s3.example.net:7070/bobr-repository/master
data base URL:      https://s3.example.net:7070/bobr-repository/
```

Pass the public CA certificate to every `bobr-repo` command which addresses
this repository:

```text
--ca-bundle local-repository-ca.cert.pem
```

The additional CA is used for both authenticated S3 requests and anonymous
reads through the public URLs. It is added to the standard trusted roots, so
the same command may still follow repository URLs certified by a public CA.
Disabling certificate verification is not an acceptable substitute.

## Backup and rotation

Back up these values independently:

- the VersityGW backend under `/srv/versitygw`;
- the repository signing private key;
- the local CA key and its serial file;
- S3 credentials required for administration.

Losing the S3 root secret requires rotating the *hoster's* credentials. Losing
the TLS CA key prevents issuing replacement certificates under the same trust
anchor. Losing the Bobr signing key prevents publishing a new master trusted
by existing *clients*; replacing it requires an authenticated trust-anchor
rollover outside the repository itself.

Compromise has different consequences. Rotate S3 credentials after an S3
credential leak, replace the HTTPS certificate after a TLS server-key leak,
and treat a repository-signing-key leak as compromise of the repository's
authenticated history.

## Further reading

- [VersityGW global options](https://github.com/versity/versitygw/wiki/Global-Options)
- [VersityGW POSIX backend](https://github.com/versity/versitygw/wiki/POSIX-Backend)
- [VersityGW differences from AWS S3](https://github.com/versity/versitygw/wiki/Differences-from-AWS-S3)
