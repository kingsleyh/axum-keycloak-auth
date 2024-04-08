use std::collections::HashMap;
use std::sync::Arc;

use http::HeaderMap;
use http::HeaderValue;
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use serde_with::{serde_as, OneOrMany};
use snafu::ResultExt;
use tracing::debug;

use crate::error::DecodeHeaderSnafu;
use crate::error::DecodeSnafu;
use crate::instance::KeycloakAuthInstance;
use crate::role::ExpectRoles;
use crate::role::KeycloakRole;
use crate::role::NumRoles;

use super::{error::AuthError, role::ExtractRoles, role::Role};

pub(crate) struct RawToken<'a>(pub(crate) &'a str);

pub(crate) fn extract_jwt_token(
    headers: &HeaderMap<HeaderValue>,
    cookie_name: Option<String>,
) -> Result<String, AuthError> {
    let token = if let Some(name) = cookie_name {
        if let Some(token) = optionally_extract_jwt_from_cookie(headers, name.as_str()) {
            token.to_owned()
        } else {
            extract_jwt_token_from_bearer_header(headers)?.to_owned()
        }
    } else {
        extract_jwt_token_from_bearer_header(headers)?.to_owned()
    };

    Ok(token)
}

fn optionally_extract_jwt_from_cookie<'a>(
    headers: &'a HeaderMap<HeaderValue>,
    cookie_name: &'a str,
) -> Option<&'a str> {
    let cookie = headers.get(http::header::COOKIE)?;
    let cookie = cookie.to_str().ok()?;
    let cookie = cookie
        .split(';')
        .find(|cookie| cookie.starts_with(cookie_name))?;
    let cookie = cookie.split('=').nth(1)?;
    Some(cookie)
}

fn extract_jwt_token_from_bearer_header(
    headers: &HeaderMap<HeaderValue>,
) -> Result<&str, AuthError> {
    headers
        .get(http::header::AUTHORIZATION)
        .ok_or(AuthError::MissingAuthorizationHeader)?
        .to_str()
        .map_err(|err| AuthError::InvalidAuthorizationHeader {
            reason: err.to_string(),
        })?
        .strip_prefix("Bearer ")
        .ok_or(AuthError::MissingBearerToken)
}

pub(crate) async fn validate_raw_token<R, Extra>(
    kc_instance: &KeycloakAuthInstance,
    raw_token: RawToken<'_>,
    expected_audiences: &[String],
    persist_raw_claims: bool,
    required_roles: &[R],
) -> Result<
    (
        Option<HashMap<String, serde_json::Value>>,
        KeycloakToken<R, Extra>,
    ),
    AuthError,
>
where
    R: Role,
    Extra: DeserializeOwned + Clone,
{
    let header = raw_token.decode_header()?;

    // First decode. This may fail if known decoding keys are out of date (Keycloak server changed).
    let mut raw_claims = {
        let decoding_keys = kc_instance.decoding_keys().await;
        raw_token.decode(&header, expected_audiences, decoding_keys.iter())
    };

    if raw_claims.is_err() {
        // Reload decoding keys. This may delay handling of the request in flight by a substantial amount of time
        // but may allow us to acknowledge it in the end without rejecting the call immediately,
        // which would then require a retry from our caller!
        #[allow(clippy::unwrap_used)]
        let retry = match raw_claims.as_ref().unwrap_err() {
            AuthError::NoDecodingKeys | AuthError::Decode { source: _ } => {
                if kc_instance.discovery.is_pending() {
                    kc_instance.discovery.notified().await;
                } else {
                    kc_instance
                        .discovery
                        .dispatch(kc_instance.oidc_discovery_endpoint.clone())
                        .await
                        .expect("No Join error");
                }
                true
            }
            _ => false,
        };

        // Second decode
        if retry {
            let decoding_keys = kc_instance.decoding_keys().await;
            raw_claims = raw_token.decode(&header, expected_audiences, decoding_keys.iter());
        }
    }

    let raw_claims = raw_claims?;

    let raw_claims_clone = match persist_raw_claims {
        true => Some(raw_claims.clone()),
        false => None,
    };
    let value = serde_json::Value::from_iter(raw_claims.into_iter());

    let standard_claims = serde_json::from_value(value).map_err(|err| AuthError::JsonParse {
        source: Arc::new(err),
    })?;
    let keycloak_token = KeycloakToken::<R, Extra>::parse(standard_claims)?;
    keycloak_token.assert_not_expired()?;
    keycloak_token.expect_roles(required_roles)?;
    Ok((raw_claims_clone, keycloak_token))
}
impl<'a> RawToken<'a> {
    pub fn decode_header(&self) -> Result<jsonwebtoken::Header, AuthError> {
        let jwt_header = jsonwebtoken::decode_header(self.0).context(DecodeHeaderSnafu {})?;
        tracing::debug!(?jwt_header, "Decoded JWT header");
        Ok(jwt_header)
    }

