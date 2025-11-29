use crate::runtime::stream_manager::{StreamChunk, StreamId, StreamManager};
use futures::StreamExt;
use openworkers_core::{HttpBody, HttpMethod, HttpRequest, HttpResponseMeta};
use std::sync::Arc;

/// Execute HTTP request with true streaming - returns immediately with metadata,
/// body chunks are streamed via StreamManager
pub async fn execute_fetch_streaming(
    request: HttpRequest,
    stream_manager: Arc<StreamManager>,
) -> Result<(HttpResponseMeta, StreamId), String> {
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
    for (key, value) in &request.headers {
        req_builder = req_builder.header(key, value);
    }

    // Add body if present
    if let HttpBody::Bytes(body) = request.body {
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

    // Create stream for body chunks
    let stream_id = stream_manager.create_stream(request.url.clone());

    // Spawn task to stream body chunks (with backpressure support)
    let manager = stream_manager.clone();
    tokio::spawn(async move {
        let mut stream = response.bytes_stream();

        while let Some(chunk_result) = stream.next().await {
            match chunk_result {
                Ok(chunk) => {
                    // write_chunk is async and will wait if buffer is full (backpressure)
                    if manager
                        .write_chunk(stream_id, StreamChunk::Data(chunk))
                        .await
                        .is_err()
                    {
                        // Stream was closed/cancelled
                        break;
                    }
                }
                Err(e) => {
                    let _ = manager
                        .write_chunk(stream_id, StreamChunk::Error(e.to_string()))
                        .await;
                    return;
                }
            }
        }

        // Signal end of stream
        let _ = manager.write_chunk(stream_id, StreamChunk::Done).await;
    });

    Ok((
        HttpResponseMeta {
            status,
            status_text,
            headers,
        },
        stream_id,
    ))
}
