use axum::{
    Router,
    extract::{Json, State},
    http::StatusCode,
    response::IntoResponse,
    routing::post,
};
use proxy::{ProxyState, handle_proxy};
use moka::future::Cache;
use openssl::{
    pkey::PKey,
    ssl::{SslConnector, SslMethod, SslVerifyMode},
    x509::X509,
};
use postgres_openssl::MakeTlsConnector;
use serde::Deserialize;
use serde_json::{Value, json};
use std::{net::SocketAddr, sync::Arc};
use tokio_postgres::Client;

mod cli;
mod proxy;

#[derive(Clone, Eq, PartialEq, Hash)]
struct ConnectionKey {
    host: String,
    port: u16,
    dbname: String,
    user: String,
    password: String,
    sslmode: String,
    sslcert: String,
    sslkey: String,
    sslrootcert: String,
}

impl ConnectionKey {
    fn from_request(req: &QueryRequest) -> Self {
        Self {
            host: req.host.clone(),
            port: req.port.unwrap_or(5432),
            dbname: req.dbname.clone(),
            user: req.user.clone(),
            password: req.password.clone().unwrap_or_default(),
            sslmode: req
                .sslmode
                .clone()
                .unwrap_or_else(|| DEFAULT_SSLMODE.into()),
            sslcert: req.sslcert.clone().unwrap_or_default(),
            sslkey: req.sslkey.clone().unwrap_or_default(),
            sslrootcert: req.sslrootcert.clone().unwrap_or_default(),
        }
    }
}

type ClientCache = Cache<ConnectionKey, Arc<Client>>;

#[derive(Clone)]
struct AppState {
    clients: ClientCache,
}

#[derive(Deserialize)]
struct QueryRequest {
    host: String,
    port: Option<u16>,
    dbname: String,
    user: String,
    password: Option<String>,
    sslmode: Option<String>,
    sslcert: Option<String>,
    sslkey: Option<String>,
    sslrootcert: Option<String>,
    query: String,
    params: Option<Vec<Value>>,
}

async fn handle_query(
    State(state): State<AppState>,
    Json(req): Json<QueryRequest>,
) -> impl IntoResponse {
    match run_query(state, req).await {
        Ok(rows) => (StatusCode::OK, Json(json!({ "rows": rows }))).into_response(),
        Err(e) => {
            let mut msg = e.to_string();
            let mut src = e.source();
            while let Some(s) = src {
                msg.push_str(&format!(": {}", s));
                src = s.source();
            }
            (StatusCode::BAD_REQUEST, Json(json!({ "error": msg }))).into_response()
        }
    }
}

async fn run_query(
    state: AppState,
    req: QueryRequest,
) -> Result<Vec<Value>, Box<dyn std::error::Error + Send + Sync>> {
    let client = get_or_create_client(&state.clients, &req).await?;

    let rows = if let Some(params) = &req.params {
        let string_params: Vec<String> = params
            .iter()
            .map(|v| match v {
                Value::String(s) => s.clone(),
                other => other.to_string(),
            })
            .collect();
        let borrowed: Vec<&(dyn tokio_postgres::types::ToSql + Sync)> = string_params
            .iter()
            .map(|s| s as &(dyn tokio_postgres::types::ToSql + Sync))
            .collect();
        client
            .query(&req.query as &str, borrowed.as_slice())
            .await?
    } else {
        client.query(&req.query as &str, &[]).await?
    };

    let result = rows
        .iter()
        .map(|row| {
            let mut obj = serde_json::Map::new();
            for (i, col) in row.columns().iter().enumerate() {
                obj.insert(col.name().to_string(), pg_value_to_json(row, i));
            }
            Value::Object(obj)
        })
        .collect();

    Ok(result)
}

async fn get_or_create_client(
    cache: &ClientCache,
    req: &QueryRequest,
) -> Result<Arc<Client>, Box<dyn std::error::Error + Send + Sync>> {
    let key = ConnectionKey::from_request(req);

    if let Some(client) = cache.get(&key).await {
        if !client.is_closed() {
            return Ok(client);
        }
        cache.invalidate(&key).await;
    }

    cache
        .try_get_with(key.clone(), async {
            open_connection(req).await.map(Arc::new)
        })
        .await
        .map_err(|e| format!("Failed to connect: {}", e).into())
}

