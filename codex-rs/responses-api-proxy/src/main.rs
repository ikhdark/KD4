use clap::Parser;
use codex_responses_api_proxy::Args as ResponsesApiProxyArgs;

#[ctor::ctor(unsafe)]
fn pre_main() {
    // SAFETY: This constructor runs before main and before this binary starts
    // threads or reads the API key. It does not load dotenv later.
    unsafe { codex_process_hardening::pre_main_hardening() };
}

pub fn main() -> anyhow::Result<()> {
    let args = ResponsesApiProxyArgs::parse();
    codex_responses_api_proxy::run_main(args)
}
