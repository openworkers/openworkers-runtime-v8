//! Callback result builders for V8.
//!
//! This module contains functions that populate V8 objects for callback results.
//! Used by both `Runtime` and `ExecutionContext` to avoid code duplication.
//!
//! These functions use a "populate" pattern: the caller creates the object and
//! passes it to be populated. This avoids lifetime issues with returning `Local<T>`.

use super::stream_manager;
use openworkers_core::{DatabaseResult, HttpResponseMeta, KvResult, StorageResult};

/// Populate metadata object for FetchStreamingSuccess callback.
///
/// Sets: { status, statusText, headers, streamId }
pub fn populate_fetch_meta(
    scope: &mut v8::PinScope,
    meta_obj: v8::Local<v8::Object>,
    meta: &HttpResponseMeta,
    stream_id: stream_manager::StreamId,
) {
    // status
    let status_key = v8::String::new(scope, "status").unwrap();
    let status_val = v8::Number::new(scope, meta.status as f64);
    meta_obj.set(scope, status_key.into(), status_val.into());

    // statusText
    let status_text_key = v8::String::new(scope, "statusText").unwrap();
    let status_text_val = v8::String::new(scope, &meta.status_text).unwrap();
    meta_obj.set(scope, status_text_key.into(), status_text_val.into());

    // headers as array of [key, value] pairs (preserves duplicates like set-cookie)
    let headers_arr = v8::Array::new(scope, meta.headers.len() as i32);

    for (i, (key, value)) in meta.headers.iter().enumerate() {
        let pair = v8::Array::new(scope, 2);
        let k = v8::String::new(scope, key).unwrap();
        let v = v8::String::new(scope, value).unwrap();
        pair.set_index(scope, 0, k.into());
        pair.set_index(scope, 1, v.into());
        headers_arr.set_index(scope, i as u32, pair.into());
    }

    let headers_key = v8::String::new(scope, "headers").unwrap();
    meta_obj.set(scope, headers_key.into(), headers_arr.into());

    // streamId
    let stream_id_key = v8::String::new(scope, "streamId").unwrap();
    let stream_id_val = v8::Number::new(scope, stream_id as f64);
    meta_obj.set(scope, stream_id_key.into(), stream_id_val.into());
}

/// Populate result object for StreamChunk callback.
///
/// Sets: { done: boolean, value?: Uint8Array, error?: string }
pub fn populate_stream_chunk_result(
    scope: &mut v8::PinScope,
    result_obj: v8::Local<v8::Object>,
    chunk: stream_manager::StreamChunk,
) {
    match chunk {
        stream_manager::StreamChunk::Data(bytes) => {
            // {done: false, value: Uint8Array}
            let done_key = v8::String::new(scope, "done").unwrap();
            let done_val = v8::Boolean::new(scope, false);
            result_obj.set(scope, done_key.into(), done_val.into());

            // Create Uint8Array from bytes
            // Use Vec::from(bytes) instead of to_vec() - avoids copy if Bytes is uniquely owned
            let vec: Vec<u8> = bytes.into();
            let len = vec.len();
            let array_buffer = crate::v8_helpers::create_array_buffer_from_vec(scope, vec);
            let uint8_array = v8::Uint8Array::new(scope, array_buffer, 0, len).unwrap();

            let value_key = v8::String::new(scope, "value").unwrap();
            result_obj.set(scope, value_key.into(), uint8_array.into());
        }
        stream_manager::StreamChunk::Done => {
            // {done: true}
            let done_key = v8::String::new(scope, "done").unwrap();
            let done_val = v8::Boolean::new(scope, true);
            result_obj.set(scope, done_key.into(), done_val.into());
        }
        stream_manager::StreamChunk::Error(err_msg) => {
            // {error: string}
            let error_key = v8::String::new(scope, "error").unwrap();
            let error_val = v8::String::new(scope, &err_msg).unwrap();
            result_obj.set(scope, error_key.into(), error_val.into());
        }
    }
}

