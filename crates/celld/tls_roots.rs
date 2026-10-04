//! The roots a Worker's outbound TLS trusts: Mozilla's, compiled in, plus the
//! operator's own CA bundle (`CELLD_EXTRA_CA_FILE`).
//!
//! The compiled-in set is deliberate: a downloaded celld reaches TLS hosts on
//! a machine with no `/etc/ssl/certs`, and a Worker trusts the same public
//! roots on every node. A private network (a company's CA, a TLS-inspecting
//! proxy) needs more anchors, so the operator names a PEM bundle and its
//! certificates join that set; they never replace it. The file is read and
//! checked once, before the node starts, and a bad bundle stops the process
//! with its reason.
//!
//! `SSL_CERT_FILE` is not read. It is OpenSSL's variable, it replaces the
//! trust store instead of adding to it, and hosts and shells set it for their
//! own tools, so honouring it would let the host change what a Worker trusts
//! without anyone naming celld. celld's settings are its own variables,
//! checked at start; `CELLD_EXTRA_CA_FILE=$SSL_CERT_FILE` says it on purpose.
//!
//! Three paths use these roots: a Worker's `fetch` (egress.rs), its outbound
//! WebSockets (ws_client.rs, which also carries the managed control plane's
//! presence socket), and `connect()` with TLS (js/tcp.rs). celld's other
//! clients (the bucket, telemetry, peers) are not affected.

use anyhow::{anyhow, bail};
use rustls::pki_types::pem::{PemObject as _, SectionKind};
use rustls::pki_types::{CertificateDer, TrustAnchor};
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

pub const VARIABLE: &str = "CELLD_EXTRA_CA_FILE";

/// The largest bundle celld reads. A distribution's whole bundle (about 150
/// roots in about 220 KB) fits, so an operator can name the file the system
/// already merges the company's CA into.
pub const EXTRA_CA_FILE_MAX_BYTES: u64 = 1024 * 1024;

/// The most certificates a bundle may hold. A handshake searches the roots
/// one by one for its issuer, so the bound keeps that search short.
pub const EXTRA_CA_MAX_CERTIFICATES: usize = 512;

/// Why a bundle was refused. `load` puts the variable and the path in front.
#[derive(Debug)]
enum ExtraCaError {
    Read(std::io::Error),
    TooLarge,
    /// No `CERTIFICATE` section: an empty file, or another format (DER, or
    /// OpenSSL's `TRUSTED CERTIFICATE`, which carries trust settings celld
    /// would not apply).
    NoCertificates,
    TooManyCertificates,
    /// Broken PEM: a section without its end line, or bad base64.
    Pem(rustls::pki_types::pem::Error),
    /// A section other than a certificate. A private key here means the
    /// operator named the wrong file, so it is refused rather than skipped.
    NotACertificate(&'static str),
    /// A certificate rustls cannot read as a trust anchor. `certificate`
    /// counts from 1, in file order.
    Unusable {
        certificate: usize,
        error: rustls::Error,
    },
}

impl std::fmt::Display for ExtraCaError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ExtraCaError::Read(error) => write!(f, "cannot be read: {error}"),
            ExtraCaError::TooLarge => {
                write!(f, "is larger than {EXTRA_CA_FILE_MAX_BYTES} bytes")
            }
            ExtraCaError::NoCertificates => f.write_str("holds no PEM certificate"),
            ExtraCaError::TooManyCertificates => {
                write!(
                    f,
                    "holds more than {EXTRA_CA_MAX_CERTIFICATES} certificates"
                )
            }
            ExtraCaError::Pem(error) => write!(f, "is not valid PEM: {error}"),
            ExtraCaError::NotACertificate(what) => {
                write!(f, "holds {what}; it must hold certificates only")
            }
            ExtraCaError::Unusable { certificate, error } => {
                write!(
                    f,
                    "certificate {certificate} cannot be a trust anchor: {error}"
                )
            }
        }
    }
}

impl std::error::Error for ExtraCaError {}

/// A checked bundle: each certificate as given, for reqwest, and as the trust
/// anchor rustls made of it.
#[derive(Debug)]
struct ExtraRoots {
    certificates: Vec<CertificateDer<'static>>,
    anchors: Vec<TrustAnchor<'static>>,
}

fn describe(kind: SectionKind) -> &'static str {
    match kind {
        SectionKind::RsaPrivateKey | SectionKind::PrivateKey | SectionKind::EcPrivateKey => {
            "a private key"
        }
        SectionKind::PublicKey => "a public key",
        SectionKind::Crl => "a certificate revocation list",
        SectionKind::Csr => "a certificate request",
        _ => "a section that is not a certificate",
    }
}

