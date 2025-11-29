pub mod request;

use std::collections::HashMap;

/// HTTP method
#[derive(Debug, Clone)]
pub enum HttpMethod {
    Get,
    Post,
    Put,
    Delete,
    Patch,
    Head,
    Options,
}

impl HttpMethod {
    pub fn from_str(s: &str) -> Option<Self> {
        match s.to_uppercase().as_str() {
            "GET" => Some(Self::Get),
            "POST" => Some(Self::Post),
            "PUT" => Some(Self::Put),
            "DELETE" => Some(Self::Delete),
            "PATCH" => Some(Self::Patch),
            "HEAD" => Some(Self::Head),
            "OPTIONS" => Some(Self::Options),
            _ => None,
        }
    }

    pub fn as_str(&self) -> &str {
        match self {
            Self::Get => "GET",
            Self::Post => "POST",
            Self::Put => "PUT",
            Self::Delete => "DELETE",
            Self::Patch => "PATCH",
            Self::Head => "HEAD",
            Self::Options => "OPTIONS",
        }
    }
}

/// Fetch request parameters
#[derive(Debug, Clone)]
pub struct FetchRequest {
    pub url: String,
    pub method: HttpMethod,
    pub headers: HashMap<String, String>,
    pub body: Option<String>,
}

impl Default for FetchRequest {
    fn default() -> Self {
        Self {
            url: String::new(),
            method: HttpMethod::Get,
            headers: HashMap::new(),
            body: None,
        }
    }
}

/// Fetch response metadata (for streaming - body comes via stream)
#[derive(Debug, Clone)]
pub struct FetchResponseMeta {
    pub status: u16,
    pub status_text: String,
    pub headers: HashMap<String, String>,
}
