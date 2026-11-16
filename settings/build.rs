fn main() {
    // The settings window has an icon of its own, so Explorer does not show the
    // generic executable glyph next to the one visible surface of the app.
    println!("cargo:rerun-if-changed=../icons/app.ico");

    #[cfg(windows)]
    {
        let mut res = winresource::WindowsResource::new();
        res.set_icon("../icons/app.ico");
        res.set("ProductName", "MyWare Voice");
        res.set(
            "FileDescription",
            "MyWare Voice \u{2014} settings and history",
        );
        res.set("CompanyName", "Sean Wu");
        res.set("LegalCopyright", "Copyright (C) 2026 Sean Wu");
        res.set("OriginalFilename", "myware-voice-settings.exe");
        if let Err(error) = res.compile() {
            println!("cargo:warning=winresource failed to embed the exe icon: {error}");
        }
    }
}
