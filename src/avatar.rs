//! Minimal PNG generation for default avatars.

/// A `px` x `px` image filled with one colour.
pub fn solid_png(px: u32, rgb: [u8; 3]) -> anyhow::Result<Vec<u8>> {
    let mut out = Vec::new();
    let mut enc = png::Encoder::new(&mut out, px, px);
    enc.set_color(png::ColorType::Rgb);
    enc.set_depth(png::BitDepth::Eight);
    let mut w = enc.write_header()?;
    let row: Vec<u8> = (0..px).flat_map(|_| rgb).collect();
    let data: Vec<u8> = (0..px).flat_map(|_| row.iter().copied()).collect();
    w.write_image_data(&data)?;
    w.finish()?;
    Ok(out)
}

#[cfg(test)]
mod tests {
    #[test]
    fn produces_png() {
        let p = super::solid_png(32, [10, 20, 30]).unwrap();
        assert_eq!(&p[..8], b"\x89PNG\r\n\x1a\n");
    }
}