/// Check a PEM bundle. Text outside the sections (the comments some bundles
/// carry) is ignored, as every PEM reader does.
fn parse(pem: &[u8]) -> Result<ExtraRoots, ExtraCaError> {
    let mut store = rustls::RootCertStore::empty();
    let mut certificates = Vec::new();
    for item in <(SectionKind, Vec<u8>)>::pem_slice_iter(pem) {
        let (kind, der) = item.map_err(ExtraCaError::Pem)?;
        if kind != SectionKind::Certificate {
            return Err(ExtraCaError::NotACertificate(describe(kind)));
        }
        if certificates.len() == EXTRA_CA_MAX_CERTIFICATES {
            return Err(ExtraCaError::TooManyCertificates);
        }
        let der = CertificateDer::from(der);
        store
            .add(der.clone())
            .map_err(|error| ExtraCaError::Unusable {
                certificate: certificates.len() + 1,
                error,
            })?;
        certificates.push(der);
    }
    if certificates.is_empty() {
        return Err(ExtraCaError::NoCertificates);
    }
    Ok(ExtraRoots {
        certificates,
        anchors: store.roots,
    })
}

/// Read a bundle, refusing one past the size limit without reading the rest.
#[allow(clippy::disallowed_methods)] // The operator's configuration file, read once at start; not node storage.
fn read(path: &Path) -> Result<Vec<u8>, ExtraCaError> {
    use std::io::Read as _;
    let file = std::fs::File::open(path).map_err(ExtraCaError::Read)?;
    let mut bytes = Vec::new();
    // One byte past the limit tells a file at the limit from a larger one.
    file.take(EXTRA_CA_FILE_MAX_BYTES + 1)
        .read_to_end(&mut bytes)
        .map_err(ExtraCaError::Read)?;
    if bytes.len() as u64 > EXTRA_CA_FILE_MAX_BYTES {
        return Err(ExtraCaError::TooLarge);
    }
    Ok(bytes)
}

struct Configured {
    path: PathBuf,
    roots: ExtraRoots,
}

static CONFIGURED: OnceLock<Option<Configured>> = OnceLock::new();

fn from_env() -> anyhow::Result<Option<Configured>> {
    let Some(path) = std::env::var_os(VARIABLE) else {
        return Ok(None);
    };
    if path.is_empty() {
        bail!("{VARIABLE} is set but names no file");
    }
    let path = PathBuf::from(path);
    let roots = read(&path)
        .and_then(|pem| parse(&pem))
        .map_err(|error| anyhow!("{VARIABLE} {} {error}", path.display()))?;
    Ok(Some(Configured { path, roots }))
}

/// Read and check `CELLD_EXTRA_CA_FILE` once. `env_vars::validate` calls this
/// before anything starts, so a bad bundle stops the process, and every TLS
/// client built later trusts the bundle as it was at start.
pub fn load() -> anyhow::Result<()> {
    if CONFIGURED.get().is_none() {
        let configured = from_env()?;
        // A racing first call read the same variable; either value is it.
        let _ = CONFIGURED.set(configured);
    }
    Ok(())
}

fn configured() -> Option<&'static Configured> {
    CONFIGURED
        .get_or_init(|| from_env().expect("validated CELLD_EXTRA_CA_FILE"))
        .as_ref()
}

/// The operator's extra roots as certificates, for reqwest. Empty when the
/// variable is unset.
pub fn extra_certificates() -> &'static [CertificateDer<'static>] {
    configured().map_or(&[], |configured| &configured.roots.certificates)
}

/// Mozilla's roots plus the operator's, for a rustls client.
pub fn store() -> rustls::RootCertStore {
    store_with(configured().map(|configured| &configured.roots))
}

fn store_with(extra: Option<&ExtraRoots>) -> rustls::RootCertStore {
    let mut roots = webpki_roots::TLS_SERVER_ROOTS.to_vec();
    if let Some(extra) = extra {
        roots.extend(extra.anchors.iter().cloned());
    }
    rustls::RootCertStore { roots }
}

