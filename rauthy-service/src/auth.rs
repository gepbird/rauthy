use crate::token_set::TokenSet;
use actix_web::dev::ServiceRequest;
use actix_web::http::header;
use actix_web::http::header::{HeaderMap, HeaderName, HeaderValue};
use actix_web::{web, Error, HttpMessage, HttpRequest, HttpResponse};
use jwt_simple::algorithms::{
    EdDSAKeyPairLike, EdDSAPublicKeyLike, RSAKeyPairLike, RSAPublicKeyLike,
};
use jwt_simple::claims;
use jwt_simple::prelude::*;
use rauthy_common::constants::{
    CACHE_NAME_12HR, CACHE_NAME_LOGIN_DELAY, COOKIE_MFA, IDX_JWKS, IDX_JWK_LATEST, IDX_LOGIN_TIME,
    TOKEN_API_KEY, TOKEN_BEARER, WEBAUTHN_REQ_EXP,
};
use rauthy_common::error_response::{ErrorResponse, ErrorResponseType};
use rauthy_common::password_hasher::HashPassword;
use rauthy_common::utils::{base64_url_encode, encrypt, get_client_ip, get_rand};
use rauthy_models::app_state::AppState;
use rauthy_models::entity::api_keys::{ApiKey, ApiKeyEntity};
use rauthy_models::entity::auth_codes::AuthCode;
use rauthy_models::entity::clients::Client;
use rauthy_models::entity::colors::ColorEntity;
use rauthy_models::entity::jwk::{Jwk, JwkKeyPair, JwkKeyPairType};
use rauthy_models::entity::principal::Principal;
use rauthy_models::entity::refresh_tokens::RefreshToken;
use rauthy_models::entity::scopes::Scope;
use rauthy_models::entity::sessions::{Session, SessionState};
use rauthy_models::entity::users::{AccountType, User};
use rauthy_models::entity::webauthn::{WebauthnCookie, WebauthnLoginReq};
use rauthy_models::language::Language;
use rauthy_models::request::{LoginRequest, LogoutRequest, TokenRequest};
use rauthy_models::response::{TokenInfo, Userinfo};
use rauthy_models::templates::LogoutHtml;
use rauthy_models::{
    sign_jwt, validate_jwt, AuthStep, AuthStepAwaitWebauthn, AuthStepLoggedIn, JwtAccessClaims,
    JwtAmrValue, JwtCommonClaims, JwtIdClaims, JwtRefreshClaims, JwtType,
};
use redhac::cache_del;
use redhac::{cache_get, cache_get_from, cache_get_value, cache_put};
use ring::digest;
use std::collections::{HashMap, HashSet};
use std::ops::{Add, Sub};
use std::str::FromStr;
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use time::OffsetDateTime;
use tracing::{debug, error, info, warn};

/// # Business logic for [POST /oidc/authorize](crate::handlers::post_authorize)
#[tracing::instrument(name = "post_authorize", skip_all, fields(client_id = req_data.client_id, email = req_data.email))]
pub async fn authorize(
    data: &web::Data<AppState>,
    req: &HttpRequest,
    req_data: LoginRequest,
    mut session: Session,
) -> Result<AuthStep, ErrorResponse> {
    // This Error must be the same if user does not exist AND passwords do not match to prevent
    // username enumeration
    let mut user = User::find_by_email(data, req_data.email)
        .await
        .map_err(|e| {
            error!("{:?}", e);
            // be careful, that this Err and the one in User::validate_password are exactly the same
            ErrorResponse::new(
                ErrorResponseType::Unauthorized,
                String::from("Invalid user credentials"),
            )
        })?;
    user.check_enabled()?;
    user.check_expired()?;
    let account_type = user.account_type();
    if account_type == AccountType::New {
        return Err(ErrorResponse::new(
            ErrorResponseType::Unauthorized,
            String::from("Invalid user credentials"),
        ));
    }

    let mfa_cookie =
        if let Ok(c) = WebauthnCookie::parse_validate(&req.cookie(COOKIE_MFA), &data.enc_keys) {
            if c.email == user.email && user.has_webauthn_enabled() {
                Some(c)
            } else {
                // If a possibly existing mfa cookie does not match the given email, or user has webauthn
                // disabled in the meantime -> ignore the cookie
                None
            }
        } else {
            None
        };

    // this allows a user without the mfa cookie to login anyway if it is an only passkey account
    // in this case, UV is always enforced, not matter what -> safe to login without cookie
    if req_data.password.is_none() && account_type != AccountType::Passkey && mfa_cookie.is_none() {
        return Err(ErrorResponse::new(
            ErrorResponseType::Unauthorized,
            String::from("Invalid user credentials"),
        ));
    }

    let has_password_been_hashed = if let Some(pwd) = req_data.password {
        match user.validate_password(data, pwd).await {
            Ok(_) => {
                // update user info
                // in case of webauthn login, the info will be updates in the auth finish step
                user.last_login = Some(OffsetDateTime::now_utc().unix_timestamp());
                user.last_failed_login = None;
                user.failed_login_attempts = None;
                user.save(data, None, None).await?;
            }
            Err(err) => {
                return Err(err);
            }
        }
        true
    } else {
        false
    };

    let client = Client::find(data, req_data.client_id).await?;

    // check allowed origin
    let header_origin = client.validate_origin(req, &data.listen_scheme, &data.public_url)?;

    // check requested challenge
    let challenge: Option<String> = req_data.code_challenge.clone();
    let mut challenge_method: Option<String> = None;
    // TODO would it be possible to omit a code challenge and skip it, even if the client should request it?
    // TODO -> double check, if the client has set code challenge? -> revert this logic and validate from client -> request
    if req_data.code_challenge.is_some() {
        if client.challenge.is_none() {
            return Err(ErrorResponse::new(
                ErrorResponseType::BadRequest,
                String::from("no 'code_challenge_method' allowed for this client"),
            ));
        }

        let method: String;
        if req_data.code_challenge_method.is_none() {
            method = String::from("plain");
        } else {
            match req_data.code_challenge_method.as_ref().unwrap().as_str() {
                "S256" => method = String::from("S256"),
                "plain" => method = String::from("plain"),
                _ => {
                    return Err(ErrorResponse::new(
                        ErrorResponseType::BadRequest,
                        String::from("invalid 'code_challenge_method"),
                    ))
                }
            }
        }

        if !client.challenge.as_ref().unwrap().contains(&method) {
            return Err(ErrorResponse::new(
                ErrorResponseType::BadRequest,
                format!(
                    "'code_challenge_method' '{}' is not allowed for this client",
                    method,
                ),
            ));
        }
        challenge_method = Some(method);
    }

    // add the timeout for mfa verification to the auth code lifetime
    let code_lifetime = if user.has_webauthn_enabled() {
        client.auth_code_lifetime + *WEBAUTHN_REQ_EXP as i32
    } else {
        client.auth_code_lifetime
    };

    // build authorization code
    let scopes = client.sanitize_login_scopes(&req_data.scopes)?;
    let code = AuthCode::new(
        user.id.clone(),
        client.id,
        Some(session.id.clone()),
        challenge,
        challenge_method,
        req_data.nonce,
        scopes,
        code_lifetime,
    );
    code.save(data).await?;

    // build location header
    let mut loc = format!("{}?code={}", req_data.redirect_uri, code.id);
    if let Some(state) = req_data.state {
        loc = format!("{}&state={}", loc, state);
    };

    // TODO double check that we do not have any problems with the direct webauthn login here
    // TODO should we allow to skip this step if set so in the config?
    // check if we need to validate the 2nd factor
    if user.has_webauthn_enabled() {
        session.set_mfa(data, true).await?;

        let step = AuthStepAwaitWebauthn {
            has_password_been_hashed,
            code: get_rand(48),
            header_csrf: Session::get_csrf_header(&session.csrf_token),
            header_origin,
            user_id: user.id.clone(),
            email: user.email,
            exp: *WEBAUTHN_REQ_EXP,
            session,
        };

        let login_req = WebauthnLoginReq {
            code: step.code.clone(),
            user_id: user.id,
            header_loc: loc,
            header_origin: step
                .header_origin
                .as_ref()
                .map(|h| h.1.to_str().unwrap().to_string()),
        };
        login_req.save(data).await?;

        Ok(AuthStep::AwaitWebauthn(step))
    } else {
        Ok(AuthStep::LoggedIn(AuthStepLoggedIn {
            has_password_been_hashed,
            header_loc: (header::LOCATION, HeaderValue::from_str(&loc).unwrap()),
            header_csrf: Session::get_csrf_header(&session.csrf_token),
            header_origin,
        }))
    }
}

