//! Authenticated, loopback-only browser access to the local tool allowlist.

use std::convert::Infallible;
use std::io::{self, BufRead, Read, Write};
use std::net::Ipv4Addr;
use std::sync::Arc;
use std::time::Duration;

use argon2::{Argon2, Params, PasswordHash, PasswordHasher, PasswordVerifier};
use base64::{Engine as _, engine::general_purpose::STANDARD};
use http_body_util::{BodyExt, Full, Limited};
use hyper::body::{Bytes, Incoming};
use hyper::header::{self, HeaderMap, HeaderValue};
use hyper::{Method, Request, Response, StatusCode};
use hyper::server::conn::http1;
use hyper::service::service_fn;
use hyper_util::rt::{TokioIo, TokioTimer};
use serde::Deserialize;
use serde_json::{Value, json};
use tokio::sync::Semaphore;
use zeroize::Zeroizing;

use crate::browser_settings::Settings;
use crate::local_api::{Client, Control};

const PASSWORD_MAX: usize = 256;
const BODY_LIMIT: usize = 64 * 1024;
const RESPONSE_LIMIT: usize = 512 * 1024;
const REQUEST_TIMEOUT: Duration = Duration::from_secs(45);
const USAGE: &str = "usage: asagiri web [--port PORT] | password [--password-stdin] | enable | disable | allow-changes on|off | status";
type Reply = Response<Full<Bytes>>;

pub fn run(args: &[String]) -> io::Result<()> {
    let settings = Settings::current()?;
    let args: Vec<_> = args.iter().map(String::as_str).collect();
    match args.as_slice() {
        ["password"] | ["password", "--password-stdin"] => {
            let password = if args.len() == 2 {
                let mut bytes = Zeroizing::new(Vec::new());
                io::stdin().lock().take(PASSWORD_MAX as u64 + 3).read_until(b'\n', &mut bytes)?;
                if bytes.last() == Some(&b'\n') { bytes.pop(); }
                if bytes.last() == Some(&b'\r') { bytes.pop(); }
                Zeroizing::new(String::from_utf8(bytes.to_vec()).map_err(|_| io::Error::other("password must be UTF-8"))?)
            } else {
                let password = Zeroizing::new(rpassword::prompt_password("Choose a browser password (at least 12 characters): ")?);
                let confirmation = Zeroizing::new(rpassword::prompt_password("Repeat password: ")?);
                if password.as_str() != confirmation.as_str() { return Err(io::Error::other("passwords do not match")); }
                password
            };
            validate_password(&password)?;
            let hash = Argon2::default().hash_password(password.as_bytes())
                .map_err(|_| io::Error::other("could not hash the browser password"))?.to_string();
            let config = settings.update(|c| {
                c.password_hash = Some(hash); c.enabled = true; c.allow_container_changes = false; Ok(())
            })?;
            println!("{}", config.status());
            return Ok(());
        }
        ["status"] => { println!("{}", settings.load()?.status()); return Ok(()); }
        ["enable"] | ["disable"] | ["allow-changes", "on" | "off"] => {
            let config = settings.update(|c| {
                match args[0] {
                    "enable" => c.enabled = true,
                    "disable" => c.enabled = false,
                    _ => {
                        if !c.permissions().enabled { return Err(io::Error::other("enable browser access first")); }
                        c.allow_container_changes = args[1] == "on";
                    }
                }
                Ok(())
            })?;
            println!("{}", config.status());
            return Ok(());
        }
        [] | ["--port", _] => {}
        _ => return Err(io::Error::other(USAGE)),
    }
    let port = args.get(1).map(|value| value.parse::<u16>().map_err(|_| io::Error::other("port must be 0..65535")))
        .transpose()?.unwrap_or(0);
    if !settings.load()?.permissions().enabled {
        return Err(io::Error::other("browser access is disabled; set a password with asagiri web password, then run asagiri web"));
    }
    tokio::runtime::Builder::new_multi_thread().worker_threads(2).max_blocking_threads(4)
        .enable_io().enable_time().build()?.block_on(serve(settings, port))
}

fn validate_password(password: &str) -> io::Result<()> {
    if password.chars().count() < 12 || password.len() > PASSWORD_MAX || password.chars().any(char::is_control) {
        Err(io::Error::other("password must have at least 12 characters, at most 256 UTF-8 bytes, and no control characters"))
    } else { Ok(()) }
}

