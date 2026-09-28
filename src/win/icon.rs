//! Draws the tray icon (a small clipboard) at runtime, so the build needs no
//! .ico asset or resource compiler.

use std::ptr;

use windows_sys::Win32::Graphics::Gdi::{
    BI_RGB, BITMAPINFO, BITMAPINFOHEADER, CreateBitmap, CreateDIBSection, DIB_RGB_COLORS, DeleteObject,
};
use windows_sys::Win32::UI::WindowsAndMessaging::{
    CreateIconIndirect, GetSystemMetrics, HICON, ICONINFO, IDI_APPLICATION, LoadIconW, SM_CXSMICON,
};

const BLUE: [u8; 3] = [0x25, 0x63, 0xEB];
const NAVY: [u8; 3] = [0x1E, 0x3A, 0x8A];
const WHITE: [u8; 3] = [0xFF, 0xFF, 0xFF];

/// Rounded rectangle in unit coordinates (0..1 of the icon size).
struct Shape {
    x0: f32,
    y0: f32,
    x1: f32,
    y1: f32,
    radius: f32,
    rgb: [u8; 3],
}

const SHAPES: [Shape; 4] = [
    // Board, then the clip on top of it, then two lines of "text".
    Shape { x0: 0.16, y0: 0.14, x1: 0.84, y1: 0.95, radius: 0.14, rgb: BLUE },
    Shape { x0: 0.33, y0: 0.04, x1: 0.67, y1: 0.25, radius: 0.07, rgb: NAVY },
    Shape { x0: 0.30, y0: 0.44, x1: 0.70, y1: 0.55, radius: 0.05, rgb: WHITE },
    Shape { x0: 0.30, y0: 0.66, x1: 0.58, y1: 0.77, radius: 0.05, rgb: WHITE },
];

pub fn create() -> HICON {
    let size = unsafe { GetSystemMetrics(SM_CXSMICON) }.clamp(16, 64) as usize;
    unsafe { to_hicon(size, &draw(size)) }.unwrap_or_else(|| unsafe { LoadIconW(ptr::null_mut(), IDI_APPLICATION) })
}

/// Straight-alpha BGRA pixels, top-down.
fn draw(size: usize) -> Vec<[u8; 4]> {
    let s = size as f32;
    let mut pixels = vec![[0u8; 4]; size * size];
    for y in 0..size {
        for x in 0..size {
            let (px, py) = (x as f32 + 0.5, y as f32 + 0.5);
            let (mut rgb, mut alpha) = ([0f32; 3], 0f32);
            for shape in &SHAPES {
                let cover = shape.coverage(px, py, s);
                if cover <= 0.0 {
                    continue;
                }
                // Porter-Duff "over" in straight alpha.
                let out = cover + alpha * (1.0 - cover);
                for (c, &src) in rgb.iter_mut().zip(&shape.rgb) {
                    *c = (f32::from(src) * cover + *c * alpha * (1.0 - cover)) / out;
                }
                alpha = out;
            }
            pixels[y * size + x] = [rgb[2] as u8, rgb[1] as u8, rgb[0] as u8, (alpha * 255.0).round() as u8];
        }
    }
    pixels
}

impl Shape {
    /// Anti-aliased coverage of the pixel centred at (px, py), from the
    /// rounded-box signed distance in pixels.
    fn coverage(&self, px: f32, py: f32, s: f32) -> f32 {
        let r = self.radius * s;
        let (cx, cy) = ((self.x0 + self.x1) * 0.5 * s, (self.y0 + self.y1) * 0.5 * s);
        let (hx, hy) = ((self.x1 - self.x0) * 0.5 * s - r, (self.y1 - self.y0) * 0.5 * s - r);
        let (qx, qy) = ((px - cx).abs() - hx, (py - cy).abs() - hy);
        let distance = qx.max(0.0).hypot(qy.max(0.0)) + qx.max(qy).min(0.0) - r;
        (0.5 - distance).clamp(0.0, 1.0)
    }
}

unsafe fn to_hicon(size: usize, pixels: &[[u8; 4]]) -> Option<HICON> {
    unsafe {
        let mut info: BITMAPINFO = std::mem::zeroed();
        info.bmiHeader = BITMAPINFOHEADER {
            biSize: size_of::<BITMAPINFOHEADER>() as u32,
            biWidth: size as i32,
            biHeight: -(size as i32), // top-down
            biPlanes: 1,
            biBitCount: 32,
            biCompression: BI_RGB,
            ..std::mem::zeroed()
        };
        let mut bits = ptr::null_mut();
        let color = CreateDIBSection(ptr::null_mut(), &info, DIB_RGB_COLORS, &mut bits, ptr::null_mut(), 0);
        if color.is_null() || bits.is_null() {
            return None;
        }
        ptr::copy_nonoverlapping(pixels.as_ptr().cast::<u8>(), bits.cast::<u8>(), pixels.len() * 4);

        // All-zero AND mask: transparency comes from the alpha channel.
        let mask_bits = vec![0u8; size.div_ceil(16) * 2 * size];
        let mask = CreateBitmap(size as i32, size as i32, 1, 1, mask_bits.as_ptr().cast());
        let icon = CreateIconIndirect(&ICONINFO { fIcon: 1, xHotspot: 0, yHotspot: 0, hbmMask: mask, hbmColor: color });
        DeleteObject(color);
        DeleteObject(mask);
        (!icon.is_null()).then_some(icon)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn icon_has_opaque_body_and_transparent_corners() {
        let size = 32;
        let pixels = draw(size);
        assert_eq!(pixels[0][3], 0, "top-left corner is transparent");
        let centre = pixels[(size / 2) * size + size / 2];
        assert_eq!(centre[3], 255, "centre is opaque");
    }
}