/// Say at node start how many roots the operator added, so a node that
/// cannot reach a private host shows whether it was given the CA.
pub fn log_configuration() {
    if let Some(configured) = configured() {
        tracing::info!(
            event = "tls_extra_roots",
            file = %configured.path.display(),
            roots = configured.roots.certificates.len(),
            "outbound TLS trusts the operator's roots beside Mozilla's"
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ca_pem(name: &str) -> String {
        let mut params = rcgen::CertificateParams::new(Vec::<String>::new()).unwrap();
        params.is_ca = rcgen::IsCa::Ca(rcgen::BasicConstraints::Unconstrained);
        params
            .distinguished_name
            .push(rcgen::DnType::CommonName, name);
        let key = rcgen::KeyPair::generate().unwrap();
        params.self_signed(&key).unwrap().pem()
    }

    #[test]
    fn valid_bundles() {
        let one = parse(ca_pem("one").as_bytes()).unwrap();
        assert_eq!(one.certificates.len(), 1);
        assert_eq!(one.anchors.len(), 1);

        // Comments between sections, as some distributions' bundles carry.
        let bundle = format!("# a\n{}\n# b\n{}", ca_pem("a"), ca_pem("b"));
        let two = parse(bundle.as_bytes()).unwrap();
        assert_eq!(two.certificates.len(), 2);
        assert_eq!(two.anchors.len(), 2);

        // The bundle joins Mozilla's roots; it does not replace them.
        let mozilla = webpki_roots::TLS_SERVER_ROOTS.len();
        assert_eq!(store_with(None).roots.len(), mozilla);
        assert_eq!(store_with(Some(&two)).roots.len(), mozilla + 2);
    }

    #[test]
    fn empty_bundles() {
        for empty in ["", "\n\n", "# only a comment\n"] {
            assert!(
                matches!(parse(empty.as_bytes()), Err(ExtraCaError::NoCertificates)),
                "{empty:?}"
            );
        }
        // A section celld does not read is not a certificate it trusts.
        let trusted = ca_pem("t").replace("CERTIFICATE", "TRUSTED CERTIFICATE");
        assert!(matches!(
            parse(trusted.as_bytes()),
            Err(ExtraCaError::NoCertificates)
        ));
    }

    #[test]
    fn malformed_bundles() {
        let pem = ca_pem("m");

        let unterminated = pem.replace("-----END CERTIFICATE-----", "");
        assert!(matches!(
            parse(unterminated.as_bytes()),
            Err(ExtraCaError::Pem(_))
        ));

        let bad_base64 = pem.replacen("MII", "M!I", 1);
        assert!(matches!(
            parse(bad_base64.as_bytes()),
            Err(ExtraCaError::Pem(_))
        ));

        // Well-formed PEM around bytes that are not a certificate.
        let not_der = "-----BEGIN CERTIFICATE-----\naGVsbG8=\n-----END CERTIFICATE-----\n";
        let bundle = format!("{pem}{not_der}");
        let error = parse(bundle.as_bytes()).unwrap_err();
        assert!(
            matches!(error, ExtraCaError::Unusable { certificate: 2, .. }),
            "{error}"
        );

        let key = rcgen::KeyPair::generate().unwrap().serialize_pem();
        let error = parse(format!("{pem}{key}").as_bytes()).unwrap_err();
        assert!(
            matches!(error, ExtraCaError::NotACertificate("a private key")),
            "{error}"
        );
    }

    #[test]
    fn oversized_bundles() {
        let pem = ca_pem("o");
        let too_many = pem.repeat(EXTRA_CA_MAX_CERTIFICATES + 1);
        assert!((too_many.len() as u64) < EXTRA_CA_FILE_MAX_BYTES);
        assert!(matches!(
            parse(too_many.as_bytes()),
            Err(ExtraCaError::TooManyCertificates)
        ));
        let at_limit = pem.repeat(EXTRA_CA_MAX_CERTIFICATES);
        assert_eq!(
            parse(at_limit.as_bytes()).unwrap().certificates.len(),
            EXTRA_CA_MAX_CERTIFICATES
        );

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("bundle.pem");
        let mut big = pem.clone().into_bytes();
        big.resize(EXTRA_CA_FILE_MAX_BYTES as usize + 1, b'\n');
        write(&path, &big);
        assert!(matches!(read(&path), Err(ExtraCaError::TooLarge)));
        big.truncate(EXTRA_CA_FILE_MAX_BYTES as usize);
        write(&path, &big);
        assert_eq!(parse(&read(&path).unwrap()).unwrap().certificates.len(), 1);

        assert!(matches!(
            read(&dir.path().join("absent.pem")),
            Err(ExtraCaError::Read(_))
        ));
    }

    #[allow(clippy::disallowed_methods)] // A test's scratch file.
    fn write(path: &Path, bytes: &[u8]) {
        std::fs::write(path, bytes).unwrap();
    }
}
