//! TLS with servers that show a certificate of their own making, as RDP
//! servers and VNC servers mostly do. The first one seen is kept by the
//! caller and any other refused after, as ssh keeps host keys; one an
//! authority vouches for is checked as usual.

use std::sync::{Arc, Mutex};

use sha2::{Digest as _, Sha256};
use tokio::io::{AsyncRead, AsyncWrite};

use crate::{Error, Target};

pub(crate) type TlsStream<S> = ironrdp_tls::TlsStream<S>;

/// `stream` under TLS with `target`: the TLS stream, the server's
/// certificate, and the fingerprint of a certificate no authority vouches
/// for, seen for the first time (`known` is `None`), for the caller to keep.
pub(crate) async fn upgrade<S>(
    stream: S,
    target: &Target,
    known: Option<&str>,
) -> Result<(TlsStream<S>, x509_cert::Certificate, Option<String>), Error>
where
    S: Unpin + AsyncRead + AsyncWrite,
{
    let seen: Arc<Mutex<Option<String>>> = Arc::default();
    let callback: ironrdp_tls::CertificateValidationCallback = {
        let seen = seen.clone();
        let known = known.map(str::to_owned);
        Arc::new(move |der: &[u8], _endpoint: &str, why: &str| {
            let fp = fingerprint(der);
            tracing::debug!(fingerprint = %fp, %why, "certificate not vouched for");
            let ok = known.as_deref().is_none_or(|k| k.eq_ignore_ascii_case(&fp));
            *seen.lock().unwrap_or_else(|e| e.into_inner()) = Some(fp);
            ok
        })
    };
    let upgraded =
        ironrdp_tls::upgrade_with_certificate_validation_callback(stream, &target.host, callback)
            .await;
    let seen = seen.lock().unwrap_or_else(|e| e.into_inner()).take();
    match upgraded {
        Ok((tls, certificate)) => {
            let new = seen.filter(|_| known.is_none());
            Ok((tls, certificate, new))
        }
        Err(e) => Err(match (seen, known) {
            (Some(seen), Some(known)) if !seen.eq_ignore_ascii_case(known) => {
                Error::CertificateChanged {
                    host: target.host.clone(),
                    seen,
                }
            }
            _ => Error::other(format!("TLS with {}: {e}", target.host)),
        }),
    }
}

/// SHA-256 of a certificate, as `openssl x509 -fingerprint -sha256` shows
/// it.
pub(crate) fn fingerprint(der: &[u8]) -> String {
    Sha256::digest(der)
        .iter()
        .map(|b| format!("{b:02X}"))
        .collect::<Vec<_>>()
        .join(":")
}
