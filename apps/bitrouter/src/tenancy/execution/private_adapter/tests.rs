use std::{error::Error, io, path::PathBuf, time::Duration};

use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::TcpListener,
};
use tokio_rustls::{TlsAcceptor, rustls};

use super::*;

const KEY_ENV: &str = "BITROUTER_TEST_PRIVATE_CA_KEY";
const CHILD_ENV: &str = "BITROUTER_TEST_PRIVATE_CA_CHILD";
const TLS_WITNESS: &str = "private adapter CA TLS cases completed";

struct FixtureCa {
    params: rcgen::CertificateParams,
    key: rcgen::KeyPair,
    pem: String,
}

impl FixtureCa {
    fn new(name: &str) -> anyhow::Result<Self> {
        let now = time::OffsetDateTime::now_utc();
        let mut params = rcgen::CertificateParams::new(Vec::<String>::new())?;
        params
            .distinguished_name
            .push(rcgen::DnType::CommonName, name);
        params.is_ca = rcgen::IsCa::Ca(rcgen::BasicConstraints::Unconstrained);
        params.key_usages = vec![
            rcgen::KeyUsagePurpose::KeyCertSign,
            rcgen::KeyUsagePurpose::CrlSign,
        ];
        params.not_before = now - time::Duration::days(7);
        params.not_after = now + time::Duration::days(30);
        let key = rcgen::KeyPair::generate()?;
        let pem = params.self_signed(&key)?.pem();
        Ok(Self { params, key, pem })
    }

    fn acceptor(&self, not_after: time::OffsetDateTime) -> anyhow::Result<TlsAcceptor> {
        let issuer = rcgen::Issuer::from_params(&self.params, &self.key);
        let mut params = rcgen::CertificateParams::new(vec!["localhost".into()])?;
        params.not_before = time::OffsetDateTime::now_utc() - time::Duration::days(2);
        params.not_after = not_after;
        params.extended_key_usages = vec![rcgen::ExtendedKeyUsagePurpose::ServerAuth];
        params.key_usages = vec![rcgen::KeyUsagePurpose::DigitalSignature];
        let key = rcgen::KeyPair::generate()?;
        let cert = params.signed_by(&key, &issuer)?.der().clone();
        let key = rustls::pki_types::PrivatePkcs8KeyDer::from(key.serialize_der());
        let config = rustls::ServerConfig::builder_with_provider(Arc::new(
            rustls::crypto::ring::default_provider(),
        ))
        .with_safe_default_protocol_versions()?
        .with_no_client_auth()
        .with_single_cert(vec![cert], key.into())?;
        Ok(TlsAcceptor::from(Arc::new(config)))
    }
}

#[test]
fn ca_bundle_checks_framing_and_bounds() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let path = dir.path().join("ca.pem");
    assert!(read_ca_bundle(&path).is_err());
    assert!(read_ca_bundle(dir.path()).is_err());

    let ca = FixtureCa::new("Private adapter bounds root")?;
    let lf = ca.pem.replace("\r\n", "\n");
    // The old LF-only framing check saw the final LF-delimited footer,
    // while the decoder had already ended the certificate at the bare CR.
    let bare_cr_junk = lf.replace(
        "\n-----END CERTIFICATE-----",
        "\r-----END CERTIFICATE-----\rtrailing junk\n-----END CERTIFICATE-----",
    );
    for invalid in [
        String::new(),
        " \r\n\t".into(),
        "-----BEGIN PRIVATE KEY-----\nYQ==\n-----END PRIVATE KEY-----\n".into(),
        format!(
            "{}-----BEGIN PRIVATE KEY-----\nYQ==\n-----END PRIVATE KEY-----\n",
            ca.pem
        ),
        format!("{}trailing text\n", ca.pem),
        bare_cr_junk,
        "-----BEGIN CERTIFICATE-----\nYQ==\n".into(),
        "-----BEGIN CERTIFICATE-----\n?\n-----END CERTIFICATE-----\n".into(),
    ] {
        std::fs::write(&path, invalid)?;
        assert!(read_ca_bundle(&path).is_err());
    }

    std::fs::write(&path, lf.replace('\n', "\r\n"))?;
    assert_eq!(read_ca_bundle(&path)?.len(), 1);

    let limit = usize::try_from(MAX_CA_BUNDLE_BYTES)?;
    let mut maximum = ca.pem.repeat(MAX_CA_CERTIFICATES).into_bytes();
    assert!(maximum.len() < limit);
    maximum.resize(limit, b' ');
    std::fs::write(&path, &maximum)?;
    assert_eq!(read_ca_bundle(&path)?.len(), MAX_CA_CERTIFICATES);
    maximum.push(b' ');
    std::fs::write(&path, maximum)?;
    assert!(read_ca_bundle(&path).is_err());
    std::fs::write(&path, ca.pem.repeat(MAX_CA_CERTIFICATES + 1))?;
    assert!(read_ca_bundle(&path).is_err());
    Ok(())
}

fn load_peer(endpoint: &str, ca: Option<&Path>) -> anyhow::Result<Peer> {
    Peer::load(
        &PrivateAdapterConfig {
            audience: "private-ca-fixture".into(),
            endpoints: vec![endpoint.into()],
            key_env: KEY_ENV.into(),
            ca_certificates_file: ca.map(PathBuf::from),
        },
        HttpTimeouts::default().read,
    )
}

enum ClientKind<'a> {
    Peer(Option<&'a Path>),
    Default,
}