// /// # Business logic for [POST /oidc/authorize/refresh](crate::handlers::post_authorize_refresh)
// pub async fn authorize_refresh(
//     data: &web::Data<AppState>,
//     session: Session,
//     client: Client,
//     header_origin: Option<(HeaderName, HeaderValue)>,
//     req_data: LoginRefreshRequest,
// ) -> Result<AuthStep, ErrorResponse> {
//     let user_id = session.user_id.as_ref().ok_or_else(|| {
//         ErrorResponse::new(
//             ErrorResponseType::Internal,
//             String::from("No linked user_id for already validated session"),
//         )
//     })?;
//     let user = User::find(data, user_id.clone()).await?;
//     user.check_enabled()?;
//
//     let scopes = client.sanitize_login_scopes(&req_data.scopes)?;
//     let code_lifetime = if user.has_webauthn_enabled() {
//         client.auth_code_lifetime + *WEBAUTHN_REQ_EXP as i32
//     } else {
//         client.auth_code_lifetime
//     };
//
//     let code = AuthCode::new(
//         user.id.clone(),
//         client.id,
//         Some(session.id.clone()),
//         req_data.code_challenge,
//         req_data.code_challenge_method,
//         req_data.nonce,
//         scopes,
//         code_lifetime,
//     );
//     code.save(data).await?;
//
//     // build location header
//     let header_loc = if let Some(s) = req_data.state {
//         format!("{}?code={}&state={}", req_data.redirect_uri, code.id, s)
//     } else {
//         format!("{}?code={}", req_data.redirect_uri, code.id)
//     };
//
//     // check if we need to validate the 2nd factor
//     if user.has_webauthn_enabled() && *SESSION_RENEW_MFA {
//         let step = AuthStepAwaitWebauthn {
//             has_password_been_hashed: false,
//             code: get_rand(48),
//             header_csrf: Session::get_csrf_header(&session.csrf_token),
//             header_origin,
//             user_id: user.id.clone(),
//             email: user.email,
//             exp: *WEBAUTHN_REQ_EXP,
//             session,
//         };
//
//         let login_req = WebauthnLoginReq {
//             code: step.code.clone(),
//             user_id: user.id,
//             header_loc,
//             header_origin: step
//                 .header_origin
//                 .as_ref()
//                 .map(|h| h.1.to_str().unwrap().to_string()),
//         };
//         login_req.save(data).await?;
//
//         Ok(AuthStep::AwaitWebauthn(step))
//     } else {
//         Ok(AuthStep::LoggedIn(AuthStepLoggedIn {
//             has_password_been_hashed: false,
//             header_loc: (
//                 header::LOCATION,
//                 HeaderValue::from_str(&header_loc).unwrap(),
//             ),
//             header_csrf: Session::get_csrf_header(&session.csrf_token),
//             header_origin,
//         }))
//     }
// }

/// Builds the access token for a user after all validation has been successful
#[allow(clippy::type_complexity)]
pub async fn build_access_token(
    user: Option<&User>,
    data: &web::Data<AppState>,
    client: &Client,
    lifetime: i64,
    scope: Option<String>,
    scope_customs: Option<(Vec<&Scope>, &Option<HashMap<String, Vec<u8>>>)>,
) -> Result<String, ErrorResponse> {
    let scope = if let Some(s) = scope {
        s
    } else {
        client.default_scopes.clone().replace(',', " ")
    };

    let mut custom_claims = JwtAccessClaims {
        typ: JwtType::Bearer,
        azp: client.id.to_string(),
        scope,
        allowed_origins: None,
        uid: None,
        preferred_username: None,
        roles: None,
        groups: None,
        custom: None,
    };

    // add user specific claims if available
    let mut sub = None;
    if let Some(user) = user {
        sub = Some(user.email.clone());
        custom_claims.preferred_username = Some(user.email.clone());
        custom_claims.uid = Some(user.id.clone());
        custom_claims.roles = Some(user.get_roles());

        if custom_claims.scope.contains("groups") {
            custom_claims.groups = Some(user.get_groups());
        }
    }

    if let Some((cust, user_attrs)) = scope_customs {
        let user_attrs = user_attrs.as_ref().unwrap();
        let mut attr = HashMap::with_capacity(cust.len());
        for c in cust {
            if let Some(csv) = &c.attr_include_access {
                let scopes = csv.split(',');
                for cust_name in scopes {
                    if let Some(value) = user_attrs.get(cust_name) {
                        let json = serde_json::from_slice(value.as_slice())
                            .expect("Converting cust user access attr to json");
                        attr.insert(cust_name.to_string(), json);
                    };
                }
            }
        }
        if !attr.is_empty() {
            custom_claims.custom = Some(attr);
        }
    }

    let mut claims = Claims::with_custom_claims(
        custom_claims,
        coarsetime::Duration::from_secs(lifetime as u64),
    )
    .with_issuer(data.issuer.clone())
    .with_audience(client.id.to_string());

    if let Some(sub) = sub {
        claims = claims.with_subject(sub);
    }

    sign_access_token(data, claims, client).await
}

