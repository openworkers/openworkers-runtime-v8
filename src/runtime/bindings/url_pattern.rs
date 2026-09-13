//! The `URLPattern` grammar, answered by `rust-urlpattern`.
//!
//! Only the parsing crosses: a pattern comes back as its components, each with
//! a matcher and an ECMAScript regexp source, and the surface matches with the
//! engine's own `RegExp`. Nothing is held on this side between calls.

use urlpattern::quirks;
use urlpattern::quirks::StringOrInit;

/// Returns the components, or null when the pattern does not parse.
#[glue_v8::method]
fn url_pattern_parse(input: StringOrInit, base: Option<String>) -> Option<quirks::UrlPattern> {
    let init = quirks::process_construct_pattern_input(input, base.as_deref()).ok()?;

    // EcmaRegexp rather than the regex crate: what crosses is the pattern's
    // source, and the surface matches with the engine's own RegExp.
    quirks::parse_pattern::<quirks::EcmaRegexp>(init, Default::default()).ok()
}

/// Canonicalises what a pattern is matched against, or null when the input is
/// not a url any pattern could match.
#[glue_v8::method]
fn url_pattern_process_input(
    input: StringOrInit,
    base: Option<String>,
) -> Option<quirks::MatchInput> {
    let (match_input, _) = quirks::process_match_input(input, base.as_deref()).ok()??;

    quirks::parse_match_input(match_input)
}

/// Register the ops the `URLPattern` class calls.
pub fn setup_url_pattern_natives(scope: &mut v8::PinScope) {
    let parse_fn = v8::Function::new(scope, url_pattern_parse_v8).unwrap();
    super::native::register_op(scope, "urlPatternParse", parse_fn.into());

    let input_fn = v8::Function::new(scope, url_pattern_process_input_v8).unwrap();
    super::native::register_op(scope, "urlPatternProcessInput", input_fn.into());
}
