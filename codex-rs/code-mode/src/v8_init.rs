use std::sync::OnceLock;

static V8_PLATFORM: OnceLock<Result<v8::SharedRef<v8::Platform>, String>> = OnceLock::new();

/// Initializes the process-wide V8 platform once; later calls reuse the result.
pub(crate) fn ensure_v8_initialized() -> Result<(), String> {
    match V8_PLATFORM.get_or_init(initialize_v8_platform) {
        Ok(_) => Ok(()),
        Err(error_text) => Err(error_text.clone()),
    }
}

fn initialize_v8_platform() -> Result<v8::SharedRef<v8::Platform>, String> {
    v8::icu::set_common_data_77(deno_core_icudata::ICU_DATA)
        .map_err(|error_code| format!("failed to initialize ICU data: {error_code}"))?;
    let platform = v8::new_default_platform(0, false).make_shared();
    v8::V8::initialize_platform(platform.clone());
    v8::V8::initialize();
    Ok(platform)
}

#[cfg(test)]
mod tests {
    use pretty_assertions::assert_eq;

    #[test]
    fn sandbox_feature_matches_linked_v8() {
        unsafe extern "C" {
            fn v8__V8__IsSandboxEnabled() -> bool;
        }

        // SAFETY: The linked rusty_v8 function takes no arguments and returns
        // V8's compile-time sandbox flag; it does not access an isolate or require initialization.
        let linked_v8_has_sandbox = unsafe { v8__V8__IsSandboxEnabled() };
        assert_eq!(linked_v8_has_sandbox, cfg!(feature = "sandbox"));
    }
}
