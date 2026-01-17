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

/// Store state in a V8 context slot.
///
/// Uses the type as the slot key (type-safe, no magic strings).
///
/// # Example
/// ```ignore
/// let state = MyState { ... };
/// store_state!(scope, state);
/// ```
macro_rules! store_state {
    ($scope:expr, $state:expr) => {
        $scope
            .get_current_context()
            .set_slot(std::rc::Rc::new($state))
    };
}

/// Get state from a V8 context slot.
///
/// Uses the type as the slot key (type-safe, no magic strings).
///
/// # Example
/// ```ignore
/// let Some(state) = get_state!(scope, MyState) else { return };
/// ```
macro_rules! get_state {
    ($scope:expr, $type:ty) => {
        $scope.get_current_context().get_slot::<$type>()
    };
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
