use bytes::Bytes;
use std::collections::HashMap;
use tokio::sync::mpsc;

/// HTTP Request data
#[derive(Debug, Clone)]
pub struct HttpRequest {
    pub method: String,
    pub url: String,
    pub headers: HashMap<String, String>,
    pub body: Option<Bytes>,
}

/// Default buffer size for streaming responses (matches StreamManager)
pub const RESPONSE_STREAM_BUFFER_SIZE: usize = 16;

/// Response body - either complete bytes, a stream, or empty
pub enum ResponseBody {
    /// No body
    None,
    /// Complete body (already buffered)
    Bytes(Bytes),
    /// Streaming body - receiver yields chunks as they become available
    /// Uses bounded channel for backpressure support
    Stream(mpsc::Receiver<Result<Bytes, String>>),
}

impl std::fmt::Debug for ResponseBody {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ResponseBody::None => write!(f, "None"),
            ResponseBody::Bytes(b) => write!(f, "Bytes({} bytes)", b.len()),
            ResponseBody::Stream(_) => write!(f, "Stream(...)"),
        }
    }
}

impl ResponseBody {
    /// Get bytes if this is a Bytes variant, None otherwise
    pub fn as_bytes(&self) -> Option<&Bytes> {
        match self {
            ResponseBody::Bytes(b) => Some(b),
            _ => None,
        }
    }

    /// Check if this is an empty response
    pub fn is_none(&self) -> bool {
        matches!(self, ResponseBody::None)
    }

    /// Check if this is a streaming response
    pub fn is_stream(&self) -> bool {
        matches!(self, ResponseBody::Stream(_))
    }
}

/// HTTP Response data
#[derive(Debug)]
pub struct HttpResponse {
    pub status: u16,
    pub headers: Vec<(String, String)>,
    pub body: ResponseBody,
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
        use actix_web::body::BodyStream;
        use futures::StreamExt;
        use tokio_stream::wrappers::ReceiverStream;

        let mut builder = actix_web::HttpResponse::build(
            actix_web::http::StatusCode::from_u16(res.status)
                .unwrap_or(actix_web::http::StatusCode::INTERNAL_SERVER_ERROR),
        );

        for (key, value) in res.headers {
            builder.insert_header((key.as_str(), value.as_str()));
        }

        match res.body {
            ResponseBody::None => builder.finish(),
            ResponseBody::Bytes(body) => {
                if body.is_empty() {
                    builder.finish()
                } else {
                    builder.body(body)
                }
            }
            ResponseBody::Stream(rx) => {
                // Convert bounded mpsc receiver to a stream of actix-compatible chunks
                // Backpressure is automatic: when actix can't send fast enough,
                // the channel fills up and upstream producers will wait
                let stream = ReceiverStream::new(rx).map(|result| {
                    result.map_err(|e| std::io::Error::new(std::io::ErrorKind::Other, e))
                });
                builder.body(BodyStream::new(stream))
            }
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
