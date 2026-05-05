//! Re-encode an image as JPEG at given quality.
//! Usage: `convert_to_jpeg <input.png> <output.jpg> [quality=85]`

fn main() {
    let args: Vec<String> = std::env::args().collect();
    if args.len() < 3 {
        eprintln!("usage: convert_to_jpeg <input> <output> [quality=85]");
        std::process::exit(1);
    }
    let input = &args[1];
    let output = &args[2];
    let quality: u8 = if args.len() > 3 {
        args[3].parse().expect("quality 0-100")
    } else {
        85
    };
    let img = image::open(input).expect("open input");
    let mut out = std::fs::File::create(output).expect("create output");
    let encoder = image::codecs::jpeg::JpegEncoder::new_with_quality(&mut out, quality);
    img.write_with_encoder(encoder).expect("write jpeg");
    let len = std::fs::metadata(output).expect("stat").len();
    println!("{output}: {len} bytes (quality {quality})");
}
