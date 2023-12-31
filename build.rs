use std::env;
use std::fs::File;
use std::io::Write;
use std::path::PathBuf;

fn main() {
    let out = &PathBuf::from(env::var_os("OUT_DIR").unwrap());

    // By default cortex-m-rt expects memory.x, however this causes issues with workspaces as it
    // will pick the first file that is found.
    // In order to get around this we make a dummy memory.x file
    File::create(out.join("memory.x")).unwrap();

    // Use memory.x.in as a template for the actual memory.x
    let memory = include_str!("memory.x.in")
        .replace("{BOOTLOADER}", "BOOTLOADER")
        .replace("{ACTIVE}", "FLASH");

    // And then include it with a unique name
    File::create(out.join("memory_app.x"))
        .unwrap()
        .write_all(memory.as_bytes())
        .unwrap();
    println!("cargo:rustc-link-search={}", out.display());
    println!("cargo:rerun-if-changed=memory.x.in");

    // And link with that one
    println!("cargo:rustc-link-arg-bins=-Tmemory_app.x");

    println!("cargo:rustc-link-arg-bins=--nmagic");
    println!("cargo:rustc-link-arg-bins=-Tlink.x");
    println!("cargo:rustc-link-arg-bins=-Tdefmt.x");

    if let Ok(dotenv_path) = dotenvy::dotenv() {
        println!("cargo:rerun-if-changed={}", dotenv_path.display());

        for env_var in dotenvy::dotenv_iter().unwrap() {
            let (key, value) = env_var.unwrap();
            println!("cargo:rustc-env={key}={value}");
        }
    }
}
