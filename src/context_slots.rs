//! Per-context binding state, keyed by type.
//!
//! Replaces `v8::Context::set_slot`, which cannot be used on a pooled isolate:
//! it hangs a `v8::Weak` finalizer off the context, and V8 rejects weak handles
//! on shared isolates. The map lives in Rust and is owned by whoever owns the
//! context; the context only carries its address in a private property of its
//! global object.

use std::any::Any;
use std::any::TypeId;
use std::cell::RefCell;
use std::collections::HashMap;
use std::ffi::c_void;
use std::rc::Rc;

/// Qualified per V8's advice on `Private::for_api`: one global name space.
const KEY_NAME: &str = "OpenWorkers#contextSlots";

#[derive(Default)]
pub struct ContextSlots {
    map: RefCell<HashMap<TypeId, Rc<dyn Any>>>,
}

/// Bind `slots` to the current context. Call once, before any binding setup.
///
/// The caller must keep the `Rc` alive for as long as the context can run JS.
pub fn attach(scope: &mut v8::PinScope, slots: &Rc<ContextSlots>) {
    let ptr = Rc::as_ptr(slots) as *mut c_void;
    let external = v8::External::new(scope, ptr);
    let key = key(scope);
    let global = scope.get_current_context().global(scope);
    global.set_private(scope, key, external.into());
}

/// Store `value` under its own type. Replaces any previous value of that type.
pub fn set<T: 'static>(scope: &mut v8::PinScope, value: Rc<T>) {
    let Some(slots) = slots(scope) else {
        // Every context we run bindings in is attached first, so this only
        // fires if a setup function is called on a foreign context.
        tracing::error!("context slots missing, state not stored");
        return;
    };

    slots.map.borrow_mut().insert(TypeId::of::<T>(), value);
}

/// Fetch the value stored for `T`, if any.
pub fn get<T: 'static>(scope: &mut v8::PinScope) -> Option<Rc<T>> {
    let slots = slots(scope)?;
    let entry = slots.map.borrow().get(&TypeId::of::<T>()).cloned()?;
    entry.downcast::<T>().ok()
}

fn key<'s>(scope: &mut v8::PinScope<'s, '_>) -> v8::Local<'s, v8::Private> {
    let name = v8::String::new(scope, KEY_NAME).unwrap();
    v8::Private::for_api(scope, Some(name))
}

fn slots<'s>(scope: &mut v8::PinScope<'s, '_>) -> Option<&'s ContextSlots> {
    let key = key(scope);
    let global = scope.get_current_context().global(scope);
    let external: v8::Local<v8::External> = global.get_private(scope, key)?.try_into().ok()?;

    // SAFETY: `attach` writes the address of a live `Rc<ContextSlots>`, and its
    // owner outlives every JS turn in this context.
    Some(unsafe { &*(external.value() as *const ContextSlots) })
}