/// Builds the id token for a user after all validation has been successful
#[allow(clippy::too_many_arguments, clippy::type_complexity)]
pub async fn build_id_token(
    user: &User,
    data: &web::Data<AppState>,
    client: &Client,
    lifetime: i64,
    nonce: Option<String>,
    scope: &str,
    scope_customs: Option<(Vec<&Scope>, &Option<HashMap<String, Vec<u8>>>)>,
    is_auth_code_flow: bool,
) -> Result<String, ErrorResponse> {
    let amr = match user.has_webauthn_enabled() {
        true => {
            if is_auth_code_flow {
                JwtAmrValue::Mfa.to_string()
            } else {
                JwtAmrValue::Pwd.to_string()
            }
        }
        false => JwtAmrValue::Pwd.to_string(),
    };

    let mut custom_claims = JwtIdClaims {
        azp: client.id.clone(),
        typ: JwtType::Id,
        amr: vec![amr],
        preferred_username: user.email.clone(),
        email: None,
        email_verified: None,
        given_name: None,
        family_name: None,
        roles: user.get_roles(),
        groups: None,
        custom: None,
    };

    if scope.contains("email") {
        custom_claims.email = Some(user.email.clone());
        custom_claims.email_verified = Some(user.email_verified);
    }

    if scope.contains("profile") {
        custom_claims.given_name = Some(user.given_name.clone());
        custom_claims.family_name = Some(user.family_name.clone());
    }

    if scope.contains("groups") {
        custom_claims.groups = Some(user.get_groups());
    }

    if let Some((cust, user_attrs)) = scope_customs {
        let user_attrs = user_attrs.as_ref().unwrap();
        let mut attr = HashMap::with_capacity(cust.len());
        for c in cust {
            if let Some(csv) = &c.attr_include_id {
                let scopes = csv.split(',');
                for cust_name in scopes {
                    if let Some(value) = user_attrs.get(cust_name) {
                        let json = serde_json::from_slice(value.as_slice())
                            .expect("Converting cust user id attr to json");
                        attr.insert(cust_name.to_string(), json);
                    };
                }
            }
        }
        if !attr.is_empty() {
            custom_claims.custom = Some(attr);
        }
    }

    let mut claims = Claims::with_custom_claims(
        custom_claims,
        coarsetime::Duration::from_secs(lifetime as u64),
    )
    .with_subject(user.id.clone())
    .with_issuer(data.issuer.clone())
    .with_audience(client.id.to_string());

    if let Some(nonce) = nonce {
        claims = claims.with_nonce(nonce);
    }

    sign_id_token(data, claims, client).await
}

/// Builds the refresh token for a user after all validation has been successful
pub async fn build_refresh_token(
    user: &User,
    data: &web::Data<AppState>,
    client: &Client,
    access_token_lifetime: i64,
    scope: Option<String>,
    is_mfa: bool,
) -> Result<String, ErrorResponse> {
    let custom_claims = JwtRefreshClaims {
        azp: client.id.clone(),
        typ: JwtType::Refresh,
        uid: user.id.clone(),
    };

    let claims = Claims::with_custom_claims(custom_claims, coarsetime::Duration::from_hours(48))
        .with_issuer(data.issuer.clone())
        .with_audience(client.id.to_string());

    let token = sign_refresh_token(data, claims).await?;

    // only save the last 50 characters for validation
    let validation_string = String::from(&token).split_off(token.len() - 49);

    // TODO extract the nbf and exp from the claims -> adjust entity
    let nbf = OffsetDateTime::now_utc().add(::time::Duration::seconds(access_token_lifetime - 60));
    let exp = &nbf.add(::time::Duration::seconds(48 * 3600));
    RefreshToken::create(
        data,
        validation_string,
        user.id.clone(),
        nbf,
        *exp,
        scope,
        is_mfa,
    )
    .await?;

    Ok(token)
}

#[inline(always)]
fn get_api_key_token_from_header(headers: &HeaderMap) -> Option<&str> {
    let api_key = headers.get("Authorization")?.to_str().ok()?;
    let (k, v) = api_key.split_once(' ')?;
    if k.ne(TOKEN_API_KEY) || k.is_empty() {
        None
    } else {
        Some(v)
    }
}

#[inline(always)]
fn get_bearer_token_from_header(headers: &HeaderMap) -> Result<String, ErrorResponse> {
    let bearer = headers.get("Authorization").ok_or_else(|| {
        ErrorResponse::new(
            ErrorResponseType::Unauthorized,
            String::from("Bearer Token missing"),
        )
    });
    if bearer.is_err() {
        return Err(ErrorResponse::new(
            ErrorResponseType::BadRequest,
            String::from("Authorization header missing"),
        ));
    }

    let head_val = bearer?
        .to_str()
        .map_err(|_| {
            ErrorResponse::new(
                ErrorResponseType::Unauthorized,
                String::from("Malformed Authorization Header. Could not extract token."),
            )
        })?
        .to_string();

    let (p, bearer) = head_val.split_once(' ').ok_or(("ERR", "")).map_err(|_| {
        ErrorResponse::new(
            ErrorResponseType::Unauthorized,
            String::from("Malformed Authorization Header. Could not extract token."),
        )
    })?;
    if p.ne(TOKEN_BEARER) || bearer.is_empty() {
        return Err(ErrorResponse::new(
            ErrorResponseType::Unauthorized,
            String::from("No bearer token given"),
        ));
    }
    Ok(bearer.to_string())
}

