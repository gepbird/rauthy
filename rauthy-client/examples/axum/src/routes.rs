use crate::config::Config;
use crate::{templates, DEV_MODE};
use axum::body::Body;
use axum::extract::Query;
use axum::http::header::{CONTENT_TYPE, SET_COOKIE};
use axum::response::{IntoResponse, Response};
use rauthy_client::handler::OidcCallbackParams;
use rauthy_client::handler::{OidcCookieInsecure, OidcSetRedirectStatus};
use rauthy_client::principal::PrincipalOidc;
use std::sync::Arc;

type ConfigExt = axum::extract::State<Arc<Config>>;

/// OIDC Auth check and login
///
/// Endpoint with no redirect on purpose to use the result inside Javascript from the frontend.
/// HTTP 200 will have a location header and a manual redirect must be done
/// HTTP 202 means logged in Principal
pub async fn get_index() -> Response<Body> {
    Response::builder()
        .status(200)
        .header(CONTENT_TYPE, "text/html")
        .body(Body::from(templates::HTML_INDEX))
        .unwrap()
}

/// OIDC Auth check and login
///
/// Endpoint with no redirect on purpose to use the result inside Javascript from the frontend.
/// HTTP 200 will have a location header and a manual redirect must be done
/// HTTP 202 means logged in Principal
pub async fn get_auth_check(config: ConfigExt, principal: Option<PrincipalOidc>) -> Response<Body> {
    let enc_key = config.enc_key.as_slice();

    if DEV_MODE {
        rauthy_client::handler::validate_redirect_principal(
            principal,
            // this enc_key must be exactly 32 bytes long
            enc_key,
            // if we are in dev mode, we allow insecure cookies
            OidcCookieInsecure::Yes,
            // if you want to browser to automatically redirect to the login, set to yes
            // we set this to no to actually show a button for logging in beforehand
            OidcSetRedirectStatus::No,
        )
        .await
    } else {
        rauthy_client::handler::validate_redirect_principal(
            principal,
            enc_key,
            OidcCookieInsecure::No,
            OidcSetRedirectStatus::No,
        )
        .await
    }
}

/// OIDC Callback endpoint - must match the `redirect_uri` for the login flow
pub async fn get_callback(
    jar: axum_extra::extract::CookieJar,
    config: ConfigExt,
    params: Query<OidcCallbackParams>,
) -> Response<Body> {
    let enc_key = config.enc_key.as_slice();

    // The `DEV_MODE` again here to just have a nicer DX when developing
    let callback = if DEV_MODE {
        rauthy_client::handler::oidc_callback(&jar, params, enc_key, OidcCookieInsecure::Yes)
    } else {
        rauthy_client::handler::oidc_callback(&jar, params, enc_key, OidcCookieInsecure::No)
    };
    let (cookie_str, token_set, _id_claims) = match callback.await {
        Ok(res) => res,
        Err(err) => {
            return Response::builder()
                .status(400)
                .body(Body::from(format!("Invalid OIDC Callback: {}", err)))
                .unwrap()
        }
    };

    // At this point, the redirect was valid and everything was fine.
    // Depending on how you like to proceed, you could create an independant session for the user,
    // or maybe create just another factor of authentication like a CSRF token.
    // Otherwise, you could just go on and using the existing access token for further authentication.
    //
    // For the sake of this example, we will return the raw access token to the user via the HTML
    // so we can use it for future authentication from the frontend, but this is really up to you
    // and the security needs of your application.

    // This is a very naive approach to HTML templating and only for simplicity in this example.
    // Please don't do this in production and use a proper templating engine.
    let body = templates::HTML_CALLBACK
        .replace("{{ TOKEN }}", &token_set.access_token)
        .replace("{{ URI }}", "/");

    Response::builder()
        .status(200)
        // we should append the returned cookie jar here to
        // delete the state cookie from the login flow
        .header(SET_COOKIE, cookie_str)
        .header(CONTENT_TYPE, "text/html")
        .body(Body::from(body))
        .unwrap()
}

/// As soon as you request the `principal: PrincipalOidc` as a parameter, this route can only be
/// accessed with a valid Token. Otherwise, the Principal cannot be built and would return a 401
/// from the extractor function.
pub async fn get_protected(principal: PrincipalOidc) -> impl IntoResponse {
    // As soon as we get here, the principal is actually valid already.
    // The Principal provides some handy base functions for further easy validation, like:
    //
    // principal.is_admin()?;
    // principal.has_any_group(vec!["group123", "group456"])?;
    // principal.has_any_role(vec!["admin", "root"])?;

    format!("Hello from Protected Resource:<br/>{:?}", principal)
}