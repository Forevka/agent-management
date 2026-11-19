//! The web UI (`apps/web`) is embedded into this crate, which means the folder has to exist
//! when the crate compiles. It does not after a fresh clone, and `cargo build` alone must still
//! work — a Rust change should not need node installed.
//!
//! So: if the built UI is missing, leave a placeholder page in its place that says how to build
//! it. Docker and CI build the real one first (see the Dockerfile's `web` stage), and a local
//! `npm run build` in `apps/web` overwrites this.

use std::{fs, path::PathBuf};

const PLACEHOLDER: &str = r#"<!doctype html>
<html lang="en"><head><meta charset="utf-8"><title>Fleet</title>
<style>body{font:15px/1.6 system-ui,sans-serif;margin:3rem auto;max-width:34rem;padding:0 1rem}
code{background:#eee;padding:.1rem .3rem;border-radius:3px}</style></head>
<body><h1>The UI was not built</h1>
<p>This binary was compiled without the web app. Build it and compile again:</p>
<pre><code>cd apps/web &amp;&amp; npm ci &amp;&amp; npm run build</code></pre>
<p>The API is unaffected and still answers under <code>/api</code>.</p>
</body></html>
"#;

fn main() {
    let dist = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../apps/web/dist");
    let index = dist.join("index.html");
    if !index.exists() {
        // A failure here would be confusing noise during a build that is otherwise fine, so it
        // is reported as a warning and the compile carries on.
        if let Err(e) = fs::create_dir_all(&dist).and_then(|()| fs::write(&index, PLACEHOLDER)) {
            println!("cargo:warning=could not write the UI placeholder: {e}");
        }
    }
    // Recompile when the UI is rebuilt, so a `npm run build` is picked up by the next
    // `cargo build` instead of embedding a stale copy.
    println!("cargo:rerun-if-changed={}", dist.display());
}
