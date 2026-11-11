fn main() {
    slint_build::compile("ui/main.slint").expect("Slint UI failed to compile");
    #[cfg(windows)]
    {
        winres::WindowsResource::new()
            .set_icon("../../Assets/favicon.ico")
            .compile()
            .expect("Could not embed the application icon");
    }
}