#[derive(Clone, Debug)]
struct Context {
    settings: Settings,
    client: Client,
    origin: String,
    host: String,
    workers: Arc<Semaphore>,
}

async fn serve(settings: Settings, port: u16) -> io::Result<()> {
    let listener = tokio::net::TcpListener::bind((Ipv4Addr::LOCALHOST, port)).await?;
    let host = listener.local_addr()?.to_string();
    let context = Context { settings, client: Client::current()?, origin: format!("http://{host}"),
        host, workers: Arc::new(Semaphore::new(4)) };
    println!("Asagiri browser: {}/\nUsername: asagiri\nPress Ctrl-C to stop.", context.origin);
    io::stdout().flush()?;
    let connections = Arc::new(Semaphore::new(8));
    loop {
        let (stream, _) = listener.accept().await?;
        let Ok(permit) = connections.clone().try_acquire_owned() else { drop(stream); continue; };
        let context = context.clone();
        tokio::spawn(async move {
            let service = service_fn(move |request| {
                let context = context.clone();
                async move { Ok::<_, Infallible>(handle(request, context).await) }
            });
            let mut builder = http1::Builder::new();
            builder.keep_alive(false).max_headers(48).max_buf_size(16 * 1024)
                .timer(TokioTimer::new()).header_read_timeout(Duration::from_secs(5));
            let _ = tokio::time::timeout(Duration::from_secs(50),
                builder.serve_connection(TokioIo::new(stream), service)).await;
            drop(permit);
        });
    }
}

fn single<'a>(headers: &'a HeaderMap, name: &str) -> Option<&'a str> {
    let mut values = headers.get_all(name).iter();
    let value = values.next()?.to_str().ok()?;
    if values.next().is_some() { None } else { Some(value) }
}

fn boundary(headers: &HeaderMap, host: &str, origin: &str, mutation: bool) -> Result<(), StatusCode> {
    if single(headers, "host") != Some(host) { return Err(StatusCode::MISDIRECTED_REQUEST); }
    if headers.contains_key("origin") && single(headers, "origin") != Some(origin) {
        return Err(StatusCode::FORBIDDEN);
    }
    if mutation && single(headers, "origin") != Some(origin) { return Err(StatusCode::FORBIDDEN); }
    if headers.contains_key("sec-fetch-site") && !matches!(single(headers, "sec-fetch-site"), Some("same-origin" | "none")) {
        return Err(StatusCode::FORBIDDEN);
    }
    Ok(())
}

fn verify(authorization: &str, stored: &str) -> bool {
    if authorization.len() > 512 { return false; }
    let Some((scheme, encoded)) = authorization.split_once(' ') else { return false; };
    if !scheme.eq_ignore_ascii_case("Basic") { return false; }
    let Ok(decoded) = STANDARD.decode(encoded) else { return false; };
    let decoded = Zeroizing::new(decoded);
    let Some(separator) = decoded.iter().position(|b| *b == b':') else { return false; };
    if &decoded[..separator] != b"asagiri" || decoded.len() - separator - 1 > PASSWORD_MAX { return false; }
    let Ok(hash) = PasswordHash::new(stored) else { return false; };
    let Ok(params) = Params::try_from(&hash) else { return false; };
    let defaults = Params::default();
    // Hash parameters come from the PHC string. Bound them before the
    // verifier allocates memory, even for a malformed private settings file.
    if hash.algorithm.as_str() != "argon2id" || hash.version != Some(19)
        || params.m_cost() != defaults.m_cost() || params.t_cost() != defaults.t_cost()
        || params.p_cost() != defaults.p_cost()
    { return false; }
    Argon2::default().verify_password(&decoded[separator + 1..], &hash).is_ok()
}

