// OnceLock is process-global, so we cannot meaningfully test set/get in
// isolation across parallel test threads.  The compile-time check below is
// enough to confirm the API surface is correct.
use super::*;
use std::path::PathBuf;

#[allow(dead_code, reason = "compile-time shape check, never executed")]
fn _shape_check() {
    set_home_dir(PathBuf::from("/tmp/meow"));
    let _: Option<PathBuf> = meow_home_dir();
}
