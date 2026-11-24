use super::*;
#[test]
fn png_completion_requires_decodable_canonical_image_bytes() {
    let mut bytes = Vec::new();
    {
        let mut encoder = png::Encoder::new(&mut bytes, 1, 1);
        encoder.set_color(png::ColorType::Rgba);
        encoder.set_depth(png::BitDepth::Eight);
        let mut writer = encoder.write_header().unwrap();
        writer.write_image_data(&[10, 20, 30, 255]).unwrap();
    }
    let encoded = base64(&bytes);
    assert_eq!(decode_png(&encoded).unwrap(), bytes);
    assert!(decode_png(&(encoded + "=")).is_err());
    let index = bytes.len() - 1;
    bytes[index] ^= 1;
    assert!(decode_png(&base64(&bytes)).is_err());
    assert!(decode_png("bm90LXBuZw==").is_err());
}