/// Returns the 'userInfo' for the [/oidc/userinfo endpoint](crate::handlers::get_userinfo)<br>
/// **Important: This function does NOT validate the token again!**
pub async fn get_userinfo(
    data: &web::Data<AppState>,
    req: HttpRequest,
) -> Result<Userinfo, ErrorResponse> {
    // get bearer token
    let bearer = get_bearer_token_from_header(req.headers())?;

    // token should already be validated in the permission extractor
    let claims = validate_token::<JwtCommonClaims>(data, &bearer).await?;

    let email = claims.subject.ok_or_else(|| {
        ErrorResponse::new(
            ErrorResponseType::Internal,
            String::from("Token without 'sub' - could not extract the Principal"),
        )
    })?;
    let user = User::find_by_email(data, email).await?;

    let roles = user.get_roles();
    let groups = user.get_groups();
    let userinfo = Userinfo {
        id: user.id,
        sub: user.email.clone(),
        email: user.email.clone(),
        email_verified: user.email_verified,
        name: format!("{} {}", &user.given_name, &user.family_name),
        roles,
        groups,
        preferred_username: user.email,
        given_name: user.given_name,
        family_name: user.family_name,
    };

    Ok(userinfo)
}

/// Returns [TokenInfo](crate::models::response::TokenInfo) for the
/// [/oidc/tokenInfo endpoint](crate::handlers::post_token_info)
pub async fn get_token_info(
    data: &web::Data<AppState>,
    token: &str,
) -> Result<TokenInfo, ErrorResponse> {
    let claims_res = validate_token::<JwtCommonClaims>(data, token).await;
    if claims_res.is_err() {
        return Ok(TokenInfo {
            active: false,
            scope: None,
            client_id: None,
            username: None,
            exp: None,
        });
    }

    let claims = claims_res.unwrap();
    // scope does not exist for ID tokens, for all others unwrap is safe
    let scope = claims.custom.scope;
    let client_id = claims.custom.azp;
    let username = claims.subject;
    let exp = claims.expires_at.unwrap().as_secs();

    Ok(TokenInfo {
        active: true,
        scope,
        client_id: Some(client_id),
        username,
        exp: Some(exp),
    })
}

/// Main entrance function for returning a whole new [TokenSet](crate::models::response::TokenSet)
pub async fn get_token_set(
    req_data: TokenRequest,
    data: &web::Data<AppState>,
    req: HttpRequest,
) -> Result<(TokenSet, Option<(HeaderName, HeaderValue)>), ErrorResponse> {
    match req_data.grant_type.as_str() {
        "authorization_code" => grant_type_code(data, req, req_data).await,
        "client_credentials" => grant_type_credentials(data, req, req_data).await,
        "password" => grant_type_password(data, req, req_data).await,
        "refresh_token" => grant_type_refresh(data, req, req_data).await,
        _ => Err(ErrorResponse::new(
            ErrorResponseType::BadRequest,
            String::from("Invalid 'grant_type'"),
        )),
    }
}

/// Return a [TokenSet](crate::models::response::TokenSet) for the `authorization_code` flow
#[tracing::instrument(skip_all, fields(client_id = req_data.client_id, username = req_data.username))]
async fn grant_type_code(
    data: &web::Data<AppState>,
    req: HttpRequest,
    req_data: TokenRequest,
) -> Result<(TokenSet, Option<(HeaderName, HeaderValue)>), ErrorResponse> {
    if req_data.code.is_none() {
        warn!("'code' is missing");
        return Err(ErrorResponse::new(
            ErrorResponseType::BadRequest,
            String::from("'code' is missing"),
        ));
    }

    // TODO another redirect_uri check? Add to AuthCode? Any security benefit?
    // let redirect_uri = if let Some(uri) = req_data.redirect_uri {
    //     if uri != code.
    // }

    // check the client for external origin and auth flow
    let (client_id, client_secret) = req_data.try_get_client_id_secret(&req)?;
    let client = Client::find(data, client_id.clone()).await.map_err(|_| {
        ErrorResponse::new(
            ErrorResponseType::NotFound,
            format!("Client '{}' not found", client_id),
        )
    })?;
    let header_origin = client.validate_origin(&req, &data.listen_scheme, &data.public_url)?;
    if client.confidential {
        let secret = client_secret.ok_or_else(|| {
            warn!("'client_secret' is missing");
            ErrorResponse::new(
                ErrorResponseType::BadRequest,
                String::from("'client_secret' is missing"),
            )
        })?;
        client.validate_secret(data, &secret, &req)?;
    }
    client.validate_flow("authorization_code")?;

    // get the auth code from the cache
    let idx = req_data.code.as_ref().unwrap().to_owned();
    let code = AuthCode::find(data, idx).await?.ok_or_else(|| {
        warn!(
            "'auth_code' could not be found inside the cache - Host: {}",
            get_client_ip(&req),
        );
        ErrorResponse::new(
            ErrorResponseType::Unauthorized,
            "'auth_code' could not be found inside the cache".to_string(),
        )
    })?;
    // validate the auth code
    if code.client_id != client_id {
        let err = format!("Wrong 'code' for client_id '{}'", client_id);
        warn!(err);
        return Err(ErrorResponse::new(ErrorResponseType::Unauthorized, err));
    }
    if code.exp < OffsetDateTime::now_utc().unix_timestamp() {
        warn!("The Authorization Code has expired");
        return Err(ErrorResponse::new(
            ErrorResponseType::SessionExpired,
            String::from("The Authorization Code has expired"),
        ));
    }
    if code.challenge.is_some() {
        if req_data.code_verifier.is_none() {
            warn!("'code_verifier' is missing");
            return Err(ErrorResponse::new(
                ErrorResponseType::BadRequest,
                String::from("'code_verifier' is missing"),
            ));
        }

        if code.challenge_method.as_ref().unwrap().eq("plain") {
            if !code.challenge.eq(&req_data.code_verifier) {
                warn!("'code_verifier' does not match the challenge");
                return Err(ErrorResponse::new(
                    ErrorResponseType::Unauthorized,
                    String::from("'code_verifier' does not match the challenge"),
                ));
            }
        } else {
            let hash = digest::digest(&digest::SHA256, req_data.code_verifier.unwrap().as_bytes());
            let hash_base64 = base64_url_encode(hash.as_ref());

            if !code.challenge.as_ref().unwrap().eq(&hash_base64) {
                warn!("'code_verifier' does not match the challenge");
                return Err(ErrorResponse::new(
                    ErrorResponseType::Unauthorized,
                    String::from("'code_verifier' does not match the challenge"),
                ));
            }
        }
    }

    let user = User::find(data, code.user_id.clone()).await?;

    let token_set = TokenSet::from_user(
        &user,
        data,
        &client,
        code.nonce.clone(),
        Some(code.scopes.join(" ")),
        true,
    )
    .await?;

    if code.session_id.is_some() {
        let sid = code.session_id.as_ref().unwrap().clone();
        let mut session = Session::find(data, sid).await?;

        session.last_seen = OffsetDateTime::now_utc().unix_timestamp();
        session.state = SessionState::Auth;
        if let Err(err) = session.validate_user_expiry(&user) {
            code.delete(data).await?;
            return Err(err);
        }
        session.validate_user_expiry(&user)?;
        session.user_id = Some(user.id);
        session.roles = Some(user.roles);
        session.groups = user.groups;
        session.save(data).await?;
    }
    code.delete(data).await?;

    Ok((token_set, header_origin))
}

