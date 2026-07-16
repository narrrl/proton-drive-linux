use std::env;
use std::process::Command;

fn main() {
    let out_dir = env::var("OUT_DIR").unwrap();
    let status = Command::new("glib-compile-resources")
        .args([
            "--target",
            &format!("{}/pdfs.gresource", out_dir),
            "--sourcedir",
            "resources/icons",
            "resources/pdfs.gresource.xml",
        ])
        .status()
        .expect("Failed to execute glib-compile-resources");

    if !status.success() {
        panic!(
            "glib-compile-resources failed with exit code: {:?}",
            status.code()
        );
    }

    println!("cargo:rerun-if-changed=resources/pdfs.gresource.xml");
    println!(
        "cargo:rerun-if-changed=resources/icons/scalable/actions/pdfs-folder-upload-symbolic.svg"
    );
}
