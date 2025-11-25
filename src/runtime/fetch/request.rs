use super::{FetchRequest, HttpMethod};
use bytes::Bytes;
use futures::StreamExt;

/// Execute HTTP request using reqwest with streaming support
pub async fn execute_fetch(request: FetchRequest) -> Result<super::FetchResponse, String> {
    let client = reqwest::Client::new();

    let mut req_builder = match request.method {
        HttpMethod::Get => client.get(&request.url),
        HttpMethod::Post => client.post(&request.url),
        HttpMethod::Put => client.put(&request.url),
        HttpMethod::Delete => client.delete(&request.url),
        HttpMethod::Patch => client.patch(&request.url),
        HttpMethod::Head => client.head(&request.url),
        HttpMethod::Options => {
            return Err("OPTIONS method not yet supported".to_string());
        }
    };

    // Add headers
    for (key, value) in request.headers {
        req_builder = req_builder.header(key, value);
    }

    // Add body if present
    if let Some(body) = request.body {
        req_builder = req_builder.body(body);
    }

    // Execute request
    let response = req_builder
        .send()
        .await
        .map_err(|e| format!("Request failed: {}", e))?;

    let status = response.status().as_u16();
    let status_text = response
        .status()
        .canonical_reason()
        .unwrap_or("")
        .to_string();

    let mut headers = std::collections::HashMap::new();
    for (key, value) in response.headers() {
        if let Ok(value_str) = value.to_str() {
            headers.insert(key.to_string(), value_str.to_string());
        }
    }

    // Read body as chunks (for streaming support)
    let mut chunks = Vec::new();
    let mut stream = response.bytes_stream();

    while let Some(chunk_result) = stream.next().await {
        match chunk_result {
            Ok(chunk) => chunks.push(chunk),
            Err(e) => return Err(format!("Failed to read chunk: {}", e)),
        }
    }

    // Also provide full body for backward compatibility
    let full_body = if chunks.is_empty() {
        Bytes::new()
    } else if chunks.len() == 1 {
        chunks[0].clone()
    } else {
        // Concatenate chunks
        let total_len = chunks.iter().map(|c| c.len()).sum();
        let mut full = Vec::with_capacity(total_len);
        for chunk in &chunks {
            full.extend_from_slice(chunk);
        }
        Bytes::from(full)
    };

    Ok(super::FetchResponse {
        status,
        status_text,
        headers,
        body: full_body,
        chunks,
    })
}