/// Return a [TokenSet](crate::models::response::TokenSet) for the `client_credentials` flow
#[tracing::instrument(skip_all, fields(client_id = req_data.client_id, username = req_data.username))]
async fn grant_type_credentials(
    data: &web::Data<AppState>,
    req: HttpRequest,
    req_data: TokenRequest,
) -> Result<(TokenSet, Option<(HeaderName, HeaderValue)>), ErrorResponse> {
    if req_data.client_secret.is_none() {
        return Err(ErrorResponse::new(
            ErrorResponseType::BadRequest,
            String::from("'client_secret' is missing"),
        ));
    }

    let (client_id, client_secret) = req_data.try_get_client_id_secret(&req)?;
    let client = Client::find(data, client_id).await?;
    if !client.confidential {
        return Err(ErrorResponse::new(
            ErrorResponseType::BadRequest,
            String::from("'client_credentials' flow is allowed for confidential clients only"),
        ));
    }
    if !client.enabled {
        return Err(ErrorResponse::new(
            ErrorResponseType::BadRequest,
            String::from("client is disabled"),
        ));
    }
    let secret = client_secret.ok_or_else(|| {
        ErrorResponse::new(
            ErrorResponseType::BadRequest,
            String::from("'client_secret' is missing"),
        )
    })?;
    client.validate_secret(data, &secret, &req)?;
    client.validate_flow("client_credentials")?;

    let ts = TokenSet::for_client_credentials(data, &client).await?;
    Ok((ts, None))
}

/// Return a [TokenSet](crate::models::response::TokenSet) for the `password` flow
#[tracing::instrument(skip_all, fields(client_id = req_data.client_id, username = req_data.username))]
async fn grant_type_password(
    data: &web::Data<AppState>,
    req: HttpRequest,
    req_data: TokenRequest,
) -> Result<(TokenSet, Option<(HeaderName, HeaderValue)>), ErrorResponse> {
    if req_data.username.is_none() {
        return Err(ErrorResponse::new(
            ErrorResponseType::BadRequest,
            String::from("Missing 'username'"),
        ));
    }
    if req_data.password.is_none() {
        return Err(ErrorResponse::new(
            ErrorResponseType::BadRequest,
            String::from("Missing 'password"),
        ));
    }
    let (client_id, client_secret) = req_data.try_get_client_id_secret(&req)?;
    let email = req_data.username.as_ref().unwrap();
    let password = req_data.password.unwrap();

    let client = Client::find(data, client_id).await?;
    let header_origin = client.validate_origin(&req, &data.listen_scheme, &data.public_url)?;
    if client.confidential {
        let secret = client_secret.ok_or_else(|| {
            ErrorResponse::new(
                ErrorResponseType::BadRequest,
                String::from("Missing 'client_secret'"),
            )
        })?;
        client.validate_secret(data, &secret, &req)?;
    }
    client.validate_flow("password")?;

    // This Error must be the same if user does not exist AND passwords do not match to prevent
    // username enumeration
    let mut user = User::find_by_email(data, String::from(email))
        .await
        .map_err(|_| {
            warn!(
                "False login from Host: '{}' with invalid username: '{}'",
                get_client_ip(&req),
                email
            );
            ErrorResponse::new(
                ErrorResponseType::Unauthorized,
                String::from("Invalid user credentials"),
            )
        })?;
    user.check_enabled()?;
    user.check_expired()?;

    match user.validate_password(data, password.clone()).await {
        Ok(_) => {
            user.last_login = Some(OffsetDateTime::now_utc().unix_timestamp());
            user.last_failed_login = None;
            user.failed_login_attempts = None;

            // check if the password hash should be upgraded
            let hash_uptodate = user.is_argon2_uptodate(&data.argon2_params)?;
            if !hash_uptodate {
                info!("Updating Argon2ID params for user '{}'", &user.email);
                let new_hash = HashPassword::hash_password(password).await?;
                // let new_hash = User::new_password_hash(&password, params).await?;
                user.password = Some(new_hash);
            }

            user.save(data, None, None).await?;

            let ts = TokenSet::from_user(&user, data, &client, None, None, false).await?;
            Ok((ts, header_origin))
        }
        Err(err) => {
            warn!(
                "False Login attempt from Host: '{}' for user: '{}'",
                get_client_ip(&req),
                user.email
            );

            user.last_failed_login = Some(OffsetDateTime::now_utc().unix_timestamp());
            user.failed_login_attempts = Some(&user.failed_login_attempts.unwrap_or(0) + 1);

            user.save(data, None, None).await?;

            // TODO add expo increasing sleeps after failed login attempts here?
            Err(err)
        }
    }
}

/// Return a [TokenSet](crate::models::response::TokenSet) for the `refresh_token` flow
#[tracing::instrument(skip_all, fields(client_id = req_data.client_id, username = req_data.username))]
async fn grant_type_refresh(
    data: &web::Data<AppState>,
    req: HttpRequest,
    req_data: TokenRequest,
) -> Result<(TokenSet, Option<(HeaderName, HeaderValue)>), ErrorResponse> {
    if req_data.refresh_token.is_none() {
        return Err(ErrorResponse::new(
            ErrorResponseType::BadRequest,
            String::from("'refresh_token' is missing"),
        ));
    }
    let (client_id, client_secret) = req_data.try_get_client_id_secret(&req)?;
    let client = Client::find(data, client_id).await?;
    let header_origin = client.validate_origin(&req, &data.listen_scheme, &data.public_url)?;

    if client.confidential {
        let secret = client_secret.ok_or_else(|| {
            ErrorResponse::new(
                ErrorResponseType::BadRequest,
                String::from("'client_secret' is missing"),
            )
        })?;
        client.validate_secret(data, &secret, &req)?;
    }

    client.validate_flow("refresh_token")?;

    let refresh_token = req_data.refresh_token.unwrap();

    // validate common refresh token claims first and get the payload
    let ts = validate_refresh_token(Some(client), &refresh_token, data).await?;
    Ok((ts, header_origin))
}

