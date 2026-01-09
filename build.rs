use std::error::Error;
use std::path::Path;

fn main() -> Result<(), Box<dyn Error>> {
    let webui_var = std::env::var("AW_WEBUI_DIR");

    let path = if let Ok(var_path) = &webui_var {
        let p = Path::new(var_path);
        if p.join("index.html").exists() {
            println!("cargo:rustc-env=AW_WEBUI_DIR={}", p.display());
            println!("cargo:rustc-cfg=feature=\"webui\"");
        } else {
            println!("cargo:warning=AW_WEBUI_DIR={} has no index.html", var_path);
            set_empty_webui()?;
        }
        p.to_path_buf()
    } else {
        // Check common locations
        let candidates = [
            "aw-webui/dist",  // submodule in this repo
            "../aw-webui/dist",
            "../aw-server-rust/aw-webui/dist",
        ];

        let mut found = None;
        for candidate in candidates {
            let p = Path::new(candidate);
            if p.join("index.html").exists() {
                println!("cargo:rustc-env=AW_WEBUI_DIR={}", p.display());
                println!("cargo:rustc-cfg=feature=\"webui\"");
                found = Some(p.to_path_buf());
                break;
            }
        }

        if found.is_none() {
            println!("cargo:warning=No webui found, compiling without webui");
            set_empty_webui()?;
        }

        found.unwrap_or_else(|| Path::new("empty-webui").to_path_buf())
    };

    println!("cargo:rerun-if-env-changed=AW_WEBUI_DIR");
    if path.exists() {
        println!("cargo:rerun-if-changed={}", path.display());
    }

    Ok(())
}

fn set_empty_webui() -> Result<(), Box<dyn Error>> {
    // Create empty dir for rust-embed to be happy
    let empty = Path::new("empty-webui");
    std::fs::create_dir_all(empty)?;
    println!("cargo:rustc-env=AW_WEBUI_DIR={}", empty.display());
    Ok(())
}
