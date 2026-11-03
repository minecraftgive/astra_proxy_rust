use std::{env, fs, path::Path};

fn collect(root: &Path, dir: &Path, source: &mut String) {
    if !dir.is_dir() {
        return;
    }
    let mut entries: Vec<_> = fs::read_dir(dir)
        .unwrap()
        .map(|e| e.unwrap().path())
        .collect();
    entries.sort();
    for path in entries {
        if path.is_dir() {
            collect(root, &path, source);
            continue;
        }
        if !path.is_file() || path.is_symlink() {
            continue;
        }
        let name = path
            .strip_prefix(root)
            .unwrap()
            .to_string_lossy()
            .replace('\\', "/");
        let mime = match path.extension().and_then(|s| s.to_str()).unwrap_or("") {
            "html" => "text/html; charset=utf-8",
            "js" => "text/javascript; charset=utf-8",
            "css" => "text/css; charset=utf-8",
            "svg" => "image/svg+xml",
            "png" => "image/png",
            "ico" => "image/x-icon",
            "woff2" => "font/woff2",
            _ => "application/octet-stream",
        };
        source.push_str(&format!(
            "({name:?}, include_bytes!({:?}), {mime:?}),\n",
            path.canonicalize().unwrap()
        ));
    }
}

fn main() {
    let manifest = env::var("CARGO_MANIFEST_DIR").unwrap();
    let root = Path::new(&manifest).join("../../apps/web/dist");
    println!("cargo:rerun-if-changed={}", root.display());
    println!("cargo:rerun-if-env-changed=ASTRA_WEB_REQUIRE_ASSETS");
    if (env::var("PROFILE").as_deref() == Ok("release")
        || env::var_os("ASTRA_WEB_REQUIRE_ASSETS").is_some())
        && !root.join("index.html").is_file()
    {
        panic!("Production web assets are missing. Run npm ci && npm run build in apps/web before building astra-web --release.");
    }
    let mut source = String::from("pub static ASSETS: &[(&str, &[u8], &str)] = &[\n");
    collect(&root, &root, &mut source);
    source.push_str("];\n");
    fs::write(
        Path::new(&env::var("OUT_DIR").unwrap()).join("assets.rs"),
        source,
    )
    .unwrap();
}
