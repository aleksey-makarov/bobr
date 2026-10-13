use super::*;
use crate::RepositoryErrorKind;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use tokio::task::JoinSet;
use tokio_rustls::TlsAcceptor;
use tokio_rustls::rustls::{self, pki_types::pem::PemObject};

// Public test-only credentials, unrelated to any deployed repository.
const CA: &[u8] = include_bytes!("testdata/transport-ca.pem");
const CERT: &[u8] = include_bytes!("testdata/transport-server.pem");
const KEY: &[u8] = include_bytes!("testdata/transport-server-key.pem");
const READ_TIMEOUT: Duration = Duration::from_millis(500);

#[derive(Clone, Copy)]
enum Response {
    Slow,
    StalledHeaders,
    StalledBody,
    Truncated,
}

struct HttpsFixture {
    url: Url,
    task: tokio::task::JoinHandle<()>,
}

impl HttpsFixture {
    async fn new(response: Response) -> Self {
        let certificates = rustls::pki_types::CertificateDer::pem_slice_iter(CERT)
            .collect::<Result<Vec<_>, _>>()
            .unwrap();
        let key = rustls::pki_types::PrivateKeyDer::from_pem_slice(KEY).unwrap();
        let config = rustls::ServerConfig::builder_with_provider(Arc::new(
            rustls::crypto::ring::default_provider(),
        ))
        .with_safe_default_protocol_versions()
        .unwrap()
        .with_no_client_auth()
        .with_single_cert(certificates, key)
        .unwrap();
        let acceptor = TlsAcceptor::from(Arc::new(config));
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = Url::parse(&format!(
            "https://{}/object",
            listener.local_addr().unwrap()
        ))
        .unwrap();
        let task = tokio::spawn(async move {
            let mut connections = JoinSet::new();
            loop {
                tokio::select! {
                    accepted = listener.accept() => {
                        let (socket, _) = accepted.unwrap();
                        connections.spawn(serve_connection(acceptor.clone(), socket, response));
                    }
                    joined = connections.join_next(), if !connections.is_empty() => {
                        joined.unwrap().unwrap();
                    }
                }
            }
        });
        Self { url, task }
    }

    fn transport(&self) -> HttpTransport {
        HttpTransport::anonymous_with_timeouts(
            &RepositoryTlsConfig::from_ca_bundle_pem(CA).unwrap(),
            Duration::from_secs(2),
            READ_TIMEOUT,
        )
        .unwrap()
    }

    fn request(&self, destination: &std::path::Path) -> FetchRequest {
        FetchRequest {
            purpose: FetchPurpose::Object(ObjectHash::from_bytes([1; 32])),
            url: self.url.clone(),
            destination: destination.to_path_buf(),
            max_bytes: 1024,
            if_none_match: None,
            progress: None,
        }
    }
}

async fn serve_connection(
    acceptor: TlsAcceptor,
    socket: tokio::net::TcpStream,
    response: Response,
) {
    let Ok(mut socket) = acceptor.accept(socket).await else {
        return;
    };
    let mut request = Vec::new();
    while !request.ends_with(b"\r\n\r\n") {
        let mut byte = [0];
        if socket.read_exact(&mut byte).await.is_err() {
            return;
        }
        request.push(byte[0]);
        assert!(request.len() < 8192);
    }
    if matches!(response, Response::StalledHeaders) {
        std::future::pending::<()>().await;
    }
    if socket
        .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 20\r\nConnection: close\r\n\r\n")
        .await
        .is_err()
    {
        return;
    }
    if socket.write_all(b"x").await.is_err() {
        return;
    }
    if socket.flush().await.is_err() {
        return;
    }
    match response {
        Response::Slow => {
            for _ in 1..20 {
                tokio::time::sleep(Duration::from_millis(50)).await;
                if socket.write_all(b"x").await.is_err() {
                    return;
                }
                if socket.flush().await.is_err() {
                    return;
                }
            }
        }
        Response::StalledBody => std::future::pending::<()>().await,
        Response::Truncated => {}
        Response::StalledHeaders => unreachable!(),
    }
    let _ = socket.shutdown().await;
}

impl Drop for HttpsFixture {
    fn drop(&mut self) {
        self.task.abort();
    }
}

