//! Only to make `KEYER_BUILD_ID` (the commit CI builds from, reported in `HELLO`)
//! part of what cargo rebuilds for: without this, a cached build keeps the build id
//! it was compiled with and the box would name a firmware it is not running.
fn main() {
    println!("cargo:rerun-if-env-changed=KEYER_BUILD_ID");
}
