use std::sync::Arc;
use std::{fmt, io};

use hyper::Client;
use hyper_rustls::HttpsConnectorBuilder;
use ring::{
    rand::SystemRandom,
    signature::{RsaKeyPair, RSA_PKCS1_SHA256},
};
use serde::{de, Deserializer};
use serde::{Deserialize, Serialize};
use time::format_description::well_known::Iso8601;
use time::{Duration, OffsetDateTime};

use crate::Error;
/// Represents an access token. All access tokens are Bearer tokens.
///
/// Tokens should not be cached, the [`AuthenticationManager`] handles the correct caching
/// already.  Tokens are cheap to clone.
///
/// The token does not implement [`Display`] to avoid accidentally printing the token in log
/// files, likewise [`Debug`] does not expose the token value itself which is only available
/// using the [Token::`as_str`] method.
///
/// [`AuthenticationManager`]: crate::AuthenticationManager
/// [`Display`]: fmt::Display
/// [`Debug`]: fmt::Debug
#[derive(Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Deserialize, Serialize)]
pub struct Token {
    #[serde(flatten)]
    inner: Arc<InnerToken>,
}

impl Token {
    pub(crate) fn from_string(access_token: String, expires_in: Duration) -> Self {
        Token {
            inner: Arc::new(InnerToken {
                access_token,
                expires_at: OffsetDateTime::now_utc() + expires_in,
            }),
        }
    }
}

impl fmt::Debug for Token {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Token")
            .field("access_token", &"****")
            .field("expires_at", &self.inner.expires_at)
            .finish()
    }
}

#[derive(Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Deserialize, Serialize)]
struct InnerToken {
    #[serde(alias = "accessToken")]
    access_token: String,
    #[serde(
        deserialize_with = "deserialize_time",
        rename(deserialize = "expires_in"),
        alias = "expireTime"
    )]
    expires_at: OffsetDateTime,
}

impl Token {
    /// Define if the token has has_expired
    ///
    /// This takes an additional 30s margin to ensure the token can still be reasonably used
    /// instead of expiring right after having checked.
    ///
    /// Note:
    /// The official Python implementation uses 20s and states it should be no more than 30s.
    /// The official Go implementation uses 10s (0s for the metadata server).
    /// The docs state, the metadata server caches tokens until 5 minutes before expiry.
    /// We use 20s to be on the safe side.
    pub fn has_expired(&self) -> bool {
        self.inner.expires_at - Duration::seconds(20) <= OffsetDateTime::now_utc()
    }

    /// Get str representation of the token.
    pub fn as_str(&self) -> &str {
        &self.inner.access_token
    }

    /// Get expiry of token, if available
    pub fn expires_at(&self) -> OffsetDateTime {
        self.inner.expires_at
    }
}

/// An RSA PKCS1 SHA256 signer
pub struct Signer {
    key: RsaKeyPair,
    rng: SystemRandom,
}

impl Signer {
    pub(crate) fn new(pem_pkcs8: &str) -> Result<Self, Error> {
        let private_keys = rustls_pemfile::pkcs8_private_keys(&mut pem_pkcs8.as_bytes());

        let key = match private_keys {
            Ok(mut keys) if !keys.is_empty() => {
                keys.truncate(1);
                keys.remove(0)
            }
            Ok(_) => {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "Not enough private keys in PEM",
                )
                .into())
            }
            Err(_) => {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "Error reading key from PEM",
                )
                .into())
            }
        };

        Ok(Signer {
            key: RsaKeyPair::from_pkcs8(&key).map_err(|_| Error::SignerInit)?,
            rng: SystemRandom::new(),
        })
    }

    /// Sign the input message and return the signature
    pub fn sign(&self, input: &[u8]) -> Result<Vec<u8>, Error> {
        let mut signature = vec![0; self.key.public_modulus_len()];
        self.key
            .sign(&RSA_PKCS1_SHA256, &self.rng, input, &mut signature)
            .map_err(|_| Error::SignerFailed)?;
        Ok(signature)
    }
}

impl fmt::Debug for Signer {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Signer").finish()
    }
}

fn deserialize_time<'de, D>(deserializer: D) -> Result<OffsetDateTime, D::Error>
where
    D: Deserializer<'de>,
{
    // For impersonating service accounts, the token endpoint returns
    // expiresTime: [iso8601] instead of the usual expires_in: [seconds]
    #[derive(Deserialize)]
    #[serde(untagged)]
    enum SecondsOrTime {
        Seconds(i64),
        Time(String),
    }

    // First try to deserialize seconds
    let time_options: SecondsOrTime = Deserialize::deserialize(deserializer)?;

    match time_options {
        SecondsOrTime::Seconds(seconds_from_now) => {
            Ok(OffsetDateTime::now_utc() + Duration::seconds(seconds_from_now))
        }
        SecondsOrTime::Time(time) => {
            OffsetDateTime::parse(time.as_str(), &Iso8601::PARSING).map_err(de::Error::custom)
        }
    }
}

pub(crate) fn client() -> HyperClient {
    #[cfg(feature = "webpki-roots")]
    let https = HttpsConnectorBuilder::new().with_webpki_roots();
    #[cfg(not(feature = "webpki-roots"))]
    let https = HttpsConnectorBuilder::new().with_native_roots();

    Client::builder().build::<_, hyper::Body>(https.https_or_http().enable_http2().build())
}

pub(crate) type HyperClient =
    hyper::Client<hyper_rustls::HttpsConnector<hyper::client::HttpConnector>>;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_serialise() {
        let token = Token {
            inner: Arc::new(InnerToken {
                access_token: "abc123".to_string(),
                expires_at: OffsetDateTime::from_unix_timestamp(123).unwrap(),
            }),
        };
        let s = serde_json::to_string(&token).unwrap();

        assert_eq!(
            s,
            r#"{"access_token":"abc123","expires_at":[1970,1,0,2,3,0,0,0,0]}"#
        );
    }

    #[test]
    fn test_deserialise_with_time() {
        let s = r#"{"access_token":"abc123","expires_in":100}"#;
        let token: Token = serde_json::from_str(s).unwrap();
        let expires = OffsetDateTime::now_utc() + Duration::seconds(100);

        assert_eq!(token.as_str(), "abc123");

        // Testing time is always racy, give it 1s leeway.
        let expires_at = token.expires_at();
        assert!(expires_at < expires + Duration::seconds(1));
        assert!(expires_at > expires - Duration::seconds(1));
    }

    #[test]
    fn test_deserialize_real_life_camel_case() {
        let resp_body = "{\n  \"accessToken\": \"secret_token\",\n  \"expireTime\": \"2023-08-18T04:09:45Z\"\n}";
        let token: Token = serde_json::from_str(resp_body).expect("Failed to parse token");
        assert_eq!(token.as_str(), "secret_token");
    }
}
