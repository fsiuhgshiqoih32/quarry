//! The app and tray icons, drawn in code so every size is crisp.

/// Amber magnifier on a dark slate tile. RGBA, `size` x `size`.
pub fn app_icon(size: usize) -> Vec<u8> {
    draw(size, true, [246, 173, 85])
}

/// The magnifier alone in one colour, for tray icons (black for the macOS template image).
pub fn glyph(size: usize, color: [u8; 3]) -> Vec<u8> {
    draw(size, false, color)
}

fn draw(size: usize, tile: bool, fg: [u8; 3]) -> Vec<u8> {
    let s = size as f32;
    let mut out = vec![0u8; size * size * 4];
    // Without the tile the glyph fills more of the square.
    let (cx, cy, r, ring, a, b, handle) = if tile {
        (0.44, 0.43, 0.20, 0.075, (0.605, 0.595), (0.75, 0.74), 0.06)
    } else {
        (0.42, 0.42, 0.27, 0.10, (0.64, 0.64), (0.88, 0.88), 0.085)
    };
    for y in 0..size {
        for x in 0..size {
            let px = (x as f32 + 0.5) / s;
            let py = (y as f32 + 0.5) / s;
            let mut rgba = [0f32; 4];
            if tile {
                let d = rounded_rect(px, py, 0.06, 0.20) * s;
                let cov = (0.5 - d).clamp(0.0, 1.0);
                if cov > 0.0 {
                    // Slate, slightly lighter at the top.
                    let t = py;
                    let top = [58.0, 70.0, 88.0];
                    let bottom = [26.0, 32.0, 44.0];
                    for i in 0..3 {
                        rgba[i] = top[i] + (bottom[i] - top[i]) * t;
                    }
                    rgba[3] = cov;
                }
            }
            let d_ring = (((px - cx).powi(2) + (py - cy).powi(2)).sqrt() - r).abs() - ring / 2.0;
            let d_handle = segment(px, py, a, b) - handle;
            let d = d_ring.min(d_handle) * s;
            let cov = (0.5 - d).clamp(0.0, 1.0);
            if cov > 0.0 {
                let base_a = rgba[3];
                let out_a = cov + base_a * (1.0 - cov);
                for i in 0..3 {
                    rgba[i] = (fg[i] as f32 * cov + rgba[i] * base_a * (1.0 - cov)) / out_a.max(1e-6);
                }
                rgba[3] = out_a;
            }
            let o = (y * size + x) * 4;
            out[o] = rgba[0].round() as u8;
            out[o + 1] = rgba[1].round() as u8;
            out[o + 2] = rgba[2].round() as u8;
            out[o + 3] = (rgba[3] * 255.0).round() as u8;
        }
    }
    out
}

fn rounded_rect(x: f32, y: f32, inset: f32, radius: f32) -> f32 {
    let half = 0.5 - inset;
    let qx = (x - 0.5).abs() - (half - radius);
    let qy = (y - 0.5).abs() - (half - radius);
    let outside = (qx.max(0.0).powi(2) + qy.max(0.0).powi(2)).sqrt();
    outside + qx.max(qy).min(0.0) - radius
}

fn segment(x: f32, y: f32, a: (f32, f32), b: (f32, f32)) -> f32 {
    let (bax, bay) = (b.0 - a.0, b.1 - a.1);
    let (pax, pay) = (x - a.0, y - a.1);
    let h = ((pax * bax + pay * bay) / (bax * bax + bay * bay)).clamp(0.0, 1.0);
    ((pax - bax * h).powi(2) + (pay - bay * h).powi(2)).sqrt()
}

/// A PNG of the app icon, used as the drag image.
pub fn icon_png(size: usize) -> Vec<u8> {
    encode_png(&app_icon(size), size)
}

/// Minimal PNG encoder (stored deflate blocks), enough for small icons without another dependency.
pub fn encode_png(rgba: &[u8], size: usize) -> Vec<u8> {
    let mut raw = Vec::with_capacity(rgba.len() + size);
    for row in rgba.chunks(size * 4) {
        raw.push(0);
        raw.extend_from_slice(row);
    }
    let mut z = vec![0x78, 0x01];
    let mut chunks = raw.chunks(65535).peekable();
    while let Some(c) = chunks.next() {
        z.push(if chunks.peek().is_none() { 1 } else { 0 });
        z.extend_from_slice(&(c.len() as u16).to_le_bytes());
        z.extend_from_slice(&(!(c.len() as u16)).to_le_bytes());
        z.extend_from_slice(c);
    }
    z.extend_from_slice(&adler32(&raw).to_be_bytes());
    let mut png = b"\x89PNG\r\n\x1a\n".to_vec();
    let mut ihdr = Vec::new();
    ihdr.extend_from_slice(&(size as u32).to_be_bytes());
    ihdr.extend_from_slice(&(size as u32).to_be_bytes());
    ihdr.extend_from_slice(&[8, 6, 0, 0, 0]);
    chunk(&mut png, b"IHDR", &ihdr);
    chunk(&mut png, b"IDAT", &z);
    chunk(&mut png, b"IEND", &[]);
    png
}

fn chunk(out: &mut Vec<u8>, kind: &[u8; 4], data: &[u8]) {
    out.extend_from_slice(&(data.len() as u32).to_be_bytes());
    out.extend_from_slice(kind);
    out.extend_from_slice(data);
    let mut crc_input = kind.to_vec();
    crc_input.extend_from_slice(data);
    out.extend_from_slice(&crc32(&crc_input).to_be_bytes());
}

fn crc32(data: &[u8]) -> u32 {
    let mut crc = 0xFFFF_FFFFu32;
    for &b in data {
        crc ^= b as u32;
        for _ in 0..8 {
            crc = if crc & 1 != 0 { (crc >> 1) ^ 0xEDB8_8320 } else { crc >> 1 };
        }
    }
    !crc
}

fn adler32(data: &[u8]) -> u32 {
    let (mut a, mut b) = (1u32, 0u32);
    for &x in data {
        a = (a + x as u32) % 65521;
        b = (b + a) % 65521;
    }
    (b << 16) | a
}

#[cfg(test)]
mod tests {
    #[test]
    fn icons_have_content() {
        let px = super::app_icon(32);
        assert_eq!(px.len(), 32 * 32 * 4);
        assert!(px.chunks(4).any(|p| p[3] == 255));
        assert_eq!(super::icon_png(16)[..8], *b"\x89PNG\r\n\x1a\n");
    }
}
