#[path = "../build_info.rs"]
mod build_info;

fn main() {
    build_info::emit();
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("macos") {
        println!("cargo:rustc-link-arg=-ObjC");
    }
}