/// Populate result object for StorageResult callback.
///
/// Sets: { success, body?, streamId?, size?, etag?, keys?, truncated?, error?, status?, headers? }
pub fn populate_storage_result(
    scope: &mut v8::PinScope,
    result_obj: v8::Local<v8::Object>,
    result: StorageResult,
    stream_manager: &std::sync::Arc<super::stream_manager::StreamManager>,
) {
    match result {
        StorageResult::Body(maybe_body) => {
            let success_key = v8::String::new(scope, "success").unwrap();
            result_obj.set(
                scope,
                success_key.into(),
                v8::Boolean::new(scope, true).into(),
            );

            if let Some(body) = maybe_body {
                let len = body.len();
                let array_buffer = crate::v8_helpers::create_array_buffer_from_vec(scope, body);
                let uint8_array = v8::Uint8Array::new(scope, array_buffer, 0, len).unwrap();
                let body_key = v8::String::new(scope, "body").unwrap();
                result_obj.set(scope, body_key.into(), uint8_array.into());
            }
        }
        StorageResult::Head { size, etag } => {
            let success_key = v8::String::new(scope, "success").unwrap();
            result_obj.set(
                scope,
                success_key.into(),
                v8::Boolean::new(scope, true).into(),
            );

            let size_key = v8::String::new(scope, "size").unwrap();
            let size_val = v8::Number::new(scope, size as f64);
            result_obj.set(scope, size_key.into(), size_val.into());

            if let Some(etag_str) = etag {
                let etag_key = v8::String::new(scope, "etag").unwrap();
                let etag_val = v8::String::new(scope, &etag_str).unwrap();
                result_obj.set(scope, etag_key.into(), etag_val.into());
            }
        }
        StorageResult::List { keys, truncated } => {
            let success_key = v8::String::new(scope, "success").unwrap();
            result_obj.set(
                scope,
                success_key.into(),
                v8::Boolean::new(scope, true).into(),
            );

            let arr = v8::Array::new(scope, keys.len() as i32);

            for (i, key) in keys.iter().enumerate() {
                let key_val = v8::String::new(scope, key).unwrap();
                arr.set_index(scope, i as u32, key_val.into());
            }

            let keys_key = v8::String::new(scope, "keys").unwrap();
            result_obj.set(scope, keys_key.into(), arr.into());

            let truncated_key = v8::String::new(scope, "truncated").unwrap();
            result_obj.set(
                scope,
                truncated_key.into(),
                v8::Boolean::new(scope, truncated).into(),
            );
        }
        StorageResult::Error(err_msg) => {
            let success_key = v8::String::new(scope, "success").unwrap();
            result_obj.set(
                scope,
                success_key.into(),
                v8::Boolean::new(scope, false).into(),
            );

            let error_key = v8::String::new(scope, "error").unwrap();
            let error_val = v8::String::new(scope, &err_msg).unwrap();
            result_obj.set(scope, error_key.into(), error_val.into());
        }

        StorageResult::Response(response) => {
            // success
            let success_key = v8::String::new(scope, "success").unwrap();
            result_obj.set(
                scope,
                success_key.into(),
                v8::Boolean::new(scope, true).into(),
            );

            // status
            let status_key = v8::String::new(scope, "status").unwrap();
            result_obj.set(
                scope,
                status_key.into(),
                v8::Number::new(scope, response.status as f64).into(),
            );

            // headers as array of [key, value] pairs (preserves duplicates like set-cookie)
            let headers_arr = v8::Array::new(scope, response.headers.len() as i32);
            for (i, (key, value)) in response.headers.iter().enumerate() {
                let pair = v8::Array::new(scope, 2);
                let k = v8::String::new(scope, key).unwrap();
                let v = v8::String::new(scope, value).unwrap();
                pair.set_index(scope, 0, k.into());
                pair.set_index(scope, 1, v.into());
                headers_arr.set_index(scope, i as u32, pair.into());
            }
            let headers_key = v8::String::new(scope, "headers").unwrap();
            result_obj.set(scope, headers_key.into(), headers_arr.into());

            // body - either bytes or stream
            match response.body {
                openworkers_core::ResponseBody::Bytes(bytes) => {
                    let body_bytes: Vec<u8> = bytes.into();
                    let len = body_bytes.len();
                    let array_buffer =
                        crate::v8_helpers::create_array_buffer_from_vec(scope, body_bytes);
                    let uint8_array = v8::Uint8Array::new(scope, array_buffer, 0, len).unwrap();
                    let body_key = v8::String::new(scope, "body").unwrap();
                    result_obj.set(scope, body_key.into(), uint8_array.into());
                }

                openworkers_core::ResponseBody::Stream(rx) => {
                    // Create a native stream and pump chunks into it
                    let stream_id = stream_manager.create_stream("storage_fetch".to_string());
                    let manager = stream_manager.clone();

                    tokio::spawn(async move {
                        use super::stream_manager::StreamChunk;
                        let mut rx = rx;

                        while let Some(result) = rx.recv().await {
                            match result {
                                Ok(bytes) => {
                                    if manager
                                        .write_chunk(stream_id, StreamChunk::Data(bytes))
                                        .await
                                        .is_err()
                                    {
                                        break;
                                    }
                                }
                                Err(e) => {
                                    let _ =
                                        manager.write_chunk(stream_id, StreamChunk::Error(e)).await;
                                    return;
                                }
                            }
                        }

                        let _ = manager.write_chunk(stream_id, StreamChunk::Done).await;
                    });

                    let stream_id_key = v8::String::new(scope, "streamId").unwrap();
                    result_obj.set(
                        scope,
                        stream_id_key.into(),
                        v8::Number::new(scope, stream_id as f64).into(),
                    );
                }

                openworkers_core::ResponseBody::None => {
                    // No body
                }
            }
        }
    }
}

