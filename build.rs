fn main() {
    println!("cargo:rerun-if-changed=assets/icon.ico");
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("windows") {
        winresource::WindowsResource::new()
            .set_icon("assets/icon.ico")
            .set("ProductName", "AI 工具看板")
            .set("FileDescription", "AI 工具看板")
            .set("OriginalFilename", "ai-tool-dashboard.exe")
            .compile()
            .expect("embed Windows app icon");
    }
    slint_build::compile("ui/dashboard.slint").expect("compile dashboard UI");
}
