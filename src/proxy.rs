use crate::health;
use axum::{Json, extract::State, http::StatusCode, response::IntoResponse};
use base64::{Engine, engine::general_purpose::STANDARD as BASE64};
use reqwest::{
    Client, Method, Url,
    cookie::{CookieStore, Jar},
    header::{self, HeaderMap, HeaderName, HeaderValue},
    redirect::Policy,
};
use serde::{Deserialize, Serialize};
use serde_json::json;
use std::{collections::HashMap, str::FromStr, sync::Arc};

const MAX_REDIRECTS: usize = 10;

#[derive(Debug)]
struct TooManyRedirects;

impl std::fmt::Display for TooManyRedirects {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "too many redirects (>{MAX_REDIRECTS})")
    }
}

impl std::error::Error for TooManyRedirects {}

// A fixed label for /health; the message itself can carry the URL.
fn error_kind(e: &(dyn std::error::Error + Send + Sync + 'static)) -> &'static str {
    if e.is::<TooManyRedirects>() {
        return "too_many_redirects";
    }
    match e.downcast_ref::<reqwest::Error>() {
        Some(r) if r.is_timeout() => "timeout",
        Some(r) if r.is_connect() => "connect",
        Some(r) if r.is_body() || r.is_decode() => "body",
        Some(_) => "request",
        None => "invalid_request",
    }
}

#[derive(Clone)]
pub struct ProxyState {
    pub client: Arc<Client>,
}

impl ProxyState {
    pub fn new() -> Self {
        // Redirects are followed by hand in `forward`, so every hop can read and
        // send the request's own cookie jar while the client stays shared.
        Self {
            client: Arc::new(
                Client::builder()
                    .redirect(Policy::none())
                    .build()
                    .expect("failed to build reqwest client"),
            ),
        }
    }
}

#[derive(Deserialize)]
pub struct ProxyRequest {
    pub url: String,
    #[serde(default = "default_method")]
    pub method: String,
    pub headers: Option<HashMap<String, String>>,
    pub body: Option<String>,
}

fn default_method() -> String {
    "GET".to_string()
}

#[derive(Serialize)]
struct ProxyResponse {
    status: u16,
    headers: HashMap<String, String>,
    body: String,
    /// Where the redirect chain ended.
    url: String,
    /// The jar's cookies for `url`, as a ready `Cookie` header value. `headers`
    /// keeps one value per name, so this is where every Set-Cookie survives.
    cookie: Option<String>,
}

pub async fn handle_proxy(
    State(state): State<ProxyState>,
    Json(req): Json<ProxyRequest>,
) -> impl IntoResponse {
    let host = Url::parse(&req.url)
        .ok()
        .and_then(|u| u.host_str().map(String::from));
    match forward(state, req).await {
        Ok(resp) => (StatusCode::OK, Json(json!(resp))).into_response(),
        Err(e) => {
            health::record("proxy", error_kind(e.as_ref()), host, None);
            (
                StatusCode::BAD_GATEWAY,
                Json(json!({ "error": e.to_string() })),
            )
                .into_response()
        }
    }
}

async fn forward(
    state: ProxyState,
    req: ProxyRequest,
) -> Result<ProxyResponse, Box<dyn std::error::Error + Send + Sync>> {
    let mut method = Method::from_str(&req.method.to_uppercase())
        .map_err(|_| format!("invalid HTTP method: {}", req.method))?;
    let mut url = Url::parse(&req.url).map_err(|_| format!("invalid URL: {}", req.url))?;

    // The caller's Cookie header seeds the jar instead of being sent as is, so
    // it is scoped to its host and merged with what the hops set.
    let jar = Jar::default();
    let mut headers = HeaderMap::new();
    if let Some(h) = req.headers {
        for (k, v) in h {
            if k.eq_ignore_ascii_case("cookie") {
                for pair in v.split(';').map(str::trim).filter(|p| !p.is_empty()) {
                    jar.add_cookie_str(pair, &url);
                }
                continue;
            }
            let name = HeaderName::from_str(&k)
                .map_err(|_| format!("invalid header name: {}", k))?;
            let value = HeaderValue::from_str(&v)
                .map_err(|_| format!("invalid header value for {}", k))?;
            headers.insert(name, value);
        }
    }
    let mut body = req.body;

    for _ in 0..=MAX_REDIRECTS {
        let mut hop = headers.clone();
        if let Some(cookie) = jar.cookies(&url) {
            hop.insert(header::COOKIE, cookie);
        }
        let mut builder = state.client.request(method.clone(), url.clone()).headers(hop);
        if let Some(b) = &body {
            builder = builder.body(b.clone());
        }

        let response = builder.send().await?;
        jar.set_cookies(&mut response.headers().get_all(header::SET_COOKIE).iter(), &url);

        let status = response.status();
        let location = response
            .headers()
            .get(header::LOCATION)
            .and_then(|v| v.to_str().ok())
            .and_then(|l| url.join(l).ok());

        let next = match location {
            Some(next) if status.is_redirection() => next,
            _ => {
                let mut resp_headers = HashMap::new();
                for (k, v) in response.headers() {
                    if let Ok(val) = v.to_str() {
                        resp_headers.insert(k.to_string(), val.to_string());
                    }
                }
                let cookie = jar
                    .cookies(&url)
                    .and_then(|c| c.to_str().ok().map(String::from));
                let body = BASE64.encode(response.bytes().await?);
                return Ok(ProxyResponse {
                    status: status.as_u16(),
                    headers: resp_headers,
                    body,
                    url: url.to_string(),
                    cookie,
                });
            }
        };

        // Browser semantics: 307/308 replay the request; 301/302 turn a POST
        // into a GET and 303 turns anything but HEAD into one, dropping the body.
        let replay = matches!(status.as_u16(), 307 | 308);
        if !replay
            && (status.as_u16() == 303 && method != Method::HEAD
                || method == Method::POST)
        {
            method = Method::GET;
            body = None;
            headers.remove(header::CONTENT_TYPE);
            headers.remove(header::CONTENT_LENGTH);
        }
        // Credentials the caller set for one origin never follow to another.
        if next.origin() != url.origin() {
            headers.remove(header::AUTHORIZATION);
            headers.remove(header::PROXY_AUTHORIZATION);
        }
        url = next;
    }

    Err(TooManyRedirects.into())
}
