fn main() {
    slint_build::compile("ui/usage-window.slint")
        .expect("failed to compile the Slint usage prototype");
}