#[tokio::test]
async fn continuous_https_body_can_outlive_read_timeout() {
    let fixture = HttpsFixture::new(Response::Slow).await;
    let temp = tempfile::tempdir().unwrap();
    let destination = temp.path().join("body");
    let started = Instant::now();
    let result = fixture
        .transport()
        .fetch(fixture.request(&destination))
        .await
        .unwrap();
    assert!(matches!(result, FetchResult::Stored(_)));
    assert!(started.elapsed() > READ_TIMEOUT);
    assert_eq!(
        tokio::fs::read(destination).await.unwrap(),
        b"xxxxxxxxxxxxxxxxxxxx"
    );
}

#[tokio::test]
async fn stalled_https_headers_and_body_report_timeout_causes() {
    for response in [Response::StalledHeaders, Response::StalledBody] {
        let fixture = HttpsFixture::new(response).await;
        let temp = tempfile::tempdir().unwrap();
        let error = fixture
            .transport()
            .fetch(fixture.request(&temp.path().join("body")))
            .await
            .unwrap_err();
        assert_eq!(
            error.kind(),
            RepositoryErrorKind::Transport { retryable: true }
        );
        assert!(error.to_string().contains("timed out"), "{error}");
    }
}

#[tokio::test]
async fn truncated_https_body_preserves_underlying_failure() {
    let fixture = HttpsFixture::new(Response::Truncated).await;
    let temp = tempfile::tempdir().unwrap();
    let error = fixture
        .transport()
        .fetch(fixture.request(&temp.path().join("body")))
        .await
        .unwrap_err();
    assert!(error.is_retryable());
    assert!(
        error
            .to_string()
            .starts_with("failed to read HTTP response body: ")
    );
    assert_ne!(
        error.to_string(),
        "failed to read HTTP response body: error decoding response body"
    );
}

#[tokio::test]
async fn caller_configured_client_keeps_its_explicit_deadline() {
    let fixture = HttpsFixture::new(Response::Slow).await;
    let temp = tempfile::tempdir().unwrap();
    let tls = RepositoryTlsConfig::from_ca_bundle_pem(CA).unwrap();
    let client = tls
        .configure_reqwest(reqwest::Client::builder().timeout(Duration::from_millis(150)))
        .build()
        .unwrap();
    let error = HttpTransport::new(client)
        .fetch(fixture.request(&temp.path().join("body")))
        .await
        .unwrap_err();
    assert!(error.is_retryable());
    assert!(error.to_string().contains("timed out"), "{error}");
}

#[tokio::test]
async fn https_response_size_limit_remains_enforced() {
    let fixture = HttpsFixture::new(Response::Slow).await;
    let temp = tempfile::tempdir().unwrap();
    let mut request = fixture.request(&temp.path().join("body"));
    request.max_bytes = 19;
    let error = fixture.transport().fetch(request).await.unwrap_err();
    assert_eq!(error.kind(), RepositoryErrorKind::InvalidRepository);
    assert!(!error.is_retryable());
}

#[derive(Debug)]
struct SelectiveFailure {
    calls: AtomicUsize,
    failing: Mutex<bool>,
    kind: RepositoryErrorKind,
}

#[async_trait]
impl RepositoryTransport for SelectiveFailure {
    async fn fetch(&self, request: FetchRequest) -> Result<FetchResult, RepositoryError> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        tokio::task::yield_now().await;
        if request.url.path() == "/failed" && *self.failing.lock().unwrap() {
            return Err(RepositoryError::with_kind(self.kind, "fixture failure"));
        }
        tokio::fs::write(request.destination, b"complete").await?;
        Ok(FetchResult::Stored(RepresentationMetadata::default()))
    }
}