/**
Handles the login delay.

With every successful login, a new average login time is calculated for how
long it took for a successful login. If a login failed though, the answer will be delayed by the
current average for a successful login, to prevent things like username enumeration.
 */
pub async fn handle_login_delay(
    start: Duration,
    cache_config: &redhac::CacheConfig,
    res: Result<(HttpResponse, bool), ErrorResponse>,
) -> Result<HttpResponse, ErrorResponse> {
    let success_time = cache_get!(
        i64,
        CACHE_NAME_LOGIN_DELAY.to_string(),
        IDX_LOGIN_TIME.to_string(),
        cache_config,
        false
    )
    .await?
    .unwrap_or(2000);

    let end = SystemTime::now().duration_since(UNIX_EPOCH).unwrap();
    let delta = end - start;

    match res {
        Ok((resp, has_password_been_hashed)) => {
            // TODO add possibly blacklisted IP cleanup here

            // only calculate the new median login time base on the full duration incl password hash
            if has_password_been_hashed {
                let new_time = (success_time + delta.as_millis() as i64) / 2;

                cache_put(
                    CACHE_NAME_LOGIN_DELAY.to_string(),
                    IDX_LOGIN_TIME.to_string(),
                    cache_config,
                    &new_time,
                )
                .await?;

                debug!("New login_success_time: {}", new_time);
            }

            Ok(resp)
        }
        Err(err) => {
            // TODO check possibly blacklisted IP cleanup here

            // casting to u64 is safe here since these values are very small anyway
            let time_taken = end.sub(start).as_millis() as u64;
            let mut sleep_time = 0;
            let su64 = success_time as u64;
            if time_taken < su64 {
                sleep_time = su64 - time_taken;
            }

            debug!("Failed login - sleeping for {}ms now", sleep_time);
            tokio::time::sleep(Duration::from_millis(sleep_time)).await;

            Err(err)
        }
    }
}

/// Returns the Logout HTML Page for [GET /oidc/logout](crate::handlers::get_logout)
pub async fn logout(
    logout_request: LogoutRequest,
    session: Session,
    data: &web::Data<AppState>,
    lang: &Language,
) -> Result<(String, String), ErrorResponse> {
    let colors = ColorEntity::find_rauthy(data).await?;

    if logout_request.id_token_hint.is_none() {
        return Ok(LogoutHtml::build(&session.csrf_token, false, &colors, lang));
    }

    // check if the provided token hint is a valid
    let token_raw = logout_request.id_token_hint.unwrap();
    let claims = validate_token::<JwtIdClaims>(data, &token_raw).await?;

    // check if it is an ID token
    if JwtType::Id != claims.custom.typ {
        return Err(ErrorResponse::new(
            ErrorResponseType::BadRequest,
            String::from("The provided token is not an ID token"),
        ));
    }

    // from here on, the token_hint contains a valid ID token -> skip the logout confirmation
    if logout_request.post_logout_redirect_uri.is_some() {
        // unwrap is safe since the token is valid already
        let client_id = claims.custom.azp;
        let client = Client::find(data, client_id).await?;
        if client.post_logout_redirect_uris.is_none() {
            return Err(ErrorResponse::new(
                ErrorResponseType::BadRequest,
                String::from("Given 'post_logout_redirect_uri' is not allowed"),
            ));
        }

        let target = logout_request.post_logout_redirect_uri.unwrap();
        let uri_vec = client.get_post_logout_uris();
        let valid_redirect = uri_vec.as_ref().unwrap().iter().filter(|uri| {
            if uri.ends_with('*') && target.starts_with(uri.split_once('*').unwrap().0) {
                return true;
            }
            if target.eq(*uri) {
                return true;
            }
            false
        });
        if valid_redirect.count() == 0 {
            return Err(ErrorResponse::new(
                ErrorResponseType::BadRequest,
                String::from("Given 'post_logout_redirect_uri' is not allowed"),
            ));
        }
        // redirect uri is valid at this point
    }

    Ok(LogoutHtml::build(&session.csrf_token, true, &colors, lang))
}

/// The permission extractor for the `GrantsMiddleware`
pub async fn permission_extractor(req: &ServiceRequest) -> Result<Vec<String>, Error> {
    let mut res = vec!["all".to_string()];
    let mut principal = None;

    // check session
    {
        let ext = req.extensions();
        // this unwrap would panic, if no rauthy session middleware would be injected
        let session_opt = ext
            .get::<Option<Session>>()
            .expect("No Option<Session> from ReqData - Rauthy Session Middleware missing?");
        if session_opt.is_some() {
            let session = session_opt.as_ref().unwrap();

            let perm = match session.state {
                SessionState::Init => "session-init",
                SessionState::Auth => "session-auth",
                _ => "session-anon",
            };
            res.push(String::from(perm));

            if session.state == SessionState::Auth {
                let roles = session.roles_as_vec()?;
                roles.iter().for_each(|r| res.push(r.clone()));

                principal = Some(Principal {
                    user_id: session
                        .user_id
                        .as_ref()
                        .expect("No user_id for authenticated session")
                        .clone(),
                    email: None,
                    has_mfa_active: session.is_mfa,
                    has_session: true,
                    has_token: false,
                    roles,
                });
            }
        }
    }

    // the Authorization header may contain either an 'API-Key' or a 'Bearer' token
    // only one of them may exist
    let mut api_key: Option<ApiKey> = None;
    if let Some(api_key_token) = get_api_key_token_from_header(req.headers()) {
        let data = req
            .app_data::<web::Data<AppState>>()
            .expect("Error getting AppData inside permission extractor");

        if let Ok(key) = ApiKeyEntity::api_key_from_token_validated(data, api_key_token).await {
            res.push(String::from("api-key"));
            api_key = Some(key);
        }
    }
    req.extensions_mut().insert(api_key);

    let bearer = get_bearer_token_from_header(req.headers());
    if bearer.is_err() {
        req.extensions_mut().insert(principal);
        return Ok(res);
    }

    let data = req
        .app_data::<web::Data<AppState>>()
        .expect("Could not get AppState");

    let claims = validate_token::<JwtAccessClaims>(data, bearer.unwrap().as_str()).await?;

    // roles
    claims
        .custom
        .roles
        .as_ref()
        .ok_or_else(|| {
            ErrorResponse::new(
                ErrorResponseType::Internal,
                "Malformed JWT Token - roles missing".to_string(),
            )
        })?
        .iter()
        .for_each(|role| res.push(format!("ROLE_{}", role)));

    // user_id
    if claims.custom.uid.is_some() {
        let uid = claims.custom.uid.unwrap();
        let sub = claims.subject.ok_or_else(|| {
            ErrorResponse::new(
                ErrorResponseType::Unauthorized,
                "Malformed JWT Token".to_string(),
            )
        })?;
        if principal.is_some() {
            let mut p = principal.unwrap();
            if p.user_id != uid {
                error!("Request with different user id's for JWT token and session - not going on with adding token roles / groups");
                req.extensions_mut().insert(Some(p));
                return Ok(res);
            }

            p.email = Some(sub);
            p.has_token = true;
            // TODO can this be skipped?
            principal = Some(p);
        } else {
            // unwrap is safe here, Error would have returned already otherwise
            let roles = claims
                .custom
                .roles
                .unwrap()
                .into_iter()
                .map(|r| format!("ROLE_{}", r))
                .collect::<Vec<String>>();

            principal = Some(Principal {
                user_id: uid,
                email: Some(sub),
                // If we just have a JWT token, we cannot know if it was retrieved with MFA.
                // The 'amr' claim is in the ID token.
                has_mfa_active: false,
                has_session: false,
                has_token: true,
                roles,
            });
        }
    }

    req.extensions_mut().insert(principal);
    res.push(String::from("token-auth"));
    Ok(res)
}

