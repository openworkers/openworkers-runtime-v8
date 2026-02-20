use bytes::Bytes;
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use tokio::sync::mpsc;

pub type StreamId = u64;

/// Default high water mark for stream buffers (number of chunks)
/// This provides backpressure when the consumer is slow
pub const DEFAULT_HIGH_WATER_MARK: usize = 16;

/// A chunk of data from a stream
#[derive(Debug, Clone)]
pub enum StreamChunk {
    /// Data chunk (bytes)
    Data(Bytes),
    /// Stream finished successfully
    Done,
    /// Stream errored
    Error(String),
}

/// Manages all active streams and their communication channels
/// Stores both senders (for writing) and receivers (for reading) internally
/// Uses bounded channels for backpressure support
#[derive(Clone)]
pub struct StreamManager {
    /// Senders for writing chunks to streams (bounded for backpressure)
    senders: Arc<Mutex<HashMap<StreamId, mpsc::Sender<StreamChunk>>>>,
    /// Receivers for reading chunks from streams (taken temporarily during reads)
    receivers: Arc<Mutex<HashMap<StreamId, mpsc::Receiver<StreamChunk>>>>,
    /// Stream metadata (URL for debugging)
    metadata: Arc<Mutex<HashMap<StreamId, String>>>,
    /// Next stream ID to allocate
    next_id: Arc<Mutex<StreamId>>,
    /// High water mark for new streams
    high_water_mark: usize,
}

impl StreamManager {
    pub fn new() -> Self {
        Self::with_high_water_mark(DEFAULT_HIGH_WATER_MARK)
    }

    /// Create a StreamManager with a custom high water mark
    pub fn with_high_water_mark(high_water_mark: usize) -> Self {
        Self {
            senders: Arc::new(Mutex::new(HashMap::new())),
            receivers: Arc::new(Mutex::new(HashMap::new())),
            metadata: Arc::new(Mutex::new(HashMap::new())),
            next_id: Arc::new(Mutex::new(1)),
            high_water_mark,
        }
    }

    /// Create a new stream and return its ID
    /// The receiver is stored internally and can be read via `read_chunk`
    /// Uses bounded channel with high_water_mark capacity for backpressure
    pub fn create_stream(&self, url: String) -> StreamId {
        let (tx, rx) = mpsc::channel(self.high_water_mark);

        let mut next_id = self.next_id.lock().unwrap();
        let id = *next_id;
        *next_id += 1;

        self.senders.lock().unwrap().insert(id, tx);
        self.receivers.lock().unwrap().insert(id, rx);
        self.metadata.lock().unwrap().insert(id, url);

        id
    }

    /// Write a chunk to a stream (async - waits if buffer is full for backpressure)
    /// Called by Rust when data arrives from upstream
    /// When the channel is closed (receiver dropped), removes the sender from the map
    pub async fn write_chunk(&self, stream_id: StreamId, chunk: StreamChunk) -> Result<(), String> {
        // Get a clone of the sender (so we don't hold the lock during await)
        let tx = {
            let senders = self.senders.lock().unwrap();
            senders.get(&stream_id).cloned()
        };

        if let Some(tx) = tx {
            match tx.send(chunk).await {
                Ok(()) => Ok(()),
                Err(_) => {
                    // Receiver was dropped (client disconnected) - remove sender from map
                    self.senders.lock().unwrap().remove(&stream_id);
                    Err("Stream channel closed".to_string())
                }
            }
        } else {
            Err(format!("Stream {} not found", stream_id))
        }
    }

    /// Try to write a chunk without waiting (returns error if buffer is full)
    /// Useful for non-async contexts
    /// When the channel is closed (receiver dropped), removes the sender from the map
    pub fn try_write_chunk(&self, stream_id: StreamId, chunk: StreamChunk) -> Result<(), String> {
        let mut senders = self.senders.lock().unwrap();

        if let Some(tx) = senders.get(&stream_id) {
            match tx.try_send(chunk) {
                Ok(()) => Ok(()),
                Err(mpsc::error::TrySendError::Full(_)) => {
                    Err("Stream buffer full (backpressure)".to_string())
                }
                Err(mpsc::error::TrySendError::Closed(_)) => {
                    // Receiver was dropped (client disconnected) - remove sender from map
                    // This makes has_sender() return false, which __responseStreamIsClosed uses
                    senders.remove(&stream_id);
                    Err("Stream channel closed".to_string())
                }
            }
        } else {
            Err(format!("Stream {} not found", stream_id))
        }
    }

    /// Read the next chunk from a stream (async, called from scheduler)
    /// This temporarily takes ownership of the receiver to await on it
    pub async fn read_chunk(&self, stream_id: StreamId) -> Result<StreamChunk, String> {
        // Take the receiver temporarily (only one reader at a time - matches ReadableStream semantics)
        let rx = {
            let mut receivers = self.receivers.lock().unwrap();
            receivers.remove(&stream_id)
        };

        if let Some(mut rx) = rx {
            let result = rx.recv().await;

            // Put it back for next read (unless stream is done/errored)
            if let Some(ref chunk) = result {
                match chunk {
                    StreamChunk::Done | StreamChunk::Error(_) => {
                        // Don't put back - stream is finished
                    }
                    StreamChunk::Data(_) => {
                        self.receivers.lock().unwrap().insert(stream_id, rx);
                    }
                }
            }

            result.ok_or_else(|| "Stream closed unexpectedly".to_string())
        } else {
            Err(format!(
                "Stream {} not found or already being read",
                stream_id
            ))
        }
    }

    /// Take the receiver from a stream (for passing to HttpBody::Stream)
    /// The sender remains active for writing chunks
    pub fn take_receiver(&self, stream_id: StreamId) -> Option<mpsc::Receiver<StreamChunk>> {
        self.receivers.lock().unwrap().remove(&stream_id)
    }

