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
    v8::icu::set_common_data_78(deno_core_icudata::ICU_DATA)
        .map_err(|error_code| format!("failed to initialize ICU data: {error_code}"))?;
    // TurboFan and the Maglev/Turbolev frontend can inline Array.prototype.sort
    // with incompatible element kinds and store objects in an integer-elements
    // array. Keep those paths off until the linked V8
    // includes https://github.com/v8/v8/commit/e0562d87ad9c17042b581582c99237d798572e67.
    v8::V8::set_flags_from_string("--no-maglev --no-turbolev --no-turbo-inline-array-builtins");
    let platform = v8::new_default_platform(0, false).make_shared();
    v8::V8::initialize_platform(platform.clone());
    v8::V8::initialize();
    Ok(platform)
}
