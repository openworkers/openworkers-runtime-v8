use super::FetchResponse;
use rusty_v8 as v8;

pub fn create_response_object<'s>(
    scope: &mut v8::HandleScope<'s>,
    response: FetchResponse,
) -> Result<v8::Local<'s, v8::Object>, String> {
    let obj = v8::Object::new(scope);

    // Set status
    let status_key = v8::String::new(scope, "status").unwrap();
    let status_val = v8::Number::new(scope, response.status as f64);
    obj.set(scope, status_key.into(), status_val.into());

    // Set body
    let body_key = v8::String::new(scope, "body").unwrap();
    let body_val = v8::String::new(scope, &response.body).unwrap();
    obj.set(scope, body_key.into(), body_val.into());

    // Set headers
    let headers_key = v8::String::new(scope, "headers").unwrap();
    let headers_obj = v8::Object::new(scope);
    for (key, value) in &response.headers {
        let k = v8::String::new(scope, key).unwrap();
        let v = v8::String::new(scope, value).unwrap();
        headers_obj.set(scope, k.into(), v.into());
    }
    obj.set(scope, headers_key.into(), headers_obj.into());

    // Add text() method
    let text_code = r#"
        (function(body) {
            return async function() { return body; };
        })
    "#;
    let text_code_str = v8::String::new(scope, text_code).unwrap();
    let text_script = v8::Script::compile(scope, text_code_str, None).unwrap();
    let text_factory = text_script.run(scope).unwrap();

    if text_factory.is_function() {
        let text_factory_fn = unsafe { v8::Local::<v8::Function>::cast(text_factory) };
        let body_for_text = v8::String::new(scope, &response.body).unwrap();
        if let Some(text_fn) = text_factory_fn.call(scope, text_factory, &[body_for_text.into()]) {
            let text_key = v8::String::new(scope, "text").unwrap();
            obj.set(scope, text_key.into(), text_fn);
        }
    }

    Ok(obj)
}
