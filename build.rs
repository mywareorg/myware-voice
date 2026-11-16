fn main() {
    // Cargo does not track the icon as an input unless it is declared, so a
    // replaced .ico would otherwise leave the old icon in a cached binary.
    println!("cargo:rerun-if-changed=icons/app.ico");

    // Embed the .ico into the Windows executables: Explorer, a pinned
    // shortcut, and the taskbar before the window icon loads. No-op elsewhere,
    // which also keeps a cross-check from Linux from failing on a resource
    // compiler this host does not have.
    #[cfg(windows)]
    {
        let mut res = winresource::WindowsResource::new();
        res.set_icon("icons/app.ico");
        res.set("ProductName", "MyWare Voice");
        res.set(
            "FileDescription",
            "MyWare Voice \u{2014} push-to-talk dictation",
        );
        res.set("CompanyName", "Sean Wu");
        res.set("LegalCopyright", "Copyright (C) 2026 Sean Wu");
        // No OriginalFilename: the daemon and the console binary share this
        // script, and one of them would be described by the other's name.
        if let Err(error) = res.compile() {
            println!("cargo:warning=winresource failed to embed the exe icon: {error}");
        }
    }
}
