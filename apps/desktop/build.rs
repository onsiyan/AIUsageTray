//! Gives the Windows executable its icon and file details.

fn main() {
    println!("cargo:rerun-if-changed=assets/icon/app.ico");
    #[cfg(windows)]
    {
        let mut resource = winresource::WindowsResource::new();
        resource
            .set_icon("assets/icon/app.ico")
            .set("ProductName", "AI Usage Tray")
            .set("FileDescription", "AI Usage Tray");
        if let Err(error) = resource.compile() {
            panic!("could not embed the Windows resources: {error}");
        }
    }
}