    /// Close and remove a stream
    pub fn close_stream(&self, stream_id: StreamId) {
        self.senders.lock().unwrap().remove(&stream_id);
        self.receivers.lock().unwrap().remove(&stream_id);
        self.metadata.lock().unwrap().remove(&stream_id);
    }

    /// Get information about a stream (for debugging)
    pub fn get_stream_info(&self, stream_id: StreamId) -> Option<String> {
        self.metadata.lock().unwrap().get(&stream_id).cloned()
    }

    /// Clear all streams (used for context reuse between requests)
    pub fn clear(&self) {
        self.senders.lock().unwrap().clear();
        self.receivers.lock().unwrap().clear();
        self.metadata.lock().unwrap().clear();
        // Reset next_id so stream IDs don't grow unbounded across reuses
        *self.next_id.lock().unwrap() = 1;
    }

    /// Count active streams
    pub fn active_count(&self) -> usize {
        self.senders.lock().unwrap().len()
    }

    /// Check if a stream has an active sender (not closed)
    pub fn has_sender(&self, stream_id: StreamId) -> bool {
        self.senders.lock().unwrap().contains_key(&stream_id)
    }

    /// Create a stream and spawn a pump task to forward data from a receiver.
    /// Returns the stream ID that can be passed to JavaScript.
    ///
    /// This is the standard way to bridge a Rust mpsc::Receiver<Result<Bytes, String>>
    /// (e.g., from a streaming HTTP request body) to a JavaScript ReadableStream.
    pub fn pump_request_body(
        self: &std::sync::Arc<Self>,
        rx: tokio::sync::mpsc::Receiver<Result<bytes::Bytes, String>>,
    ) -> StreamId {
        let stream_id = self.create_stream("request_body".to_string());
        let stream_manager = self.clone();

        tokio::spawn(async move {
            let mut rx = rx;

            while let Some(result) = rx.recv().await {
                match result {
                    Ok(bytes) => {
                        if stream_manager
                            .write_chunk(stream_id, StreamChunk::Data(bytes))
                            .await
                            .is_err()
                        {
                            // Stream closed by consumer
                            break;
                        }
                    }
                    Err(e) => {
                        let _ = stream_manager
                            .write_chunk(stream_id, StreamChunk::Error(e))
                            .await;
                        break;
                    }
                }
            }

            // Signal end of stream
            let _ = stream_manager
                .write_chunk(stream_id, StreamChunk::Done)
                .await;
        });

        stream_id
    }
}

impl Default for StreamManager {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_stream_manager_create() {
        let manager = StreamManager::new();
        let id = manager.create_stream("https://example.com".to_string());

        assert_eq!(id, 1);
        assert_eq!(manager.active_count(), 1);

        // Write a chunk (now async)
        manager
            .write_chunk(id, StreamChunk::Data(Bytes::from("test")))
            .await
            .unwrap();

        // Read it using read_chunk
        let chunk = manager.read_chunk(id).await.unwrap();
        match chunk {
            StreamChunk::Data(data) => assert_eq!(data, Bytes::from("test")),
            _ => panic!("Expected data chunk"),
        }
    }

    #[tokio::test]
    async fn test_stream_manager_multiple_chunks() {
        let manager = StreamManager::new();
        let id = manager.create_stream("https://example.com".to_string());

        // Write multiple chunks (now async)
        manager
            .write_chunk(id, StreamChunk::Data(Bytes::from("chunk1")))
            .await
            .unwrap();
        manager
            .write_chunk(id, StreamChunk::Data(Bytes::from("chunk2")))
            .await
            .unwrap();
        manager.write_chunk(id, StreamChunk::Done).await.unwrap();

        // Read them using read_chunk
        let chunk1 = manager.read_chunk(id).await.unwrap();
        let chunk2 = manager.read_chunk(id).await.unwrap();
        let done = manager.read_chunk(id).await.unwrap();

        match (chunk1, chunk2, done) {
            (StreamChunk::Data(d1), StreamChunk::Data(d2), StreamChunk::Done) => {
                assert_eq!(d1, Bytes::from("chunk1"));
                assert_eq!(d2, Bytes::from("chunk2"));
            }
            _ => panic!("Expected data chunks + done"),
        }
    }

    #[tokio::test]
    async fn test_stream_manager_close() {
        let manager = StreamManager::new();
        let id = manager.create_stream("https://example.com".to_string());

        assert_eq!(manager.active_count(), 1);

        manager.close_stream(id);

        assert_eq!(manager.active_count(), 0);
    }

    #[tokio::test]
    async fn test_stream_info() {
        let manager = StreamManager::new();
        let id = manager.create_stream("https://api.example.com/data".to_string());

        assert_eq!(
            manager.get_stream_info(id),
            Some("https://api.example.com/data".to_string())
        );
    }

    #[tokio::test]
    async fn test_backpressure() {
        // Create manager with small buffer to test backpressure
        let manager = StreamManager::with_high_water_mark(2);
        let id = manager.create_stream("https://example.com".to_string());

        // Fill the buffer
        manager
            .write_chunk(id, StreamChunk::Data(Bytes::from("1")))
            .await
            .unwrap();
        manager
            .write_chunk(id, StreamChunk::Data(Bytes::from("2")))
            .await
            .unwrap();

        // try_write should fail when buffer is full
        let result = manager.try_write_chunk(id, StreamChunk::Data(Bytes::from("3")));
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("backpressure"));

        // Read one to free space
        let _ = manager.read_chunk(id).await.unwrap();

        // Now try_write should succeed
        let result = manager.try_write_chunk(id, StreamChunk::Data(Bytes::from("3")));
        assert!(result.is_ok());
    }
}
