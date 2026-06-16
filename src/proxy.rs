use axum::{Json, extract::State, http::StatusCode, response::IntoResponse};
use base64::{Engine, engine::general_purpose::STANDARD as BASE64};
use reqwest::{Client, Method, header::{HeaderMap, HeaderName, HeaderValue}};
use serde::{Deserialize, Serialize};
use serde_json::json;
use std::{collections::HashMap, str::FromStr, sync::Arc};

#[derive(Clone)]
pub struct ProxyState {
    pub client: Arc<Client>,
}

impl ProxyState {
    pub fn new() -> Self {
        Self {
            client: Arc::new(Client::builder().build().expect("failed to build reqwest client")),
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
}

pub async fn handle_proxy(
    State(state): State<ProxyState>,
    Json(req): Json<ProxyRequest>,
) -> impl IntoResponse {
    match forward(state, req).await {
        Ok(resp) => (StatusCode::OK, Json(json!(resp))).into_response(),
        Err(e) => (
            StatusCode::BAD_GATEWAY,
            Json(json!({ "error": e.to_string() })),
        )
            .into_response(),
    }
}

async fn forward(
    state: ProxyState,
    req: ProxyRequest,
) -> Result<ProxyResponse, Box<dyn std::error::Error + Send + Sync>> {
    let method = Method::from_str(&req.method.to_uppercase())
        .map_err(|_| format!("invalid HTTP method: {}", req.method))?;

    let mut headers = HeaderMap::new();
    if let Some(h) = req.headers {
        for (k, v) in h {
            let name = HeaderName::from_str(&k)
                .map_err(|_| format!("invalid header name: {}", k))?;
            let value = HeaderValue::from_str(&v)
                .map_err(|_| format!("invalid header value for {}", k))?;
            headers.insert(name, value);
        }
    }

    let mut builder = state.client.request(method, &req.url).headers(headers);
    if let Some(body) = req.body {
        builder = builder.body(body);
    }

    let response = builder.send().await?;
    let status = response.status().as_u16();

    let mut resp_headers = HashMap::new();
    for (k, v) in response.headers() {
        if let Ok(val) = v.to_str() {
            resp_headers.insert(k.to_string(), val.to_string());
        }
    }

    let body = BASE64.encode(response.bytes().await?);

    Ok(ProxyResponse {
        status,
        headers: resp_headers,
        body,
    })
}