/// Populate result object for KvResult callback.
///
/// Sets: { success, value?, keys?, error? }
pub fn populate_kv_result(
    scope: &mut v8::PinScope,
    result_obj: v8::Local<v8::Object>,
    result: KvResult,
) {
    match result {
        KvResult::Value(maybe_value) => {
            let success_key = v8::String::new(scope, "success").unwrap();
            result_obj.set(
                scope,
                success_key.into(),
                v8::Boolean::new(scope, true).into(),
            );

            let value_key = v8::String::new(scope, "value").unwrap();

            if let Some(value) = maybe_value {
                let json_str = serde_json::to_string(&value).unwrap();
                let json_v8_str = v8::String::new(scope, &json_str).unwrap();
                let parsed =
                    v8::json::parse(scope, json_v8_str).unwrap_or_else(|| v8::null(scope).into());
                result_obj.set(scope, value_key.into(), parsed);
            } else {
                result_obj.set(scope, value_key.into(), v8::null(scope).into());
            }
        }
        KvResult::Ok => {
            let success_key = v8::String::new(scope, "success").unwrap();
            result_obj.set(
                scope,
                success_key.into(),
                v8::Boolean::new(scope, true).into(),
            );
        }
        KvResult::Keys(keys) => {
            let success_key = v8::String::new(scope, "success").unwrap();
            result_obj.set(
                scope,
                success_key.into(),
                v8::Boolean::new(scope, true).into(),
            );

            let keys_array = v8::Array::new(scope, keys.len().try_into().unwrap());

            for (i, key) in keys.iter().enumerate() {
                let key_val = v8::String::new(scope, key).unwrap();
                keys_array.set_index(scope, i as u32, key_val.into());
            }

            let keys_key = v8::String::new(scope, "keys").unwrap();
            result_obj.set(scope, keys_key.into(), keys_array.into());
        }
        KvResult::Error(err_msg) => {
            let success_key = v8::String::new(scope, "success").unwrap();
            result_obj.set(
                scope,
                success_key.into(),
                v8::Boolean::new(scope, false).into(),
            );

            let error_key = v8::String::new(scope, "error").unwrap();
            let error_val = v8::String::new(scope, &err_msg).unwrap();
            result_obj.set(scope, error_key.into(), error_val.into());
        }
    }
}

/// Populate result object for DatabaseResult callback.
///
/// Sets: { success, rows?, error? }
pub fn populate_database_result(
    scope: &mut v8::PinScope,
    result_obj: v8::Local<v8::Object>,
    result: DatabaseResult,
) {
    match result {
        DatabaseResult::Rows(rows_json) => {
            let success_key = v8::String::new(scope, "success").unwrap();
            result_obj.set(
                scope,
                success_key.into(),
                v8::Boolean::new(scope, true).into(),
            );

            let rows_key = v8::String::new(scope, "rows").unwrap();
            let rows_str = v8::String::new(scope, &rows_json).unwrap();
            let parsed = v8::json::parse(scope, rows_str);

            if let Some(parsed_value) = parsed {
                result_obj.set(scope, rows_key.into(), parsed_value);
            } else {
                // Fallback: return raw string if parsing fails
                result_obj.set(scope, rows_key.into(), rows_str.into());
            }
        }
        DatabaseResult::Error(err_msg) => {
            let success_key = v8::String::new(scope, "success").unwrap();
            result_obj.set(
                scope,
                success_key.into(),
                v8::Boolean::new(scope, false).into(),
            );

            let error_key = v8::String::new(scope, "error").unwrap();
            let error_val = v8::String::new(scope, &err_msg).unwrap();
            result_obj.set(scope, error_key.into(), error_val.into());
        }
    }
}
