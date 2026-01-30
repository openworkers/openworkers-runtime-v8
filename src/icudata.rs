//! Embedded ICU data for V8's Intl support.
//!
//! This module embeds the ICU data file (icudtl.dat) which is required for
//! V8's internationalization features (Intl.DateTimeFormat, Intl.NumberFormat, etc.).
//!
//! Without this data, V8 tries to load ICU data at runtime which can cause
//! memory issues and OOM crashes.
//!
//! ## Source
//!
//! The `icudtl.dat` file comes from Chromium's ICU repository:
//! <https://chromium.googlesource.com/chromium/deps/icu/+/main/common/icudtl.dat>
//!
//! SHA256: `1cf67874b5a87a8363a86fb3f81e3cbbed54d389062dab8fb52308d5cf8c8612`

/// ICU data must be 16-byte aligned per ICU requirements.
/// See: https://unicode-org.github.io/icu/userguide/icu_data/
#[repr(C, align(16))]
struct IcuData<T: ?Sized>(T);

static ICU_DATA_RAW: &IcuData<[u8]> = &IcuData(*include_bytes!("icudtl.dat"));

/// Raw ICU data, properly aligned for V8/ICU.
pub static ICU_DATA: &[u8] = &ICU_DATA_RAW.0;

#[cfg(test)]
mod tests {
    use std::pin::pin;

    use openworkers_core::RuntimeLimits;

    use crate::LockerManagedIsolate;

    #[test]
    fn test_icu_data_is_loaded() {
        assert!(
            super::ICU_DATA.len() > 1_000_000,
            "ICU data should be at least 1MB"
        );
        // ICU data header: 0x90 0x00 followed by 0xda 0x27 (little-endian magic)
        assert_eq!(
            &super::ICU_DATA[0..4],
            &[0x90, 0x00, 0xda, 0x27],
            "ICU data should have correct header"
        );
    }

    #[test]
    fn test_intl_datetimeformat_works() {
        // Use LockerManagedIsolate to be consistent with other tests
        // Mixing v8::Isolate::new() with Locker-based isolates causes race conditions
        let limits = RuntimeLimits::default();
        let mut isolate_wrapper = LockerManagedIsolate::new(limits);

        let mut locker = v8::Locker::new(&mut isolate_wrapper.isolate);
        let scope = pin!(v8::HandleScope::new(&mut *locker));
        let mut scope = scope.init();
        let context = v8::Context::new(&scope, Default::default());
        let scope = &mut v8::ContextScope::new(&mut scope, context);

        // Test Intl.DateTimeFormat with multiple timezones (this was causing OOM before)
        let code = v8::String::new(
            scope,
            r#"
            const timezones = [
                'Europe/Paris',
                'America/New_York',
                'Asia/Tokyo',
                'Australia/Sydney',
                'America/Los_Angeles'
            ];

            const results = timezones.map(tz => {
                const formatter = new Intl.DateTimeFormat('en-US', {
                    timeZone: tz,
                    hour: 'numeric',
                    minute: 'numeric',
                    second: 'numeric',
                    hour12: false
                });
                return formatter.resolvedOptions().timeZone;
            });

            JSON.stringify(results);
            "#,
        )
        .unwrap();

        let script = v8::Script::compile(scope, code, None).expect("Failed to compile script");
        let result = script.run(scope).expect("Script should not OOM");

        let result_str = result.to_string(scope).unwrap().to_rust_string_lossy(scope);

        assert!(
            result_str.contains("Europe/Paris"),
            "Should contain Paris timezone"
        );
        assert!(
            result_str.contains("America/New_York"),
            "Should contain New York timezone"
        );
    }

    #[test]
    fn test_intl_numberformat_works() {
        let limits = RuntimeLimits::default();
        let mut isolate_wrapper = LockerManagedIsolate::new(limits);

        let mut locker = v8::Locker::new(&mut isolate_wrapper.isolate);
        let scope = pin!(v8::HandleScope::new(&mut *locker));
        let mut scope = scope.init();
        let context = v8::Context::new(&scope, Default::default());
        let scope = &mut v8::ContextScope::new(&mut scope, context);

        let code = v8::String::new(
            scope,
            r#"
            const formatter = new Intl.NumberFormat('de-DE', {
                style: 'currency',
                currency: 'EUR'
            });
            formatter.format(1234.56);
            "#,
        )
        .unwrap();

        let script = v8::Script::compile(scope, code, None).expect("Failed to compile script");
        let result = script.run(scope).expect("Script should execute");

        let result_str = result.to_string(scope).unwrap().to_rust_string_lossy(scope);

        // German format: 1.234,56 €
        assert!(
            result_str.contains("1.234,56") || result_str.contains("€"),
            "Should format as German currency: {}",
            result_str
        );
    }
}
