//! Common macros for V8 bindings.
//!
//! These macros help reduce boilerplate when creating native JavaScript bindings.

/// Register a native function on the global scope.
///
/// # Example
/// ```ignore
/// let my_fn = v8::Function::new(scope, callback).unwrap();
/// register_fn!(scope, "myFunction", my_fn);
/// ```
macro_rules! register_fn {
    ($scope:expr, $name:literal, $func:expr) => {{
        let global = $scope.get_current_context().global($scope);
        let key = v8::String::new($scope, $name).unwrap();
        global.set($scope, key.into(), $func.into());
    }};
}

/// Store state in the global scope as a V8 External.
///
/// # Example
/// ```ignore
/// let state = MyState { ... };
/// store_state!(scope, "__myState", state);
/// ```
macro_rules! store_state {
    ($scope:expr, $name:literal, $state:expr) => {{
        let state_ptr = Box::into_raw(Box::new($state)) as *mut std::ffi::c_void;
        let external = v8::External::new($scope, state_ptr);
        let global = $scope.get_current_context().global($scope);
        let state_key = v8::String::new($scope, $name).unwrap();
        global.set($scope, state_key.into(), external.into());
    }};
}

/// Get state from the global scope.
///
/// Returns `Option<&'s T>` where T is the state type.
///
/// # Example
/// ```ignore
/// let state: Option<&MyState> = get_state!(scope, "__myState", MyState);
/// ```
macro_rules! get_state {
    ($scope:expr, $name:literal, $type:ty) => {{
        (|| -> Option<&$type> {
            let global = $scope.get_current_context().global($scope);
            let state_key = v8::String::new($scope, $name)?;
            let state_val = global.get($scope, state_key.into())?;

            if !state_val.is_external() {
                return None;
            }

            let external: v8::Local<v8::External> = state_val.try_into().ok()?;
            let state_ptr = external.value() as *const $type;
            Some(unsafe { &*state_ptr })
        })()
    }};
}

/// Execute JavaScript code in the current scope.
///
/// # Example
/// ```ignore
/// exec_js!(scope, r#"globalThis.foo = 42;"#);
/// ```
macro_rules! exec_js {
    ($scope:expr, $code:expr) => {{
        let code_str = v8::String::new($scope, $code).unwrap();
        let script = v8::Script::compile($scope, code_str, None).unwrap();
        script.run($scope)
    }};
}