    pub fn decode<'d>(
        &self,
        header: &jsonwebtoken::Header,
        expected_audiences: &[String],
        decoding_keys: impl Iterator<Item = &'d jsonwebtoken::DecodingKey>,
    ) -> Result<RawClaims, AuthError> {
        let mut validation = jsonwebtoken::Validation::new(header.alg);
        validation.set_audience(expected_audiences);

        let mut token_data: Result<
            jsonwebtoken::TokenData<HashMap<String, serde_json::Value>>,
            AuthError,
        > = Err(AuthError::NoDecodingKeys);
        for key in decoding_keys {
            token_data =
                jsonwebtoken::decode::<RawClaims>(self.0, key, &validation).context(DecodeSnafu {});
            if token_data.is_ok() {
                break;
            }
        }
        let token_data = token_data?;
        let raw_claims = token_data.claims;
        debug!(?raw_claims, "Decoded JWT data");

        Ok(raw_claims)
    }
}

pub type RawClaims = HashMap<String, serde_json::Value>;

#[serde_as]
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StandardClaims<Extra> {
    /// Expiration time (unix timestamp).
    pub exp: i64,
    /// Issued at time (unix timestamp).
    pub iat: i64,
    /// JWT ID (unique identifier for this token).
    pub jti: String,
    /// Issuer (who created and signed this token). This is the UUID which uniquely identifies this user inside Keycloak.
    pub iss: String,
    /// Audience (who or what the token is intended for).
    #[serde_as(deserialize_as = "OneOrMany<_>")]
    pub aud: Vec<String>,
    /// Subject (whom the token refers to).
    pub sub: String,
    /// Type of token.
    pub typ: String,
    /// Authorized party (the party to which this token was issued).
    pub azp: String,

    /// Keycloak: Optional realm roles from Keycloak.
    pub realm_access: Option<RealmAccess>,
    /// Keycloak: Optional client roles from Keycloak.
    pub resource_access: Option<ResourceAccess>,

    #[serde(flatten)]
    pub extra: Extra,
}

/// Access details.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Access {
    /// A list of role names.
    pub roles: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RealmAccess(pub Access);

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ResourceAccess(pub HashMap<String, Access>);

impl NumRoles for RealmAccess {
    fn num_roles(&self) -> usize {
        self.0.roles.len()
    }
}

impl NumRoles for ResourceAccess {
    fn num_roles(&self) -> usize {
        self.0.values().map(|access| access.roles.len()).sum()
    }
}

impl<R: Role> ExtractRoles<R> for RealmAccess {
    fn extract_roles(self, target: &mut Vec<KeycloakRole<R>>) {
        for role in self.0.roles {
            target.push(KeycloakRole::Realm { role: role.into() });
        }
    }
}

impl<R: Role> ExtractRoles<R> for ResourceAccess {
    fn extract_roles(self, target: &mut Vec<KeycloakRole<R>>) {
        for (res_name, access) in &self.0 {
            for role in &access.roles {
                target.push(KeycloakRole::Client {
                    client: res_name.to_owned(),
                    role: role.to_owned().into(),
                });
            }
        }
    }
}

