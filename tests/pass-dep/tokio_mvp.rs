//@compile-flags: -Zmiri-disable-isolation -Zmiri-permissive-provenance
//@only-target-x86_64-unknown-linux: support for tokio only on linux and x86

#[tokio::main]
async fn main() {}
