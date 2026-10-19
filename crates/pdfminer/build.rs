//! Embeds the cmap data of the pinned pdfminer.six (`upstream/pdfminer.six/pdfminer/cmap/*.json.gz`)
//! as a table of `include_bytes!`, so `CMapDB` finds exactly the files Python finds. The table is
//! built from the directory listing: an upstream update that adds or changes cmaps needs no code
//! change.

use std::env;
use std::fs;
use std::path::PathBuf;

fn main() {
    let manifest = PathBuf::from(env::var("CARGO_MANIFEST_DIR").unwrap());
    let dir = manifest.join("../../upstream/pdfminer.six/pdfminer/cmap");
    let dir = dir
        .canonicalize()
        .unwrap_or_else(|_| panic!("{} is missing: run `git submodule update --init`", dir.display()));
    println!("cargo:rerun-if-changed={}", dir.display());

    let mut names: Vec<String> = fs::read_dir(&dir)
        .unwrap()
        .map(|e| e.unwrap().file_name().into_string().expect("cmap file names are UTF-8"))
        .filter(|n| n.ends_with(".json.gz"))
        .collect();
    // Sorted by file name, for a binary search.
    names.sort();

    let mut out = String::new();
    out.push_str("/// The files of `pdfminer/cmap/` (`<name>.json.gz`), sorted by file name.\n");
    out.push_str("pub static CMAP_FILES: &[(&str, &[u8])] = &[\n");
    for n in &names {
        let path = dir.join(n);
        println!("cargo:rerun-if-changed={}", path.display());
        out.push_str(&format!("    ({:?}, include_bytes!({:?})),\n", n, path.display().to_string()));
    }
    out.push_str("];\n");
    let dest = PathBuf::from(env::var("OUT_DIR").unwrap()).join("cmap_files.rs");
    fs::write(dest, out).unwrap();
}