// TODO move into entity
/// Rotates and generates a whole new Set of JWKs for signing JWT Tokens
pub async fn rotate_jwks(data: &web::Data<AppState>) -> Result<(), ErrorResponse> {
    info!("Starting JWKS rotation");

    let key = data.enc_keys.get(&data.enc_key_active).unwrap();

    // RSA256
    let jwk_plain = web::block(|| {
        RS256KeyPair::generate(2048)
            .unwrap()
            .with_key_id(&get_rand(24))
    })
    .await?;
    let jwk = encrypt(jwk_plain.to_der().unwrap().as_slice(), key)?;
    let entity = Jwk {
        kid: jwk_plain.key_id().as_ref().unwrap().clone(),
        created_at: OffsetDateTime::now_utc().unix_timestamp(),
        signature: JwkKeyPairType::RS256,
        enc_key_id: data.enc_key_active.to_string(),
        jwk,
    };
    entity.save(&data.db).await?;

    // RS384
    let jwk_plain = web::block(|| {
        RS384KeyPair::generate(3072)
            .unwrap()
            .with_key_id(&get_rand(24))
    })
    .await?;
    let jwk = encrypt(jwk_plain.to_der().unwrap().as_slice(), key)?;
    let entity = Jwk {
        kid: jwk_plain.key_id().as_ref().unwrap().clone(),
        created_at: OffsetDateTime::now_utc().unix_timestamp(),
        signature: JwkKeyPairType::RS384,
        enc_key_id: data.enc_key_active.to_string(),
        jwk,
    };
    entity.save(&data.db).await?;

    // RSA512
    let jwk_plain = web::block(|| {
        RS512KeyPair::generate(4096)
            .unwrap()
            .with_key_id(&get_rand(24))
    })
    .await?;
    let jwk = encrypt(jwk_plain.to_der().unwrap().as_slice(), key)?;
    let entity = Jwk {
        kid: jwk_plain.key_id().as_ref().unwrap().clone(),
        created_at: OffsetDateTime::now_utc().unix_timestamp(),
        signature: JwkKeyPairType::RS512,
        enc_key_id: data.enc_key_active.to_string(),
        jwk,
    };
    entity.save(&data.db).await?;

    // Ed25519
    let jwk_plain = web::block(|| Ed25519KeyPair::generate().with_key_id(&get_rand(24))).await?;
    let jwk = encrypt(jwk_plain.to_der().as_slice(), key)?;
    let entity = Jwk {
        kid: jwk_plain.key_id().as_ref().unwrap().clone(),
        created_at: OffsetDateTime::now_utc().unix_timestamp(),
        signature: JwkKeyPairType::EdDSA,
        enc_key_id: data.enc_key_active.to_string(),
        jwk,
    };
    entity.save(&data.db).await?;

    // clear all latest_jwk from cache
    cache_del(
        CACHE_NAME_12HR.to_string(),
        format!("{}{}", IDX_JWK_LATEST, JwkKeyPairType::RS256.to_string()),
        &data.caches.ha_cache_config,
    )
    .await?;
    cache_del(
        CACHE_NAME_12HR.to_string(),
        format!("{}{}", IDX_JWK_LATEST, JwkKeyPairType::RS384.to_string()),
        &data.caches.ha_cache_config,
    )
    .await?;
    cache_del(
        CACHE_NAME_12HR.to_string(),
        format!("{}{}", IDX_JWK_LATEST, JwkKeyPairType::RS512.to_string()),
        &data.caches.ha_cache_config,
    )
    .await?;
    cache_del(
        CACHE_NAME_12HR.to_string(),
        format!("{}{}", IDX_JWK_LATEST, JwkKeyPairType::EdDSA.to_string()),
        &data.caches.ha_cache_config,
    )
    .await?;

    // clear the all_certs / JWKS cache
    cache_del(
        CACHE_NAME_12HR.to_string(),
        IDX_JWKS.to_string(),
        &data.caches.ha_cache_config,
    )
    .await?;

    info!("Finished JWKS rotation");

    Ok(())
}

/// Signs an access token
async fn sign_access_token(
    data: &web::Data<AppState>,
    claims: claims::JWTClaims<JwtAccessClaims>,
    client: &Client,
) -> Result<String, ErrorResponse> {
    let key_pair_type = JwkKeyPairType::from_str(&client.access_token_alg)?;
    let kp = JwkKeyPair::find_latest(data, &client.access_token_alg, key_pair_type).await?;
    sign_jwt!(kp, claims)
}

/// Signs an id token
async fn sign_id_token(
    data: &web::Data<AppState>,
    claims: claims::JWTClaims<JwtIdClaims>,
    client: &Client,
) -> Result<String, ErrorResponse> {
    let key_pair_type = JwkKeyPairType::from_str(&client.id_token_alg)?;
    let kp = JwkKeyPair::find_latest(data, &client.id_token_alg, key_pair_type).await?;
    sign_jwt!(kp, claims)
}

