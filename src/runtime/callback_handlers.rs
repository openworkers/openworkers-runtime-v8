//! Callback result builders for V8.
//!
//! This module contains functions that populate V8 objects for callback results.
//! Used by both `Runtime` and `ExecutionContext` to avoid code duplication.
//!
//! These functions use a "populate" pattern: the caller creates the object and
//! passes it to be populated. This avoids lifetime issues with returning `Local<T>`.

use super::stream_manager;
use openworkers_core::{
    D1Error, D1ErrorKind, D1Result, D1StatementResult, DatabaseResult, HttpResponseMeta, KvResult,
    SqlPrimitive, StorageResult,
};

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
        DatabaseResult::Error(err_msg) => set_database_error(scope, result_obj, &err_msg),
        DatabaseResult::Table { columns, rows } => {
            let success_key = v8::String::new(scope, "success").unwrap();
            result_obj.set(
                scope,
                success_key.into(),
                v8::Boolean::new(scope, true).into(),
            );

            let rows_array = v8::Array::new(scope, rows.len().try_into().unwrap());

            for (index, row) in rows.iter().enumerate() {
                let row_obj = v8::Object::new(scope);

                for (column, value) in columns.iter().zip(row) {
                    let key = v8::String::new(scope, column).unwrap();
                    let value = sql_primitive_to_v8(scope, value);

                    row_obj.set(scope, key.into(), value);
                }

                rows_array.set_index(scope, index as u32, row_obj.into());
            }

            let rows_key = v8::String::new(scope, "rows").unwrap();
            result_obj.set(scope, rows_key.into(), rows_array.into());
        }
    }
}

/// Populate result object for D1Result callback.
///
/// Sets `{ columns, rows, meta }` for a query, `{ results }` for a batch and
/// `{ count, duration }` for an exec. Rows are value arrays; `js/env.js` builds
/// the row objects, so `raw()` keeps duplicate column names.
pub fn populate_d1_result(
    scope: &mut v8::PinScope,
    result_obj: v8::Local<v8::Object>,
    result: D1Result,
) {
    match result {
        D1Result::Query(statement) => populate_d1_statement(scope, result_obj, &statement),
        D1Result::Batch { results } => {
            let array = v8::Array::new(scope, results.len().try_into().unwrap());

            for (index, statement) in results.iter().enumerate() {
                let statement_obj = v8::Object::new(scope);

                populate_d1_statement(scope, statement_obj, statement);
                array.set_index(scope, index as u32, statement_obj.into());
            }

            set_value(scope, result_obj, "results", array.into());
        }
        D1Result::Exec { count, duration } => {
            let count = v8::Number::new(scope, f64::from(count));
            let duration = v8::Number::new(scope, duration);

            set_value(scope, result_obj, "count", count.into());
            set_value(scope, result_obj, "duration", duration.into());
        }
        D1Result::Error(err) => {
            let message = v8::String::new(scope, &d1_error_message(&err)).unwrap();

            set_value(scope, result_obj, "error", message.into());
        }
    }
}

/// The guest error message: the D1 prefix of the kind, then the message.
pub fn d1_error_message(err: &D1Error) -> String {
    let prefix = match err.kind {
        D1ErrorKind::Type => "D1_TYPE_ERROR",
        D1ErrorKind::Range => "D1_RANGE_ERROR",
        D1ErrorKind::Sql
        | D1ErrorKind::Interrupted
        | D1ErrorKind::Overloaded
        | D1ErrorKind::Unavailable => "D1_ERROR",
    };

    format!("{prefix}: {}", err.message)
}

fn populate_d1_statement(
    scope: &mut v8::PinScope,
    statement_obj: v8::Local<v8::Object>,
    statement: &D1StatementResult,
) {
    let columns = v8::Array::new(scope, statement.columns.len().try_into().unwrap());

    for (index, column) in statement.columns.iter().enumerate() {
        let column = v8::String::new(scope, column).unwrap();

        columns.set_index(scope, index as u32, column.into());
    }

    let rows = v8::Array::new(scope, statement.rows.len().try_into().unwrap());

    for (row_index, row) in statement.rows.iter().enumerate() {
        let values = v8::Array::new(scope, row.len().try_into().unwrap());

        for (index, value) in row.iter().enumerate() {
            let value = sql_primitive_to_v8(scope, value);

            values.set_index(scope, index as u32, value);
        }

        rows.set_index(scope, row_index as u32, values.into());
    }

    let meta = v8::Object::new(scope);
    let duration = v8::Number::new(scope, statement.meta.duration);
    let changes = v8::Number::new(scope, statement.meta.changes as f64);
    let last_row_id = v8::Number::new(scope, statement.meta.last_row_id as f64);
    let changed_db = v8::Boolean::new(scope, statement.meta.changed_db);
    let size_after = v8::Number::new(scope, statement.meta.size_after as f64);

    set_value(scope, meta, "duration", duration.into());
    set_value(scope, meta, "changes", changes.into());
    set_value(scope, meta, "last_row_id", last_row_id.into());
    set_value(scope, meta, "changed_db", changed_db.into());
    set_value(scope, meta, "size_after", size_after.into());

    set_value(scope, statement_obj, "columns", columns.into());
    set_value(scope, statement_obj, "rows", rows.into());
    set_value(scope, statement_obj, "meta", meta.into());
}

fn set_value(
    scope: &mut v8::PinScope,
    object: v8::Local<v8::Object>,
    key: &str,
    value: v8::Local<v8::Value>,
) {
    let key = v8::String::new(scope, key).unwrap();

    object.set(scope, key.into(), value);
}

/// One SQL value as the guest sees it.
///
/// Both integer kinds become a JavaScript number, like the JSON the `Rows`
/// shape parses to, and a blob becomes an array of byte values, which is what
/// D1 hands a worker for a BLOB column.
fn sql_primitive_to_v8<'s>(
    scope: &mut v8::PinScope<'s, '_>,
    value: &SqlPrimitive,
) -> v8::Local<'s, v8::Value> {
    match value {
        SqlPrimitive::Null => v8::null(scope).into(),
        SqlPrimitive::Bool(value) => v8::Boolean::new(scope, *value).into(),
        SqlPrimitive::Int(value) => v8::Number::new(scope, *value as f64).into(),
        SqlPrimitive::Float(value) => v8::Number::new(scope, *value).into(),
        SqlPrimitive::String(value) => v8::String::new(scope, value).unwrap().into(),
        SqlPrimitive::Bytes(value) => {
            let array = v8::Array::new(scope, value.len().try_into().unwrap());

            for (index, byte) in value.iter().enumerate() {
                let byte_val = v8::Integer::new_from_unsigned(scope, u32::from(*byte));

                array.set_index(scope, index as u32, byte_val.into());
            }

            array.into()
        }
    }
}

/// Set { success: false, error } on a database result object.
fn set_database_error(scope: &mut v8::PinScope, result_obj: v8::Local<v8::Object>, err_msg: &str) {
    let success_key = v8::String::new(scope, "success").unwrap();
    result_obj.set(
        scope,
        success_key.into(),
        v8::Boolean::new(scope, false).into(),
    );

    let error_key = v8::String::new(scope, "error").unwrap();
    let error_val = v8::String::new(scope, err_msg).unwrap();
    result_obj.set(scope, error_key.into(), error_val.into());
}
