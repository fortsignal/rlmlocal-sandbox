fn main() {
  // Tauri embeds the frontend (../ui) via the generate_context! PROC-MACRO at compile time. Cargo does NOT
  // track the files a proc-macro reads, so a changed ui/index.html does NOT invalidate the build → it ships a
  // STALE embedded window (the "old pairing window keeps appearing" bug, incl. across CI's restored target/
  // cache). Watching the dir explicitly forces a re-embed when the window changes.
  // Ref: https://github.com/tauri-apps/tauri/issues/9062
  println!("cargo:rerun-if-changed=../ui");
  tauri_build::build()
}