const DEFAULT_SSLMODE: &str = "prefer";

struct SslPolicy {
    pg_sslmode: &'static str,
    verify_peer: bool,
    verify_hostname: bool,
}

fn ssl_policy(sslmode: &str) -> SslPolicy {
    match sslmode {
        "disable" => SslPolicy {
            pg_sslmode: "disable",
            verify_peer: false,
            verify_hostname: false,
        },
        "allow" => SslPolicy {
            pg_sslmode: "prefer",
            verify_peer: false,
            verify_hostname: false,
        },
        "prefer" => SslPolicy {
            pg_sslmode: "prefer",
            verify_peer: false,
            verify_hostname: false,
        },
        "require" => SslPolicy {
            pg_sslmode: "require",
            verify_peer: false,
            verify_hostname: false,
        },
        "verify-ca" => SslPolicy {
            pg_sslmode: "require",
            verify_peer: true,
            verify_hostname: false,
        },
        "verify-full" => SslPolicy {
            pg_sslmode: "require",
            verify_peer: true,
            verify_hostname: true,
        },
        _ => SslPolicy {
            pg_sslmode: "prefer",
            verify_peer: false,
            verify_hostname: false,
        },
    }
}

async fn open_connection(
    req: &QueryRequest,
) -> Result<Client, Box<dyn std::error::Error + Send + Sync>> {
    let port = req.port.unwrap_or(5432);
    let sslmode = req.sslmode.as_deref().unwrap_or(DEFAULT_SSLMODE);
    let policy = ssl_policy(sslmode);

    let mut config = tokio_postgres::Config::new();
    config
        .host(req.host.trim())
        .port(port)
        .dbname(req.dbname.trim())
        .user(req.user.trim());
    if let Some(ref pw) = req.password {
        config.password(pw.trim_end_matches(|c| c == '\n' || c == '\r'));
    }
    config.ssl_mode(match policy.pg_sslmode {
        "disable" => tokio_postgres::config::SslMode::Disable,
        "require" => tokio_postgres::config::SslMode::Require,
        _ => tokio_postgres::config::SslMode::Prefer,
    });

    let tls = build_tls_connector(&policy, req)?;
    let (client, connection) = config.connect(tls).await?;

    tokio::spawn(async move {
        if let Err(e) = connection.await {
            eprintln!("connection dropped: {}", e);
        }
    });

    Ok(client)
}

fn build_tls_connector(
    policy: &SslPolicy,
    req: &QueryRequest,
) -> Result<MakeTlsConnector, Box<dyn std::error::Error + Send + Sync>> {
    let mut builder = SslConnector::builder(SslMethod::tls())?;

    if policy.verify_peer {
        builder.set_verify(SslVerifyMode::PEER);
        if let Some(ref pem) = req.sslrootcert {
            let cert = X509::from_pem(pem.as_bytes())?;
            builder.cert_store_mut().add_cert(cert)?;
        }
    } else {
        builder.set_verify(SslVerifyMode::NONE);
    }

    if let Some(ref cert_pem) = req.sslcert {
        let cert = X509::from_pem(cert_pem.as_bytes())?;
        builder.set_certificate(&cert)?;
    }
    if let Some(ref key_pem) = req.sslkey {
        let key = PKey::private_key_from_pem(key_pem.as_bytes())?;
        builder.set_private_key(&key)?;
    }

    let mut connector = MakeTlsConnector::new(builder.build());
    let verify_hostname = policy.verify_hostname;
    connector.set_callback(move |config, _domain| {
        config.set_verify_hostname(verify_hostname);
        Ok(())
    });

    Ok(connector)
}

