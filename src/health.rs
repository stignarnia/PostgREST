use axum::{Json, response::IntoResponse};
use serde::Serialize;
use serde_json::json;
use std::{
    collections::VecDeque,
    sync::{LazyLock, Mutex, MutexGuard},
    time::{Instant, SystemTime, UNIX_EPOCH},
};

const MAX_ERRORS: usize = 50;

/// One failure, held in RAM only. Every field is a fixed label or a value parsed
/// into a narrow shape (a host name, a 5-char SQLSTATE) — never an error message,
/// which may echo request data. The caller already got the full error in its
/// response.
#[derive(Clone, Serialize)]
struct ErrorEntry {
    at: u64,
    source: &'static str,
    kind: &'static str,
    host: Option<String>,
    code: Option<String>,
}

#[derive(Clone, Default, Serialize)]
pub struct UpdaterStatus {
    pub enabled: bool,
    pub last_check: Option<u64>,
    pub latest_release: Option<String>,
    pub last_error: Option<&'static str>,
}

struct Health {
    started: Instant,
    errors: Mutex<VecDeque<ErrorEntry>>,
    updater: Mutex<UpdaterStatus>,
}

static HEALTH: LazyLock<Health> = LazyLock::new(|| Health {
    started: Instant::now(),
    errors: Mutex::new(VecDeque::with_capacity(MAX_ERRORS)),
    updater: Mutex::new(UpdaterStatus::default()),
});

// A panic while holding the lock must not take /health down with it.
fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

pub fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Starts the uptime clock.
pub fn init() {
    LazyLock::force(&HEALTH);
}

pub fn record(
    source: &'static str,
    kind: &'static str,
    host: Option<String>,
    code: Option<String>,
) {
    let mut errors = lock(&HEALTH.errors);
    if errors.len() == MAX_ERRORS {
        errors.pop_front();
    }
    errors.push_back(ErrorEntry {
        at: now(),
        source,
        kind,
        host,
        code,
    });
}

pub fn update_updater(f: impl FnOnce(&mut UpdaterStatus)) {
    f(&mut lock(&HEALTH.updater));
}

pub async fn handle_health() -> impl IntoResponse {
    let errors: Vec<ErrorEntry> = lock(&HEALTH.errors).iter().cloned().collect();
    let updater = lock(&HEALTH.updater).clone();
    Json(json!({
        "version": env!("CARGO_PKG_VERSION"),
        "uptime_secs": HEALTH.started.elapsed().as_secs(),
        "updater": updater,
        "errors": errors,
    }))
}
