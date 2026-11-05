//! Build-time guard: the management page is embedded into the binary, so a
//! missing build product must fail loudly instead of producing a blank page.

use std::path::Path;

fn main() {
    let index = Path::new("webui/dist/index.html");
    println!("cargo:rerun-if-changed=webui/dist/index.html");
    if !index.is_file() {
        panic!(
            "webui/dist/index.html is missing. Build the WebUI first \
             (cd webui && npm ci && npm run build); its dist output is embedded into the binary \
             and is not checked into git."
        );
    }
}