fn pg_value_to_json(row: &tokio_postgres::Row, idx: usize) -> Value {
    use tokio_postgres::types::Type;

    fn get<'a, T>(row: &'a tokio_postgres::Row, idx: usize) -> Value
    where
        T: tokio_postgres::types::FromSql<'a> + Into<Value>,
    {
        row.try_get::<_, Option<T>>(idx)
            .ok()
            .flatten()
            .map(Into::into)
            .unwrap_or(Value::Null)
    }

    let col_type = match row.columns().get(idx) {
        Some(col) => col.type_().clone(),
        _ => return Value::Null,
    };

    match col_type {
        Type::BOOL => get::<bool>(row, idx),
        Type::INT2 => get::<i16>(row, idx),
        Type::INT4 => get::<i32>(row, idx),
        Type::INT8 => get::<i64>(row, idx),
        Type::FLOAT4 => get::<f32>(row, idx),
        Type::FLOAT8 => get::<f64>(row, idx),
        Type::JSON | Type::JSONB => get::<Value>(row, idx),
        _ => get::<String>(row, idx),
    }
}

pub async fn run_server() -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let state = AppState {
        clients: Cache::builder().max_capacity(100).build(),
    };

    let proxy_state = ProxyState::new();

    let app = Router::new()
        .route("/query", post(handle_query))
        .with_state(state)
        .route("/proxy", post(handle_proxy))
        .with_state(proxy_state);

    let addr = SocketAddr::from(([127, 0, 0, 1], 3000));
    println!("PostgREST listening on {}", addr);
    let listener = tokio::net::TcpListener::bind(addr).await?;
    axum::serve(listener, app).await?;
    Ok(())
}

#[cfg(windows)]
mod windows_service_impl {
    use std::time::Duration;
    use tokio::sync::oneshot;
    use windows_service::{
        define_windows_service,
        service::{
            ServiceControl, ServiceControlAccept, ServiceExitCode, ServiceState, ServiceStatus,
            ServiceType,
        },
        service_control_handler::{self, ServiceControlHandlerResult},
        service_dispatcher,
    };

    define_windows_service!(ffi_service_main, windows_service_main);

    pub fn run() -> Result<(), Box<dyn std::error::Error>> {
        service_dispatcher::start("postgrest-server", ffi_service_main)?;
        Ok(())
    }

    fn windows_service_main(_arguments: Vec<std::ffi::OsString>) {
        if let Err(_e) = run_service() {
            // Log error
        }
    }

    fn run_service() -> Result<(), Box<dyn std::error::Error>> {
        let (tx, rx) = oneshot::channel();
        let mut tx = Some(tx);

        let event_handler = move |control_event| -> ServiceControlHandlerResult {
            match control_event {
                ServiceControl::Stop => {
                    if let Some(sender) = tx.take() {
                        let _ = sender.send(());
                    }
                    ServiceControlHandlerResult::NoError
                }
                ServiceControl::Interrogate => ServiceControlHandlerResult::NoError,
                _ => ServiceControlHandlerResult::NotImplemented,
            }
        };

        let status_handle = service_control_handler::register("postgrest-server", event_handler)?;

        status_handle.set_service_status(ServiceStatus {
            service_type: ServiceType::OWN_PROCESS,
            current_state: ServiceState::Running,
            controls_accepted: ServiceControlAccept::STOP,
            exit_code: ServiceExitCode::Win32(0),
            checkpoint: 0,
            wait_hint: Duration::default(),
            process_id: None,
        })?;

        let rt = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()?;

        rt.block_on(async {
            tokio::select! {
                _ = crate::run_server() => {},
                _ = rx => {},
            }
        });

        status_handle.set_service_status(ServiceStatus {
            service_type: ServiceType::OWN_PROCESS,
            current_state: ServiceState::Stopped,
            controls_accepted: ServiceControlAccept::empty(),
            exit_code: ServiceExitCode::Win32(0),
            checkpoint: 0,
            wait_hint: Duration::default(),
            process_id: None,
        })?;

        Ok(())
    }
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    if cli::handle_cli()? {
        return Ok(());
    }

    #[cfg(windows)]
    {
        if let Err(e) = windows_service_impl::run() {
            let mut is_not_service = false;
            if let Some(win_err) = e.downcast_ref::<windows_service::Error>() {
                if let windows_service::Error::Winapi(io_err) = win_err {
                    if io_err.raw_os_error() == Some(1063) {
                        is_not_service = true;
                    }
                }
            }

            if !is_not_service {
                return Err(e);
            }
        } else {
            return Ok(());
        }
    }

    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?;

    rt.block_on(async {
        if let Err(e) = run_server().await {
            eprintln!("Server error: {}", e);
        }
    });

    Ok(())
}
