use super::FetchResponse;
use v8;

pub fn create_response_object<'s>(
    scope: &mut v8::PinScope<'s, '_>,
    response: FetchResponse,
) -> Result<v8::Local<'s, v8::Object>, String> {
    // Create Response by calling the global Response constructor with streaming chunks
    let global = scope.get_current_context().global(scope);
    let response_ctor_key = v8::String::new(scope, "Response").unwrap();
    let response_ctor = global
        .get(scope, response_ctor_key.into())
        .and_then(|v| v8::Local::<v8::Function>::try_from(v).ok())
        .ok_or("Response constructor not found")?;

    // Create a ReadableStream with chunks
    let stream_code = if response.chunks.is_empty() {
        // Empty body
        format!(
            r#"
            new ReadableStream({{
                start(controller) {{
                    controller.close();
                }}
            }})
        "#
        )
    } else if response.chunks.len() == 1 {
        // Single chunk - inline the bytes
        let bytes_array = response.chunks[0]
            .iter()
            .map(|b| b.to_string())
            .collect::<Vec<_>>()
            .join(",");
        format!(
            r#"
            new ReadableStream({{
                start(controller) {{
                    controller.enqueue(new Uint8Array([{}]));
                    controller.close();
                }}
            }})
        "#,
            bytes_array
        )
    } else {
        // Multiple chunks - we'll create them in JS
        let chunks_code = response
            .chunks
            .iter()
            .map(|chunk| {
                let bytes_array = chunk
                    .iter()
                    .map(|b| b.to_string())
                    .collect::<Vec<_>>()
                    .join(",");
                format!("new Uint8Array([{}])", bytes_array)
            })
            .collect::<Vec<_>>()
            .join(",");

        format!(
            r#"
            new ReadableStream({{
                start(controller) {{
                    const chunks = [{}];
                    for (const chunk of chunks) {{
                        controller.enqueue(chunk);
                    }}
                    controller.close();
                }}
            }})
        "#,
            chunks_code
        )
    };

    let stream_code_str = v8::String::new(scope, &stream_code).unwrap();
    let stream_script = v8::Script::compile(scope, stream_code_str, None).unwrap();
    let stream_val = stream_script.run(scope).unwrap();

    // Create init object with status and headers
    let init_obj = v8::Object::new(scope);

    // Set status
    let status_key = v8::String::new(scope, "status").unwrap();
    let status_val = v8::Number::new(scope, response.status as f64);
    init_obj.set(scope, status_key.into(), status_val.into());

    // Set headers
    let headers_key = v8::String::new(scope, "headers").unwrap();
    let headers_obj = v8::Object::new(scope);
    for (key, value) in &response.headers {
        let k = v8::String::new(scope, key).unwrap();
        let v = v8::String::new(scope, value).unwrap();
        headers_obj.set(scope, k.into(), v.into());
    }
    init_obj.set(scope, headers_key.into(), headers_obj.into());

    // Call Response constructor: new Response(stream, init)
    let recv = v8::undefined(scope);
    let result = response_ctor
        .call(scope, recv.into(), &[stream_val, init_obj.into()])
        .ok_or("Failed to create Response")?;

    let response_obj = result.to_object(scope).ok_or("Response is not an object")?;

    Ok(response_obj)
}
