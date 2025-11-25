use bytes::Bytes;
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use tokio::sync::mpsc;

pub type StreamId = u64;

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
#[derive(Clone)]
pub struct StreamManager {
    /// Senders for writing chunks to streams
    senders: Arc<Mutex<HashMap<StreamId, mpsc::UnboundedSender<StreamChunk>>>>,
    /// Receivers for reading chunks from streams (taken temporarily during reads)
    receivers: Arc<Mutex<HashMap<StreamId, mpsc::UnboundedReceiver<StreamChunk>>>>,
    /// Stream metadata (URL for debugging)
    metadata: Arc<Mutex<HashMap<StreamId, String>>>,
    /// Next stream ID to allocate
    next_id: Arc<Mutex<StreamId>>,
}

impl StreamManager {
    pub fn new() -> Self {
        Self {
            senders: Arc::new(Mutex::new(HashMap::new())),
            receivers: Arc::new(Mutex::new(HashMap::new())),
            metadata: Arc::new(Mutex::new(HashMap::new())),
            next_id: Arc::new(Mutex::new(1)),
        }
    }

    /// Create a new stream and return its ID
    /// The receiver is stored internally and can be read via `read_chunk`
    pub fn create_stream(&self, url: String) -> StreamId {
        let (tx, rx) = mpsc::unbounded_channel();

        let mut next_id = self.next_id.lock().unwrap();
        let id = *next_id;
        *next_id += 1;

        self.senders.lock().unwrap().insert(id, tx);
        self.receivers.lock().unwrap().insert(id, rx);
        self.metadata.lock().unwrap().insert(id, url);

        id
    }

    /// Write a chunk to a stream (called by Rust when data arrives)
    pub fn write_chunk(&self, stream_id: StreamId, chunk: StreamChunk) -> Result<(), String> {
        let senders = self.senders.lock().unwrap();

        if let Some(tx) = senders.get(&stream_id) {
            tx.send(chunk)
                .map_err(|_| "Stream channel closed".to_string())
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

    /// Count active streams
    pub fn active_count(&self) -> usize {
        self.senders.lock().unwrap().len()
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

        // Write a chunk
        manager
            .write_chunk(id, StreamChunk::Data(Bytes::from("test")))
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

        // Write multiple chunks
        manager
            .write_chunk(id, StreamChunk::Data(Bytes::from("chunk1")))
            .unwrap();
        manager
            .write_chunk(id, StreamChunk::Data(Bytes::from("chunk2")))
            .unwrap();
        manager.write_chunk(id, StreamChunk::Done).unwrap();

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
}
