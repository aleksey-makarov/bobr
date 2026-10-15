//! TLS trust configuration shared by repository transports.

use crate::RepositoryError;
use aws_smithy_http_client::tls::{TlsContext, TrustStore};
use std::fmt;
use std::path::Path;
use std::sync::Arc;

/// TLS trust policy shared by the S3 and anonymous HTTPS transports.
///
/// Each transport retains its default trusted roots. An optional PEM bundle
/// adds repository-local certificate authorities to those roots.
#[derive(Clone, Default)]
pub struct RepositoryTlsConfig {
    additional_roots: Option<AdditionalRoots>,
}

#[derive(Clone)]
struct AdditionalRoots {
    pem: Arc<[u8]>,
    reqwest: Arc<[reqwest::Certificate]>,
}

impl fmt::Debug for RepositoryTlsConfig {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("RepositoryTlsConfig")
            .field(
                "additional_root_certificates",
                &self
                    .additional_roots
                    .as_ref()
                    .map_or(0, |roots| roots.reqwest.len()),
            )
            .finish()
    }
}

impl RepositoryTlsConfig {
    /// Uses only the transports' default trusted certificate authorities.
    pub fn default_roots() -> Self {
        Self::default()
    }

    /// Loads a PEM bundle whose certificates are added to the platform roots.
    pub fn from_ca_bundle(path: &Path) -> Result<Self, RepositoryError> {
        let pem = std::fs::read(path).map_err(|error| {
            RepositoryError::configuration(format!(
                "failed to read CA bundle '{}': {error}",
                path.display()
            ))
        })?;
        Self::from_ca_bundle_pem(&pem).map_err(|error| {
            RepositoryError::configuration(format!(
                "invalid CA bundle '{}': {error}",
                path.display()
            ))
        })
    }

    /// Parses PEM certificates which are added to the platform roots.
    pub fn from_ca_bundle_pem(pem: &[u8]) -> Result<Self, RepositoryError> {
        let certificates = reqwest::Certificate::from_pem_bundle(pem).map_err(|error| {
            RepositoryError::configuration(format!("failed to parse PEM certificates: {error}"))
        })?;
        if certificates.is_empty() {
            return Err(RepositoryError::configuration(
                "the PEM bundle contains no certificates",
            ));
        }
        Ok(Self {
            additional_roots: Some(AdditionalRoots {
                pem: Arc::from(pem),
                reqwest: certificates.into(),
            }),
        })
    }

    pub(crate) fn configure_reqwest(
        &self,
        mut builder: reqwest::ClientBuilder,
    ) -> reqwest::ClientBuilder {
        if let Some(roots) = &self.additional_roots {
            for certificate in roots.reqwest.iter() {
                builder = builder.add_root_certificate(certificate.clone());
            }
        }
        builder
    }

    pub(crate) fn smithy_tls_context(&self) -> Result<TlsContext, RepositoryError> {
        let mut trust_store = TrustStore::default();
        if let Some(roots) = &self.additional_roots {
            trust_store.add_pem_certificate(roots.pem.to_vec());
        }
        TlsContext::builder()
            .with_trust_store(trust_store)
            .build()
            .map_err(|error| {
                RepositoryError::configuration(format!("failed to configure S3 TLS trust: {error}"))
            })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const TEST_CA: &[u8] = include_bytes!("testdata/transport-unrelated-ca.pem");

    #[test]
    fn accepts_one_or_more_pem_certificates() {
        let one = RepositoryTlsConfig::from_ca_bundle_pem(TEST_CA).unwrap();
        assert_eq!(
            format!("{one:?}"),
            "RepositoryTlsConfig { additional_root_certificates: 1 }"
        );

        let two = [TEST_CA, TEST_CA].concat();
        let two = RepositoryTlsConfig::from_ca_bundle_pem(&two).unwrap();
        assert_eq!(
            format!("{two:?}"),
            "RepositoryTlsConfig { additional_root_certificates: 2 }"
        );
    }

    #[test]
    fn configures_both_http_stacks_from_the_same_bundle() {
        let config = RepositoryTlsConfig::from_ca_bundle_pem(TEST_CA).unwrap();
        config
            .configure_reqwest(reqwest::Client::builder())
            .build()
            .unwrap();
        config.smithy_tls_context().unwrap();
    }

    #[test]
    fn rejects_empty_and_invalid_pem_bundles() {
        assert!(RepositoryTlsConfig::from_ca_bundle_pem(b"").is_err());
        assert!(RepositoryTlsConfig::from_ca_bundle_pem(b"not a certificate").is_err());
    }

    #[test]
    fn reports_the_ca_bundle_path_on_read_errors() {
        let error = RepositoryTlsConfig::from_ca_bundle(Path::new("/no/such/ca.pem"))
            .unwrap_err()
            .to_string();
        assert!(error.contains("/no/such/ca.pem"));
    }
}
