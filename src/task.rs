use bytes::Bytes;
use std::collections::HashMap;

/// HTTP Request data
#[derive(Debug, Clone)]
pub struct HttpRequest {
    pub method: String,
    pub url: String,
    pub headers: HashMap<String, String>,
    pub body: Option<Bytes>,
}

/// HTTP Response data
#[derive(Debug, Clone)]
pub struct HttpResponse {
    pub status: u16,
    pub headers: Vec<(String, String)>,
    pub body: Option<Bytes>,
}

// Actix-web conversions (only available when actix feature is enabled)
#[cfg(feature = "actix")]
impl HttpRequest {
    /// Convert from actix_web::HttpRequest + body bytes
    pub fn from_actix(req: &actix_web::HttpRequest, body: Bytes) -> Self {
        let method = req.method().to_string();
        let url = format!(
            "{}://{}{}",
            req.connection_info().scheme(),
            req.connection_info().host(),
            req.uri()
        );

        let mut headers = HashMap::new();
        for (key, value) in req.headers() {
            if let Ok(val_str) = value.to_str() {
                headers.insert(key.to_string(), val_str.to_string());
            }
        }

        HttpRequest {
            method,
            url,
            headers,
            body: if body.is_empty() { None } else { Some(body) },
        }
    }
}

#[cfg(feature = "actix")]
impl From<HttpResponse> for actix_web::HttpResponse {
    fn from(res: HttpResponse) -> Self {
        let mut builder = actix_web::HttpResponse::build(
            actix_web::http::StatusCode::from_u16(res.status)
                .unwrap_or(actix_web::http::StatusCode::INTERNAL_SERVER_ERROR),
        );

        for (key, value) in res.headers {
            builder.insert_header((key.as_str(), value.as_str()));
        }

        match res.body {
            Some(body) => builder.body(body),
            None => builder.finish(),
        }
    }
}

/// Fetch event initialization data
#[derive(Debug)]
pub struct FetchInit {
    pub(crate) req: HttpRequest,
    pub(crate) res_tx: tokio::sync::oneshot::Sender<HttpResponse>,
}

impl FetchInit {
    pub fn new(req: HttpRequest, res_tx: tokio::sync::oneshot::Sender<HttpResponse>) -> Self {
        Self { req, res_tx }
    }
}

/// Scheduled event initialization data
#[derive(Debug)]
pub struct ScheduledInit {
    pub(crate) time: u64,
    pub(crate) res_tx: tokio::sync::oneshot::Sender<()>,
}

impl ScheduledInit {
    /// Create ScheduledInit (openworkers-runtime compatible: res_tx first, time second)
    pub fn new(res_tx: tokio::sync::oneshot::Sender<()>, time: u64) -> Self {
        Self { time, res_tx }
    }
}

/// Task type discriminator
#[derive(Debug, Clone, Copy)]
pub enum TaskType {
    Fetch,
    Scheduled,
}

/// Task to be executed by a Worker
pub enum Task {
    Fetch(Option<FetchInit>),
    Scheduled(Option<ScheduledInit>),
}

impl Task {
    pub fn task_type(&self) -> TaskType {
        match self {
            Task::Fetch(_) => TaskType::Fetch,
            Task::Scheduled(_) => TaskType::Scheduled,
        }
    }

    /// Create a fetch task
    pub fn fetch(req: HttpRequest) -> (Self, tokio::sync::oneshot::Receiver<HttpResponse>) {
        let (tx, rx) = tokio::sync::oneshot::channel();
        (Task::Fetch(Some(FetchInit::new(req, tx))), rx)
    }

    /// Create a scheduled task
    pub fn scheduled(time: u64) -> (Self, tokio::sync::oneshot::Receiver<()>) {
        let (tx, rx) = tokio::sync::oneshot::channel();
        (Task::Scheduled(Some(ScheduledInit::new(tx, time))), rx)
    }
}