/// Signs a refresh token
async fn sign_refresh_token(
    data: &web::Data<AppState>,
    claims: claims::JWTClaims<JwtRefreshClaims>,
) -> Result<String, ErrorResponse> {
    let alg = String::from("EdDSA");
    let key_pair_type = JwkKeyPairType::from_str(&alg)?;
    let kp = JwkKeyPair::find_latest(data, &alg, key_pair_type).await?;
    sign_jwt!(kp, claims)
}

/// Validates request parameters for the authorization and refresh endpoints
pub async fn validate_auth_req_param(
    data: &web::Data<AppState>,
    req: &HttpRequest,
    client_id: &str,
    redirect_uri: &str,
    code_challenge: &Option<String>,
    code_challenge_method: &Option<String>,
) -> Result<(Client, Option<(HeaderName, HeaderValue)>), ErrorResponse> {
    // client exists
    let client = Client::find(data, String::from(client_id))
        .await
        .map_err(|mut e| {
            e.message = format!("Client '{}' not found", client_id);
            e
        })?;

    // allowed origin
    let header = client.validate_origin(req, &data.listen_scheme, &data.public_url)?;

    // allowed redirect uris
    let uris = client
        .get_redirect_uris()
        .iter()
        .filter(|uri| {
            if (uri.ends_with('*') && redirect_uri.starts_with(uri.split_once('*').unwrap().0))
                || uri.eq(&redirect_uri)
            {
                return true;
            }
            false
        })
        .count();
    if uris == 0 {
        return Err(ErrorResponse::new(
            ErrorResponseType::BadRequest,
            String::from("Invalid redirect uri"),
        ));
    }

    // code challenge + method
    if client.challenge.is_some() {
        if code_challenge.is_none() {
            return Err(ErrorResponse::new(
                ErrorResponseType::BadRequest,
                String::from("'code_challenge' is missing"),
            ));
        } else {
            // 'plain' is the default method to be assumed by the OAuth specification when it is
            // not further specified.
            let method = if let Some(m) = code_challenge_method {
                m.to_owned()
            } else {
                String::from("plain")
            };
            client.validate_challenge_method(&method)?;
        }
    }

    Ok((client, header))
}

// TODO remove handler /refresh and move into grant_type_refresh? -> obsolete since grant_type_refresh?
/// Validates common claims for refresh tokens used in different places
pub async fn validate_refresh_token(
    // when this is some, it will be checked against the 'azp' claim, otherwise skipped and a client
    // will be fetched inside this function
    client_opt: Option<Client>,
    refresh_token: &str,
    data: &web::Data<AppState>,
) -> Result<TokenSet, ErrorResponse> {
    let options = VerificationOptions {
        // allowed_audiences: Some(HashSet::from_strings(&[&])), // TODO
        allowed_issuers: Some(HashSet::from_strings(&[&data.issuer])),
        ..Default::default()
    };

    // extract metadata
    let kid = JwkKeyPair::kid_from_token(refresh_token)?;

    // retrieve jwk for kid
    let kp = JwkKeyPair::find(data, kid).await?;
    let claims: claims::JWTClaims<JwtRefreshClaims> =
        validate_jwt!(JwtRefreshClaims, kp, refresh_token, options)?;

    // validate typ
    if claims.custom.typ != JwtType::Refresh {
        return Err(ErrorResponse::new(
            ErrorResponseType::BadRequest,
            String::from("Provided Token is not a valid refresh token"),
        ));
    }

    // get uid
    let uid = claims.custom.uid;

    // get azp / client
    let client = if let Some(c) = client_opt {
        c
    } else {
        Client::find(data, claims.custom.azp.clone()).await?
    };
    if client.id != claims.custom.azp {
        return Err(ErrorResponse::new(
            ErrorResponseType::BadRequest,
            String::from("'client_id' does not match"),
        ));
    }

    // validate that it exists in the db
    let (_, validation_str) = refresh_token.split_at(refresh_token.len() - 49);
    // let validation_str = String::from(refresh_token).split_off(&refresh_token.len() - 49);

    let mut rt = RefreshToken::find(data, validation_str).await?;

    // check expires_at from the db entry
    if rt.exp < OffsetDateTime::now_utc().unix_timestamp() {
        // if an already used refresh token was provided again, invalidate all existing ones for the
        // user as well to prevent possible security issues
        RefreshToken::invalidate_all_for_user(data, &rt.user_id).await?;
        return Err(ErrorResponse::new(
            ErrorResponseType::BadRequest,
            String::from(
                "Refresh Token has expired already. All other refresh tokens\
                for this user have been invalidated now because of misuse.",
            ),
        ));
    }

    let mut user = User::find(data, uid).await?;
    user.check_enabled()?;
    user.check_expired()?;

    // at this point, everything has been validated -> we can issue a new TokenSet safely
    debug!("Refresh Token - all good!");

    // set last login
    user.last_login = Some(OffsetDateTime::now_utc().unix_timestamp());
    user.save(data, None, None).await?;

    // invalidate current refresh token
    let now = OffsetDateTime::now_utc().unix_timestamp();
    let exp_at_secs = now + data.refresh_grace_time as i64;
    // do not set expires_at, if we are below our refresh token grace time anyway already
    if rt.exp > exp_at_secs + 1 {
        rt.exp = exp_at_secs;
        rt.save(data).await?;
    }

    // TODO do we somehow need to be able to set 'nonce' here too?
    if let Some(s) = rt.scope {
        TokenSet::from_user(&user, data, &client, None, Some(s), rt.is_mfa).await
    } else {
        TokenSet::from_user(&user, data, &client, None, None, rt.is_mfa).await
    }
}

/// Validates a given JWT Access Token
pub async fn validate_token<T: serde::Serialize + for<'de> ::serde::Deserialize<'de>>(
    data: &web::Data<AppState>,
    token: &str,
) -> Result<claims::JWTClaims<T>, ErrorResponse> {
    let options = jwt_simple::prelude::VerificationOptions {
        // allowed_audiences: Some(HashSet::from_strings(&[&])), // TODO
        allowed_issuers: Some(HashSet::from_strings(&[&data.issuer])),
        ..Default::default()
    };

    // extract metadata
    let kid = JwkKeyPair::kid_from_token(token)?;

    // retrieve jwk for kid
    let kp = JwkKeyPair::find(data, kid).await?;
    validate_jwt!(T, kp, token, options)

    // TODO check roles if we add more users / roles
}

#[cfg(test)]
mod tests {}