#[tokio::test]
async fn concurrent_failures_are_cached_per_url_and_scope() {
    let temp = tempfile::tempdir().unwrap();
    let fixture = HttpsFixture::new(Response::Truncated).await;
    let inner = Arc::new(SelectiveFailure {
        calls: AtomicUsize::new(0),
        failing: Mutex::new(true),
        kind: RepositoryErrorKind::Transport { retryable: true },
    });
    let scoped = Arc::new(RunTransport::new(inner.clone()));
    let mut tasks = JoinSet::new();
    for index in 0..8 {
        let mut request = fixture.request(&temp.path().join(format!("failed-{index}")));
        request.url = fixture.url.join("/failed").unwrap();
        let scoped = scoped.clone();
        tasks.spawn(async move { scoped.fetch(request).await });
    }
    while let Some(result) = tasks.join_next().await {
        assert!(result.unwrap().is_err());
    }
    assert_eq!(inner.calls.load(Ordering::SeqCst), 1);
    assert_eq!(scoped.resources.lock().unwrap().len(), 1);
    for index in 0..2 {
        let request = fixture.request(&temp.path().join(format!("good-{index}")));
        scoped.fetch(request).await.unwrap();
    }
    assert_eq!(inner.calls.load(Ordering::SeqCst), 3);
    assert_eq!(scoped.resources.lock().unwrap().len(), 1);
    *inner.failing.lock().unwrap() = false;
    let mut request = fixture.request(&temp.path().join("recovered"));
    request.url = fixture.url.join("/failed").unwrap();
    assert!(scoped.fetch(request.clone()).await.is_err());
    RunTransport::new(inner.clone())
        .fetch(request)
        .await
        .unwrap();
    assert_eq!(inner.calls.load(Ordering::SeqCst), 4);
    assert_eq!(
        tokio::fs::read(temp.path().join("recovered"))
            .await
            .unwrap(),
        b"complete"
    );
}

#[tokio::test]
async fn local_validation_and_cancellation_errors_are_not_cached() {
    let fixture = HttpsFixture::new(Response::Truncated).await;
    let temp = tempfile::tempdir().unwrap();
    for kind in [
        RepositoryErrorKind::LocalIo,
        RepositoryErrorKind::InvalidRepository,
        RepositoryErrorKind::Cancelled,
    ] {
        let inner = Arc::new(SelectiveFailure {
            calls: AtomicUsize::new(0),
            failing: Mutex::new(true),
            kind,
        });
        let scoped = RunTransport::new(inner.clone());
        let mut request = fixture.request(&temp.path().join("body"));
        request.url = fixture.url.join("/failed").unwrap();
        assert_eq!(
            scoped.fetch(request.clone()).await.unwrap_err().kind(),
            kind
        );
        *inner.failing.lock().unwrap() = false;
        scoped.fetch(request).await.unwrap();
        assert_eq!(inner.calls.load(Ordering::SeqCst), 2);
        assert!(scoped.resources.lock().unwrap().is_empty());
    }
}

#[tokio::test]
async fn dropping_active_fetch_releases_its_resource_gate() {
    #[derive(Debug)]
    struct HangOnce(AtomicUsize);

    #[async_trait]
    impl RepositoryTransport for HangOnce {
        async fn fetch(&self, request: FetchRequest) -> Result<FetchResult, RepositoryError> {
            if self.0.fetch_add(1, Ordering::SeqCst) == 0 {
                std::future::pending::<()>().await;
            }
            tokio::fs::write(request.destination, b"complete").await?;
            Ok(FetchResult::Stored(RepresentationMetadata::default()))
        }
    }

    let temp = tempfile::tempdir().unwrap();
    let fixture = HttpsFixture::new(Response::Truncated).await;
    let inner = Arc::new(HangOnce(AtomicUsize::new(0)));
    let scoped = Arc::new(RunTransport::new(inner.clone()));
    let request = fixture.request(&temp.path().join("cancelled"));
    let first = {
        let scoped = scoped.clone();
        tokio::spawn(async move { scoped.fetch(request).await })
    };
    tokio::time::timeout(Duration::from_secs(2), async {
        while inner.0.load(Ordering::SeqCst) == 0 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    let request = fixture.request(&temp.path().join("second"));
    let second = {
        let scoped = scoped.clone();
        tokio::spawn(async move { scoped.fetch(request).await })
    };
    first.abort();
    assert!(first.await.unwrap_err().is_cancelled());
    tokio::time::timeout(Duration::from_secs(2), second)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    assert_eq!(inner.0.load(Ordering::SeqCst), 2);
    assert!(scoped.resources.lock().unwrap().is_empty());
}
