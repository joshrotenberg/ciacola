//! Provider credentials consumed from inherited descriptors at startup.
//!
//! The descriptor number may appear in the environment; the credential may
//! not. This module runs from synchronous `main`, before Tokio creates worker
//! threads and before a provider child can exist. Values stay in redacted
//! memory and are moved directly into the provider adapters.

use std::ffi::OsString;
use std::fmt;
use std::sync::Arc;

use crate::operator_auth::OPERATOR_TOKEN_FD_ENV;

pub const CLAUDE_TOKEN_FD_ENV: &str = "CIACOLA_CLAUDE_TOKEN_FD";
pub const CODEX_TOKEN_FD_ENV: &str = "CIACOLA_CODEX_TOKEN_FD";
const MAX_TOKEN_BYTES: u64 = 65_536;

/// One provider credential, deliberately opaque to formatting and logs.
#[derive(Clone)]
pub struct ProviderToken(Arc<str>);

impl fmt::Debug for ProviderToken {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("ProviderToken([REDACTED])")
    }
}

impl ProviderToken {
    fn parse(bytes: Vec<u8>, owner: &'static str) -> Result<Self, ProviderTokenError> {
        let value = String::from_utf8(bytes)
            .map_err(|_| ProviderTokenError(format!("{owner} credential must be valid UTF-8")))?;
        if value.is_empty() {
            return Err(ProviderTokenError(format!(
                "{owner} credential descriptor was empty"
            )));
        }
        if value.contains('\0') {
            return Err(ProviderTokenError(format!(
                "{owner} credential must not contain a NUL byte"
            )));
        }
        Ok(Self(Arc::from(value)))
    }

    /// Move the credential into its provider adapter.
    pub fn into_string(self) -> String {
        self.0.to_string()
    }
}

/// The two optional provider credentials accepted by this binary.
#[derive(Debug, Clone, Default)]
pub struct ProviderCredentials {
    pub claude: Option<ProviderToken>,
    pub codex: Option<ProviderToken>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProviderTokenError(String);

impl fmt::Display for ProviderTokenError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}

impl std::error::Error for ProviderTokenError {}

/// Refuse descriptor aliasing before any reader closes its descriptor.
///
/// Without this preflight, two credential variables naming the same file
/// descriptor make the first reader consume and close the source, and the
/// second failure depends on call order. The operator and both providers are
/// one startup authority set, so aliases are always configuration errors.
pub fn validate_distinct_descriptors() -> Result<(), ProviderTokenError> {
    let values = [
        (
            OPERATOR_TOKEN_FD_ENV,
            std::env::var_os(OPERATOR_TOKEN_FD_ENV),
        ),
        (CLAUDE_TOKEN_FD_ENV, std::env::var_os(CLAUDE_TOKEN_FD_ENV)),
        (CODEX_TOKEN_FD_ENV, std::env::var_os(CODEX_TOKEN_FD_ENV)),
    ];
    validate_descriptor_values(&values)
}

/// Consume provider credentials before the async runtime exists.
pub fn take_from_environment() -> Result<ProviderCredentials, ProviderTokenError> {
    let claude = std::env::var_os(CLAUDE_TOKEN_FD_ENV);
    let codex = std::env::var_os(CODEX_TOKEN_FD_ENV);

    // SAFETY: `main` calls this before constructing Tokio or starting any
    // other application thread. Only descriptor metadata is removed; the
    // secret itself was never an environment value.
    unsafe {
        std::env::remove_var(CLAUDE_TOKEN_FD_ENV);
        std::env::remove_var(CODEX_TOKEN_FD_ENV);
    }

    Ok(ProviderCredentials {
        claude: claude
            .map(|raw| read_token_descriptor(raw, CLAUDE_TOKEN_FD_ENV, "Claude"))
            .transpose()?,
        codex: codex
            .map(|raw| read_token_descriptor(raw, CODEX_TOKEN_FD_ENV, "Codex"))
            .transpose()?,
    })
}