async fn tls_exchange(
    acceptor: &TlsAcceptor,
    host: &str,
    kind: ClientKind<'_>,
) -> anyhow::Result<(reqwest::Result<Response>, bool)> {
    let listener = TcpListener::bind("127.0.0.1:0").await?;
    let endpoint = format!("https://{host}:{}/generate", listener.local_addr()?.port());
    let client = match kind {
        ClientKind::Peer(ca) => load_peer(&endpoint, ca)?.client,
        ClientKind::Default => reqwest::Client::builder().no_proxy().build()?,
    };
    let serve = async {
        let (socket, _) = listener.accept().await?;
        let Ok(mut stream) = acceptor.accept(socket).await else {
            return Ok::<_, anyhow::Error>(false);
        };
        let mut request = Vec::new();
        let mut chunk = [0; 1024];
        while !request.windows(4).any(|part| part == b"\r\n\r\n") {
            let count = stream.read(&mut chunk).await?;
            ensure!(
                count > 0 && request.len() + count <= 8192,
                "invalid fixture HTTP request"
            );
            request.extend_from_slice(&chunk[..count]);
        }
        ensure!(request.starts_with(b"POST /generate HTTP/1.1\r\n"));
        stream
            .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\nok")
            .await?;
        stream.shutdown().await?;
        Ok(true)
    };
    let (response, sent) = tokio::join!(client.post(endpoint).send(), serve);
    Ok((response, sent?))
}

fn certificate_failure(mut error: &(dyn Error + 'static)) -> bool {
    loop {
        if matches!(
            error.downcast_ref::<rustls::Error>(),
            Some(rustls::Error::InvalidCertificate(_))
        ) {
            return true;
        }
        // io::Error's source need not expose the wrapper's inner error itself.
        let Some(next) = error
            .downcast_ref::<io::Error>()
            .and_then(io::Error::get_ref)
            .map(|inner| inner as &(dyn Error + 'static))
            .or_else(|| error.source())
        else {
            return false;
        };
        error = next;
    }
}

async fn tls_cases() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let ca = FixtureCa::new("Private adapter TLS root")?;
    let ca_path = dir.path().join("peer-ca.pem");
    std::fs::write(&ca_path, &ca.pem)?;
    let wrong_path = dir.path().join("wrong-ca.pem");
    std::fs::write(&wrong_path, FixtureCa::new("Unrelated TLS root")?.pem)?;
    let invalid_path = dir.path().join("invalid-ca.pem");
    std::fs::write(
        &invalid_path,
        "-----BEGIN CERTIFICATE-----\nYQ==\n-----END CERTIFICATE-----\n",
    )?;
    assert!(load_peer("https://localhost:8443/generate", Some(&invalid_path)).is_err());

    #[cfg(target_os = "linux")]
    {
        let fifo = dir.path().join("ca.fifo");
        rustix::fs::mkfifoat(rustix::fs::CWD, fifo.as_path(), rustix::fs::Mode::RUSR)?;
        let Err(error) = load_peer("https://localhost:8443/generate", Some(&fifo)) else {
            bail!("FIFO was accepted as a CA file");
        };
        assert_eq!(error.to_string(), "private adapter CA file must be regular");
    }

    let now = time::OffsetDateTime::now_utc();
    let valid = ca.acceptor(now + time::Duration::days(1))?;
    let expired = ca.acceptor(now - time::Duration::days(1))?;
    let (response, sent) =
        tls_exchange(&valid, "localhost", ClientKind::Peer(Some(&ca_path))).await?;
    assert!(sent);
    let response = response?;
    assert_eq!(response.status(), reqwest::StatusCode::OK);
    assert_eq!(response.text().await?, "ok");

    for (case, server, host, client) in [
        ("omitted CA", &valid, "localhost", ClientKind::Peer(None)),
        (
            "wrong CA",
            &valid,
            "localhost",
            ClientKind::Peer(Some(&wrong_path)),
        ),
        (
            "wrong hostname",
            &valid,
            "127.0.0.1",
            ClientKind::Peer(Some(&ca_path)),
        ),
        (
            "expired leaf",
            &expired,
            "localhost",
            ClientKind::Peer(Some(&ca_path)),
        ),
        ("ordinary client", &valid, "localhost", ClientKind::Default),
    ] {
        let (response, sent) = tls_exchange(server, host, client).await?;
        ensure!(!sent, "{case} reached the HTTP callback");
        let Err(error) = response else {
            bail!("{case} unexpectedly completed TLS");
        };
        ensure!(
            certificate_failure(&error),
            "{case} failed outside certificate verification: {error:?}"
        );
    }
    Ok(())
}

#[tokio::test]
async fn configured_peer_ca_verifies_real_tls_without_global_trust() -> anyhow::Result<()> {
    // Run the actual loader with a child-local pairing key. Do not mutate the
    // test runner's environment or use the test-only Peer constructor.
    if std::env::var_os(CHILD_ENV).is_some() {
        tokio::time::timeout(Duration::from_secs(15), tls_cases()).await??;
        println!("{TLS_WITNESS}");
        return Ok(());
    }
    let (_, module) = module_path!()
        .split_once("::")
        .context("fixture module path")?;
    let test = format!("{module}::configured_peer_ca_verifies_real_tls_without_global_trust");
    let mut command = tokio::process::Command::new(std::env::current_exe()?);
    command
        .args(["--exact", &test, "--nocapture"])
        .env(CHILD_ENV, "1")
        .env(KEY_ENV, URL_SAFE_NO_PAD.encode([17; 32]))
        .env_remove("SSL_CERT_FILE")
        .env_remove("SSL_CERT_DIR")
        .kill_on_drop(true);
    let output = tokio::time::timeout(Duration::from_secs(20), command.output()).await??;
    let stdout = String::from_utf8_lossy(&output.stdout);
    ensure!(
        output.status.success() && stdout.contains(TLS_WITNESS),
        "private CA TLS child failed or did not execute: {stdout}\n{}",
        String::from_utf8_lossy(&output.stderr)
    );
    Ok(())
}