#[derive(Debug, PartialEq, Clone)]
pub struct KeycloakToken<R, Extra = ProfileAndEmail>
where
    R: Role,
    Extra: DeserializeOwned + Clone,
{
    /// Expiration time (UTC).
    pub expires_at: time::OffsetDateTime,
    /// Issued at time (UTC).
    pub issued_at: time::OffsetDateTime,
    /// JWT ID (unique identifier for this token).
    pub jwt_id: String,
    /// Issuer (who created and signed this token).
    pub issuer: String,
    /// Audience (who or what the token is intended for).
    pub audience: Vec<String>,
    /// Subject (whom the token refers to). This is the UUID which uniquely identifies this user inside Keycloak.
    pub subject: String,
    /// Authorized party (the party to which this token was issued).
    pub authorized_party: String,

    // Keycloak: Roles of the user.
    pub roles: Vec<KeycloakRole<R>>,

    pub extra: Extra,
}

impl<R, Extra> KeycloakToken<R, Extra>
where
    R: Role,
    Extra: DeserializeOwned + Clone,
{
    pub(crate) fn parse(raw: StandardClaims<Extra>) -> Result<Self, AuthError> {
        Ok(Self {
            expires_at: time::OffsetDateTime::from_unix_timestamp(raw.exp).map_err(|err| {
                AuthError::InvalidToken {
                    reason: format!(
                        "Could not parse 'exp' (expires_at) field as unix timestamp: {err}"
                    ),
                }
            })?,
            issued_at: time::OffsetDateTime::from_unix_timestamp(raw.iat).map_err(|err| {
                AuthError::InvalidToken {
                    reason: format!(
                        "Could not parse 'iat' (issued_at) field as unix timestamp: {err}"
                    ),
                }
            })?,
            jwt_id: raw.jti,
            issuer: raw.iss,
            audience: raw.aud,
            subject: raw.sub,
            authorized_party: raw.azp,
            roles: {
                let mut roles = Vec::new();
                (raw.realm_access, raw.resource_access).extract_roles(&mut roles);
                roles
            },
            extra: raw.extra,
        })
    }

    pub fn is_expired(&self) -> bool {
        time::OffsetDateTime::now_utc() > self.expires_at
    }

    pub fn assert_not_expired(&self) -> Result<(), AuthError> {
        match self.is_expired() {
            true => Err(AuthError::TokenExpired),
            false => Ok(()),
        }
    }
}

impl<R, Extra> ExpectRoles<R> for KeycloakToken<R, Extra>
where
    R: Role,
    Extra: DeserializeOwned + Clone,
{
    type Rejection = AuthError;

    fn expect_roles<I: Into<R> + Clone>(&self, roles: &[I]) -> Result<(), Self::Rejection> {
        for expected in roles {
            let expected: R = expected.clone().into();
            if !self.roles.iter().any(|role| role.role() == &expected) {
                return Err(AuthError::MissingExpectedRole {
                    role: expected.to_string(),
                });
            }
        }
        Ok(())
    }

    fn not_expect_roles<I: Into<R> + Clone>(&self, roles: &[I]) -> Result<(), Self::Rejection> {
        for expected in roles {
            let expected: R = expected.clone().into();
            if let Some(_role) = self.roles.iter().find(|role| role.role() == &expected) {
                return Err(AuthError::UnexpectedRole);
            }
        }
        Ok(())
    }
}

#[derive(serde::Deserialize, Debug, Clone)]
pub struct Profile {
    /// Keycloak: First name.
    pub given_name: Option<String>,
    /// Keycloak: Combined name. Assume this to equal `format!("{given_name} {family name}")`.
    pub full_name: Option<String>,
    /// Keycloak: Last name.
    pub family_name: Option<String>,
    /// Keycloak: Username of the user.
    pub preferred_username: String,
}

#[derive(serde::Deserialize, Debug, Clone)]
pub struct Email {
    /// Keycloak: Email address of the user.
    pub email: String,
    /// Keycloak: Whether the users email is verified.
    pub email_verified: bool,
}

#[derive(serde::Deserialize, Debug, Clone)]
pub struct ProfileAndEmail {
    #[serde(flatten)]
    pub profile: Profile,
    #[serde(flatten)]
    pub email: Email,
}
