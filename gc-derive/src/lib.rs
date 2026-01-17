//! Derive macro for GcTraceable trait.
//!
//! This crate provides `#[derive(GcTraceable)]` for automatic implementation
//! of the `GcTraceable` trait from `openworkers-runtime-v8`.
//!
//! # Usage
//!
//! ```ignore
//! use openworkers_runtime_v8::GcTraceable;
//!
//! #[derive(GcTraceable)]
//! struct MyBuffer {
//!     #[gc(track)]
//!     data: Vec<u8>,
//!     #[gc(track)]
//!     metadata: String,
//!     // Fields without #[gc(track)] are not counted
//!     id: u64,
//! }
//! ```
//!
//! # Inside the crate
//!
//! When using inside `openworkers-runtime-v8` itself, use `#[gc(crate_path = "crate")]`:
//!
//! ```ignore
//! #[derive(GcTraceable)]
//! #[gc(crate_path = "crate")]
//! struct InternalBuffer {
//!     #[gc(track)]
//!     data: Vec<u8>,
//! }
//! ```

use proc_macro::TokenStream;
use quote::quote;
use syn::{Data, DeriveInput, Fields, parse_macro_input};

/// Derive macro for implementing `GcTraceable`.
///
/// Use `#[gc(track)]` on fields that should be included in memory tracking.
/// Fields without this attribute are ignored.
///
/// # Attributes
///
/// - `#[gc(track)]` - Mark a field to be tracked
/// - `#[gc(crate_path = "path")]` - Override the crate path (default: `openworkers_runtime_v8`)
///
/// # Example
///
/// ```ignore
/// #[derive(GcTraceable)]
/// struct StreamState {
///     #[gc(track)]
///     buffer: Vec<u8>,       // tracked
///     #[gc(track)]
///     pending: Vec<Bytes>,   // tracked
///     callback_id: u64,      // NOT tracked
/// }
/// ```
#[proc_macro_derive(GcTraceable, attributes(gc))]
pub fn derive_gc_traceable(input: TokenStream) -> TokenStream {
    let input = parse_macro_input!(input as DeriveInput);
    let name = &input.ident;
    let generics = &input.generics;
    let (impl_generics, ty_generics, where_clause) = generics.split_for_impl();

    // Check for #[gc(crate_path = "...")] on the struct
    let crate_path = get_crate_path(&input);

    let body = match &input.data {
        Data::Struct(data) => generate_struct_body(&data.fields, &crate_path),
        Data::Enum(_) => {
            return syn::Error::new_spanned(&input, "GcTraceable cannot be derived for enums")
                .to_compile_error()
                .into();
        }
        Data::Union(_) => {
            return syn::Error::new_spanned(&input, "GcTraceable cannot be derived for unions")
                .to_compile_error()
                .into();
        }
    };

    let expanded = quote! {
        impl #impl_generics #crate_path::GcTraceable for #name #ty_generics #where_clause {
            fn external_memory_size(&self) -> usize {
                #body
            }
        }
    };

    TokenStream::from(expanded)
}

fn get_crate_path(input: &DeriveInput) -> proc_macro2::TokenStream {
    for attr in &input.attrs {
        if !attr.path().is_ident("gc") {
            continue;
        }

        let mut crate_path = None;

        let _ = attr.parse_nested_meta(|meta| {
            if meta.path.is_ident("crate_path") {
                let value: syn::LitStr = meta.value()?.parse()?;
                let path: syn::Path = value.parse()?;
                crate_path = Some(quote! { #path });
            }
            Ok(())
        });

        if let Some(path) = crate_path {
            return path;
        }
    }

    // Default path
    quote! { openworkers_runtime_v8 }
}

fn generate_struct_body(
    fields: &Fields,
    crate_path: &proc_macro2::TokenStream,
) -> proc_macro2::TokenStream {
    let tracked_fields: Vec<_> = match fields {
        Fields::Named(named) => named
            .named
            .iter()
            .filter(|f| has_gc_track_attr(f))
            .map(|f| {
                let name = &f.ident;
                quote! { #crate_path::GcTraceable::external_memory_size(&self.#name) }
            })
            .collect(),
        Fields::Unnamed(unnamed) => unnamed
            .unnamed
            .iter()
            .enumerate()
            .filter(|(_, f)| has_gc_track_attr(f))
            .map(|(i, _)| {
                let index = syn::Index::from(i);
                quote! { #crate_path::GcTraceable::external_memory_size(&self.#index) }
            })
            .collect(),
        Fields::Unit => vec![],
    };

    if tracked_fields.is_empty() {
        quote! { 0 }
    } else {
        quote! {
            0 #(+ #tracked_fields)*
        }
    }
}

fn has_gc_track_attr(field: &syn::Field) -> bool {
    field.attrs.iter().any(|attr| {
        if !attr.path().is_ident("gc") {
            return false;
        }

        // Parse #[gc(track)]
        attr.parse_nested_meta(|meta| {
            if meta.path.is_ident("track") {
                Ok(())
            } else {
                Err(meta.error("expected `track`"))
            }
        })
        .is_ok()
    })
}
