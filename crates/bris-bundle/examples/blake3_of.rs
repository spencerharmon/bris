//! Compute the BLAKE3 hex digest of a file. Helper used when
//! authoring debug-bundle `bundle.json` files by hand.

fn main() {
    let path = std::env::args().nth(1).expect("path arg");
    let bytes = std::fs::read(&path).expect("read");
    println!("{}", blake3::hash(&bytes).to_hex());
}