async fn handle(request: Request<Incoming>, context: Context) -> Reply {
    if let Err(status) = boundary(request.headers(), &context.host, &context.origin, request.method() == Method::POST) {
        return failure(status, "This browser request is outside the local origin.");
    }
    if request.uri().scheme().is_some() || request.uri().authority().is_some() || request.uri().query().is_some() {
        return failure(StatusCode::BAD_REQUEST, "Use the local browser URL without a proxy or query.");
    }
    let Some(authorization) = single(request.headers(), "authorization").filter(|value| value.len() <= 512) else {
        return unauthorized();
    };
    let authorization = Zeroizing::new(authorization.to_owned());
    let settings = context.settings.clone();
    let Ok(Ok(permit)) = tokio::time::timeout(Duration::from_secs(2), context.workers.clone().acquire_owned()).await else {
        return failure(StatusCode::SERVICE_UNAVAILABLE, "Browser requests are busy. Try again.");
    };
    let authorized = tokio::task::spawn_blocking(move || {
        let _permit = permit;
        settings.load().ok().is_some_and(|c| c.permissions().enabled &&
            c.password_hash.as_deref().is_some_and(|hash| verify(&authorization, hash)))
    }).await.unwrap_or(false);
    if !authorized { return unauthorized(); }
    match (request.method(), request.uri().path()) {
        (&Method::GET, "/") => reply(StatusCode::OK, "text/html; charset=utf-8", include_str!("browser/index.html").as_bytes().to_vec()),
        (&Method::GET, "/app.css") => reply(StatusCode::OK, "text/css; charset=utf-8", include_str!("browser/app.css").as_bytes().to_vec()),
        (&Method::GET, "/app.js") => reply(StatusCode::OK, "text/javascript; charset=utf-8", include_str!("browser/app.js").as_bytes().to_vec()),
        (&Method::GET, "/mark.svg") => reply(StatusCode::OK, "image/svg+xml", include_str!("../../../brand/logo/asagiri-mark-light.svg").as_bytes().to_vec()),
        (&Method::GET, "/api/tools") => {
            let config = match context.settings.load() { Ok(c) => c, Err(_) => return unauthorized() };
            if !config.permissions().enabled { return unauthorized(); }
            let mut value = crate::tooling::definitions(config.permissions().allow_container_changes);
            value["permissions"] = config.status();
            json_reply(StatusCode::OK, value)
        }
        (&Method::POST, "/api/call") => {
            if single(request.headers(), "content-type") != Some("application/json") {
                return failure(StatusCode::UNSUPPORTED_MEDIA_TYPE, "Requests must use application/json.");
            }
            if request.headers().get(header::CONTENT_LENGTH).and_then(|v| v.to_str().ok())
                .and_then(|v| v.parse::<u64>().ok()).is_some_and(|n| n > BODY_LIMIT as u64) {
                return failure(StatusCode::PAYLOAD_TOO_LARGE, "Browser request exceeds 64 KiB.");
            }
            let bytes = match tokio::time::timeout(Duration::from_secs(5),
                Limited::new(request.into_body(), BODY_LIMIT).collect()).await {
                Ok(Ok(body)) => body.to_bytes(),
                Ok(Err(_)) => return failure(StatusCode::PAYLOAD_TOO_LARGE, "Invalid or oversized browser request."),
                Err(_) => return failure(StatusCode::REQUEST_TIMEOUT, "Browser request body timed out."),
            };
            #[derive(Deserialize)]
            #[serde(deny_unknown_fields)]
            struct Call { name: String, #[serde(default = "arguments")] arguments: Value }
            fn arguments() -> Value { json!({}) }
            let call: Call = match serde_json::from_slice(&bytes) {
                Ok(call) => call,
                Err(_) => return failure(StatusCode::BAD_REQUEST, "Invalid tool request."),
            };
            if !crate::tooling::known(&call.name) || !call.arguments.is_object() {
                return failure(StatusCode::BAD_REQUEST, "Unknown tool or invalid arguments.");
            }
            let Ok(Ok(permit)) = tokio::time::timeout(Duration::from_secs(2), context.workers.clone().acquire_owned()).await else {
                return failure(StatusCode::SERVICE_UNAVAILABLE, "Browser requests are busy. Try again.");
            };
            let control = Control::new(REQUEST_TIMEOUT);
            struct CancelOnDrop(Arc<Control>);
            impl Drop for CancelOnDrop { fn drop(&mut self) { self.0.cancel(); } }
            let _cancel = CancelOnDrop(control.clone());
            let result = tokio::task::spawn_blocking(move || {
                let _permit = permit;
                crate::tooling::call(&context.client, &|| context.settings.load().map(|c| c.permissions()),
                    &call.name, &call.arguments, control)
            }).await;
            match result {
                Ok(Ok(value)) => json_reply(StatusCode::OK, json!({"result": value})),
                Ok(Err(message)) => failure(StatusCode::BAD_REQUEST, &message),
                Err(_) => failure(StatusCode::INTERNAL_SERVER_ERROR, "Browser operation failed."),
            }
        }
        (_, "/api/call") | (_, "/api/tools") | (_, "/") | (_, "/app.css") | (_, "/app.js") | (_, "/mark.svg") =>
            failure(StatusCode::METHOD_NOT_ALLOWED, "This endpoint does not accept that method."),
        _ => failure(StatusCode::NOT_FOUND, "Browser endpoint not found."),
    }
}

fn unauthorized() -> Reply {
    let mut response = failure(StatusCode::UNAUTHORIZED, "Browser access requires your Asagiri browser password.");
    response.headers_mut().insert(header::WWW_AUTHENTICATE, HeaderValue::from_static("Basic realm=\"Asagiri local browser\", charset=\"UTF-8\""));
    response
}

fn failure(status: StatusCode, message: &str) -> Reply { json_reply(status, json!({"error": message})) }

fn json_reply(status: StatusCode, value: Value) -> Reply {
    match serde_json::to_vec(&value) {
        Ok(bytes) if bytes.len() <= RESPONSE_LIMIT => reply(status, "application/json", bytes),
        _ => reply(StatusCode::BAD_GATEWAY, "application/json", b"{\"error\":\"Browser response exceeds 512 KiB. Select fewer items.\"}".to_vec()),
    }
}

fn reply(status: StatusCode, content_type: &'static str, bytes: Vec<u8>) -> Reply {
    let mut response = Response::new(Full::new(Bytes::from(bytes)));
    *response.status_mut() = status;
    let headers = response.headers_mut();
    headers.insert(header::CONTENT_TYPE, HeaderValue::from_static(content_type));
    headers.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    headers.insert(header::CONTENT_SECURITY_POLICY, HeaderValue::from_static(
        "default-src 'none'; script-src 'self'; style-src 'self'; img-src 'self'; connect-src 'self'; frame-ancestors 'none'; base-uri 'none'; form-action 'none'"));
    headers.insert(header::X_CONTENT_TYPE_OPTIONS, HeaderValue::from_static("nosniff"));
    headers.insert(header::REFERRER_POLICY, HeaderValue::from_static("no-referrer"));
    response
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn standard_basic_auth_verifies_the_password_and_rejects_arbitrary_credentials() {
        let password = "a browser test password";
        let hash = Argon2::default().hash_password(password.as_bytes()).unwrap().to_string();
        let auth = |value: &str| format!("Basic {}", STANDARD.encode(value));
        assert!(verify(&auth(&format!("asagiri:{password}")), &hash));
        assert!(!verify(&auth("asagiri:not the password"), &hash));
        assert!(!verify(&auth(&format!("other:{password}")), &hash));
        assert!(!verify("Bearer arbitrary-token", &hash));
        assert!(!verify("Basic invalid", &hash));
        assert!(!verify(&auth(&format!("asagiri:{password}")), &hash.replace("m=19456", "m=4294967295")));
        assert!(validate_password("short").is_err());
        assert!(validate_password("a password with\nnewline").is_err());
    }

    #[test]
    fn host_origin_and_fetch_metadata_block_rebinding_and_cross_origin_actions() {
        let host = "127.0.0.1:12345";
        let origin = "http://127.0.0.1:12345";
        let mut headers = HeaderMap::new();
        headers.insert("host", HeaderValue::from_static(host));
        assert!(boundary(&headers, host, origin, false).is_ok());
        assert!(boundary(&headers, host, origin, true).is_err());
        headers.insert("origin", HeaderValue::from_static(origin));
        assert!(boundary(&headers, host, origin, true).is_ok());
        headers.insert("origin", HeaderValue::from_static("http://127.0.0.1:9999"));
        assert!(boundary(&headers, host, origin, false).is_err());
        headers.insert("origin", HeaderValue::from_static(origin));
        headers.append("origin", HeaderValue::from_static(origin));
        assert!(boundary(&headers, host, origin, true).is_err());
        headers.remove("origin");
        headers.insert("host", HeaderValue::from_static("rebinding.example:12345"));
        assert!(boundary(&headers, host, origin, false).is_err());
        headers.insert("host", HeaderValue::from_static(host));
        headers.insert("sec-fetch-site", HeaderValue::from_static("same-site"));
        assert!(boundary(&headers, host, origin, false).is_err());
        headers.remove("sec-fetch-site");
        headers.append("host", HeaderValue::from_static(host));
        assert!(boundary(&headers, host, origin, false).is_err());
    }
}