fn validate_descriptor_values(
    values: &[(&'static str, Option<OsString>)],
) -> Result<(), ProviderTokenError> {
    let mut parsed = Vec::new();
    for (name, value) in values {
        let Some(value) = value else { continue };
        let descriptor = parse_descriptor(value.clone(), name)?;
        if let Some((prior, _)) = parsed
            .iter()
            .find(|(_, prior_descriptor)| *prior_descriptor == descriptor)
        {
            return Err(ProviderTokenError(format!(
                "{name} and {prior} must not name the same descriptor"
            )));
        }
        parsed.push((*name, descriptor));
    }
    Ok(())
}

fn parse_descriptor(raw: OsString, name: &str) -> Result<i32, ProviderTokenError> {
    let descriptor: i32 = raw
        .into_string()
        .map_err(|_| ProviderTokenError(format!("{name} must be an integer")))?
        .parse()
        .map_err(|_| ProviderTokenError(format!("{name} must be an integer")))?;
    if descriptor < 3 {
        return Err(ProviderTokenError(format!(
            "{name} must not replace stdin, stdout, or stderr"
        )));
    }
    Ok(descriptor)
}

#[cfg(unix)]
fn read_token_descriptor(
    raw: OsString,
    name: &'static str,
    owner: &'static str,
) -> Result<ProviderToken, ProviderTokenError> {
    use std::io::Read;
    use std::os::fd::FromRawFd;

    let descriptor = parse_descriptor(raw, name)?;
    // Validate and duplicate before constructing a File from untrusted
    // process metadata. CLOEXEC keeps the duplicate out of any accidental
    // child, and closing the original prevents later inheritance.
    let duplicate = unsafe { libc::fcntl(descriptor, libc::F_DUPFD_CLOEXEC, 3) };
    if duplicate < 0 {
        return Err(ProviderTokenError(format!(
            "{name} is not an open readable descriptor"
        )));
    }
    // SAFETY: `duplicate` is a fresh descriptor returned by fcntl and is now
    // uniquely owned by File. The caller-owned original is no longer needed.
    unsafe {
        libc::close(descriptor);
    }
    let file = unsafe { std::fs::File::from_raw_fd(duplicate) };
    let mut bytes = Vec::new();
    file.take(MAX_TOKEN_BYTES + 1)
        .read_to_end(&mut bytes)
        .map_err(|_| ProviderTokenError(format!("could not read {name}")))?;
    if bytes.len() as u64 > MAX_TOKEN_BYTES {
        return Err(ProviderTokenError(format!(
            "{owner} credential is unexpectedly large"
        )));
    }
    while matches!(bytes.last(), Some(b'\n' | b'\r')) {
        bytes.pop();
    }
    ProviderToken::parse(bytes, owner)
}

#[cfg(not(unix))]
fn read_token_descriptor(
    _raw: OsString,
    name: &'static str,
    _owner: &'static str,
) -> Result<ProviderToken, ProviderTokenError> {
    Err(ProviderTokenError(format!(
        "{name} is not supported on this platform; use a separately logged-in provider home"
    )))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(unix)]
    use std::io::{Seek, Write};
    #[cfg(unix)]
    use std::os::fd::{AsRawFd, IntoRawFd};

    #[test]
    fn descriptor_metadata_is_validated_as_one_authority_set() {
        let duplicate = validate_descriptor_values(&[
            (OPERATOR_TOKEN_FD_ENV, Some(OsString::from("9"))),
            (CLAUDE_TOKEN_FD_ENV, Some(OsString::from("9"))),
        ])
        .expect_err("aliased secret descriptors must fail");
        assert!(duplicate.to_string().contains("same descriptor"));

        let standard =
            validate_descriptor_values(&[(CODEX_TOKEN_FD_ENV, Some(OsString::from("2")))])
                .expect_err("stdio is never a secret descriptor");
        assert!(standard.to_string().contains("stdin, stdout, or stderr"));
    }

    #[test]
    fn debug_never_discloses_provider_credentials() {
        let token = ProviderToken::parse(b"provider-secret-value".to_vec(), "test")
            .expect("provider token");
        assert_eq!(format!("{token:?}"), "ProviderToken([REDACTED])");
        assert!(
            !format!(
                "{:?}",
                ProviderCredentials {
                    claude: Some(token),
                    codex: None,
                }
            )
            .contains("provider-secret-value")
        );
    }

    #[cfg(unix)]
    #[test]
    fn credential_is_consumed_from_a_bounded_descriptor() {
        let (reader, mut writer) = std::os::unix::net::UnixStream::pair().expect("socket pair");
        writer
            .write_all(b"provider-secret-value\n")
            .expect("write token");
        drop(writer);
        let descriptor = reader.into_raw_fd();

        let token = read_token_descriptor(
            OsString::from(descriptor.to_string()),
            CLAUDE_TOKEN_FD_ENV,
            "Claude",
        )
        .expect("read descriptor");
        assert_eq!(token.into_string(), "provider-secret-value");
        // The whole-binary provider_environment test observes this descriptor
        // as closed from the eventual provider child. Checking the numeric fd
        // here is racy because another parallel test may immediately reuse it.
    }

    #[cfg(unix)]
    #[test]
    fn a_closed_descriptor_is_refused_without_assuming_file_ownership() {
        let file = tempfile::tempfile().expect("temporary credential source");
        let descriptor = file.as_raw_fd();
        drop(file);

        let error = read_token_descriptor(
            OsString::from(descriptor.to_string()),
            CODEX_TOKEN_FD_ENV,
            "Codex",
        )
        .expect_err("closed descriptor");
        assert!(
            error
                .to_string()
                .contains("not an open readable descriptor")
        );
    }

    #[cfg(unix)]
    #[test]
    fn oversized_and_non_utf8_credentials_are_refused() {
        let mut oversized = tempfile::tempfile().expect("oversized credential source");
        oversized
            .write_all(&vec![b'x'; (MAX_TOKEN_BYTES + 1) as usize])
            .expect("write oversized credential");
        oversized.rewind().expect("rewind oversized credential");
        let error = read_token_descriptor(
            OsString::from(oversized.into_raw_fd().to_string()),
            CLAUDE_TOKEN_FD_ENV,
            "Claude",
        )
        .expect_err("oversized credential");
        assert!(error.to_string().contains("unexpectedly large"));

        let mut non_utf8 = tempfile::tempfile().expect("non-UTF credential source");
        non_utf8
            .write_all(&[0xff, 0xfe])
            .expect("write non-UTF credential");
        non_utf8.rewind().expect("rewind non-UTF credential");
        let error = read_token_descriptor(
            OsString::from(non_utf8.into_raw_fd().to_string()),
            CODEX_TOKEN_FD_ENV,
            "Codex",
        )
        .expect_err("non-UTF credential");
        assert!(error.to_string().contains("valid UTF-8"));
    }

    #[test]
    fn empty_and_nul_credentials_are_refused() {
        assert!(ProviderToken::parse(Vec::new(), "test").is_err());
        assert!(ProviderToken::parse(b"secret\0tail".to_vec(), "test").is_err());
    }
}
