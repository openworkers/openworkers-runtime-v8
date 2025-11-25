use super::FetchResponse;
use v8;

pub fn create_response_object<'s>(
    scope: &mut v8::PinScope<'s, '_>,
    response: FetchResponse,
) -> Result<v8::Local<'s, v8::Object>, String> {
    let obj = v8::Object::new(scope);

    // Set status
    let status_key = v8::String::new(scope, "status").unwrap();
    let status_val = v8::Number::new(scope, response.status as f64);
    obj.set(scope, status_key.into(), status_val.into());

    // Set body as Uint8Array (binary support)
    let body_key = v8::String::new(scope, "body").unwrap();

    // Create ArrayBuffer from bytes
    let array_buffer = v8::ArrayBuffer::new(scope, response.body.len());
    {
        let backing_store = array_buffer.get_backing_store();
        let data = backing_store.data().unwrap();
        unsafe {
            std::ptr::copy_nonoverlapping(
                response.body.as_ptr(),
                data.as_ptr() as *mut u8,
                response.body.len(),
            );
        }
    }

    // Create Uint8Array view
    let uint8_array = v8::Uint8Array::new(scope, array_buffer, 0, response.body.len()).unwrap();
    obj.set(scope, body_key.into(), uint8_array.into());

    // Set headers
    let headers_key = v8::String::new(scope, "headers").unwrap();
    let headers_obj = v8::Object::new(scope);
    for (key, value) in &response.headers {
        let k = v8::String::new(scope, key).unwrap();
        let v = v8::String::new(scope, value).unwrap();
        headers_obj.set(scope, k.into(), v.into());
    }
    obj.set(scope, headers_key.into(), headers_obj.into());

    // Add text() method that decodes Uint8Array to string
    let text_code = r#"
        (function(bodyBytes) {
            return async function() {
                const decoder = new TextDecoder();
                return decoder.decode(bodyBytes);
            };
        })
    "#;
    let text_code_str = v8::String::new(scope, text_code).unwrap();
    let text_script = v8::Script::compile(scope, text_code_str, None).unwrap();
    let text_factory = text_script.run(scope).unwrap();

    if text_factory.is_function() {
        let text_factory_fn: v8::Local<v8::Function> = text_factory.try_into().unwrap();
        if let Some(text_fn) = text_factory_fn.call(scope, text_factory, &[uint8_array.into()]) {
            let text_key = v8::String::new(scope, "text").unwrap();
            obj.set(scope, text_key.into(), text_fn);
        }
    }

    // Add arrayBuffer() method
    let array_buffer_code = r#"
        (function(bodyBytes) {
            return async function() {
                return bodyBytes.buffer;
            };
        })
    "#;
    let ab_code_str = v8::String::new(scope, array_buffer_code).unwrap();
    let ab_script = v8::Script::compile(scope, ab_code_str, None).unwrap();
    let ab_factory = ab_script.run(scope).unwrap();

    if ab_factory.is_function() {
        let ab_factory_fn: v8::Local<v8::Function> = ab_factory.try_into().unwrap();
        if let Some(ab_fn) = ab_factory_fn.call(scope, ab_factory, &[uint8_array.into()]) {
            let ab_key = v8::String::new(scope, "arrayBuffer").unwrap();
            obj.set(scope, ab_key.into(), ab_fn);
        }
    }

    Ok(obj)
}
