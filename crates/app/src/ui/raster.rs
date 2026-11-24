//! CPU rendering of the HUD pill for native overlay surfaces (the Wayland
//! `wlr-layer-shell` HUD), pixel-for-pixel the same drawing as the egui HUD:
//! the real [`hud::paint_pill`] runs in an offscreen egui context with the
//! same fonts and [`theme`] palette; its tessellated, feathered (anti-aliased)
//! meshes are filled here into a premultiplied buffer (`wl_shm` ARGB8888).

use super::{hud, theme};
use cantrip_core::surface::Hud;
use eframe::egui::{
    self, epaint::Primitive, Color32, ImageData, Pos2, RawInput, Rect, TextureId, Vec2, ViewportId,
};
use std::collections::HashMap;
use std::time::Instant;

/// One rendered pill: `width`×`height` physical pixels at integer `scale`
/// (both divisible by it), little-endian ARGB8888 premultiplied (B, G, R, A).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Pixels {
    pub width: u32,
    pub height: u32,
    pub scale: u32,
    pub argb: Vec<u8>,
}

struct Texture {
    size: [usize; 2],
    px: Vec<Color32>,
}

/// An offscreen egui context that paints the HUD pill to pixels.
pub struct PillRaster {
    ctx: egui::Context,
    textures: HashMap<TextureId, Texture>,
    start: Instant,
}

impl PillRaster {
    pub fn new(sans: Option<Vec<u8>>, mono: Option<Vec<u8>>) -> PillRaster {
        let ctx = egui::Context::default();
        theme::install_fonts(&ctx, sans, mono);
        PillRaster {
            ctx,
            textures: HashMap::new(),
            start: Instant::now(),
        }
    }

    /// Paint `hud` (with mic `level`) in palette `p` at integer `scale`.
    pub fn render(&mut self, hud: &Hud, level: f32, p: &theme::Palette, scale: u32) -> Pixels {
        let scale = scale.clamp(1, 4);
        let ppp = scale as f32;
        let mut input = RawInput {
            screen_rect: Some(Rect::from_min_size(Pos2::ZERO, Vec2::new(2048.0, 64.0))),
            time: Some(self.start.elapsed().as_secs_f64()),
            ..Default::default()
        };
        input
            .viewports
            .entry(ViewportId::ROOT)
            .or_default()
            .native_pixels_per_point = Some(ppp);
        let mut size = Vec2::ZERO;
        let mut out = self.ctx.run_ui(input, |ui| {
            size = hud::pill_size(ui.ctx(), hud);
            hud::paint_pill(ui, p, Pos2::ZERO, hud, level);
        });
        for (id, deltas) in &out.textures_delta.set {
            for d in deltas {
                self.apply(*id, d);
            }
        }
        let shapes = std::mem::take(&mut out.shapes);
        let prims = self.ctx.tessellate(shapes, out.pixels_per_point);
        for id in &out.textures_delta.free {
            self.textures.remove(id);
        }
        out.textures_delta.clear(); // applied above
                                    // Integer logical size, so the buffer divides by its scale.
        let (lw, lh) = (size.x.ceil().max(1.0) as u32, size.y.ceil().max(1.0) as u32);
        let (w, h) = (lw * scale, lh * scale);
        let mut buf = vec![[0f32; 4]; (w * h) as usize];
        for cp in &prims {
            let Primitive::Mesh(mesh) = &cp.primitive else {
                continue;
            };
            let tex = self.textures.get(&mesh.texture_id);
            let clip = [
                (cp.clip_rect.min.x * ppp).floor().max(0.0) as i64,
                (cp.clip_rect.min.y * ppp).floor().max(0.0) as i64,
                ((cp.clip_rect.max.x * ppp).ceil() as i64).min(w as i64),
                ((cp.clip_rect.max.y * ppp).ceil() as i64).min(h as i64),
            ];
            for tri in mesh.indices.as_chunks::<3>().0 {
                let v = [
                    &mesh.vertices[tri[0] as usize],
                    &mesh.vertices[tri[1] as usize],
                    &mesh.vertices[tri[2] as usize],
                ];
                fill_triangle(&mut buf, w, clip, v, ppp, tex);
            }
        }
        let mut argb = Vec::with_capacity(buf.len() * 4);
        for [r, g, b, a] in buf {
            let q = |c: f32| (c.clamp(0.0, 1.0) * 255.0).round() as u8;
            argb.extend_from_slice(&[q(b), q(g), q(r), q(a)]);
        }
        Pixels {
            width: w,
            height: h,
            scale,
            argb,
        }
    }

    fn apply(&mut self, id: TextureId, delta: &egui::epaint::ImageDelta) {
        let ImageData::Color(img) = &delta.image;
        match delta.pos {
            None => {
                self.textures.insert(
                    id,
                    Texture {
                        size: img.size,
                        px: img.pixels.clone(),
                    },
                );
            }
            Some([x0, y0]) => {
                let Some(t) = self.textures.get_mut(&id) else {
                    return;
                };
                for y in 0..img.size[1] {
                    for x in 0..img.size[0] {
                        let (tx, ty) = (x0 + x, y0 + y);
                        if tx < t.size[0] && ty < t.size[1] {
                            t.px[ty * t.size[0] + tx] = img.pixels[y * img.size[0] + x];
                        }
                    }
                }
            }
        }
    }
}

fn rgba(c: Color32) -> [f32; 4] {
    let [r, g, b, a] = c.to_array();
    [r, g, b, a].map(|v| v as f32 / 255.0)
}

/// Bilinear sample (premultiplied) at normalized `uv`.
fn sample(t: &Texture, u: f32, v: f32) -> [f32; 4] {
    let (tw, th) = (t.size[0] as f32, t.size[1] as f32);
    let x = (u * tw - 0.5).clamp(0.0, tw - 1.0);
    let y = (v * th - 0.5).clamp(0.0, th - 1.0);
    let (x0, y0) = (x.floor() as usize, y.floor() as usize);
    let (x1, y1) = ((x0 + 1).min(t.size[0] - 1), (y0 + 1).min(t.size[1] - 1));
    let (fx, fy) = (x - x0 as f32, y - y0 as f32);
    let at = |x: usize, y: usize| rgba(t.px[y * t.size[0] + x]);
    let (a, b, c, d) = (at(x0, y0), at(x1, y0), at(x0, y1), at(x1, y1));
    let mut o = [0.0; 4];
    for i in 0..4 {
        let top = a[i] + (b[i] - a[i]) * fx;
        let bot = c[i] + (d[i] - c[i]) * fx;
        o[i] = top + (bot - top) * fy;
    }
    o
}

fn edge(a: (f32, f32), b: (f32, f32), p: (f32, f32)) -> f32 {
    (b.0 - a.0) * (p.1 - a.1) - (b.1 - a.1) * (p.0 - a.0)
}

/// Top-left rule for a positively oriented triangle: a pixel centre exactly
/// on a shared edge belongs to one triangle only (no double blending).
fn owns_edge(a: (f32, f32), b: (f32, f32)) -> bool {
    let (dx, dy) = (b.0 - a.0, b.1 - a.1);
    (dy == 0.0 && dx < 0.0) || dy > 0.0
}

fn fill_triangle(
    buf: &mut [[f32; 4]],
    w: u32,
    clip: [i64; 4],
    v: [&egui::epaint::Vertex; 3],
    ppp: f32,
    tex: Option<&Texture>,
) {
    let mut p = v.map(|v| (v.pos.x * ppp, v.pos.y * ppp));
    let mut vs = v;
    let mut area = edge(p[0], p[1], p[2]);
    if area.abs() < 1e-6 {
        return;
    }
    if area < 0.0 {
        p.swap(1, 2);
        vs.swap(1, 2);
        area = -area;
    }
    let minx = p.iter().map(|q| q.0).fold(f32::MAX, f32::min).floor() as i64;
    let maxx = p.iter().map(|q| q.0).fold(f32::MIN, f32::max).ceil() as i64;
    let miny = p.iter().map(|q| q.1).fold(f32::MAX, f32::min).floor() as i64;
    let maxy = p.iter().map(|q| q.1).fold(f32::MIN, f32::max).ceil() as i64;
    let (x0, x1) = (minx.max(clip[0]), maxx.min(clip[2]));
    let (y0, y1) = (miny.max(clip[1]), maxy.min(clip[3]));
    let col = vs.map(|v| rgba(v.color));
    let own = [
        owns_edge(p[1], p[2]),
        owns_edge(p[2], p[0]),
        owns_edge(p[0], p[1]),
    ];
    for y in y0..y1 {
        for x in x0..x1 {
            let c = (x as f32 + 0.5, y as f32 + 0.5);
            let e = [
                edge(p[1], p[2], c),
                edge(p[2], p[0], c),
                edge(p[0], p[1], c),
            ];
            if (0..3).any(|i| e[i] < 0.0 || (e[i] == 0.0 && !own[i])) {
                continue;
            }
            let b = e.map(|e| e / area);
            let mut src = [0.0f32; 4];
            for (i, s) in src.iter_mut().enumerate() {
                *s = col[0][i] * b[0] + col[1][i] * b[1] + col[2][i] * b[2];
            }
            if let Some(t) = tex {
                let u = vs[0].uv.x * b[0] + vs[1].uv.x * b[1] + vs[2].uv.x * b[2];
                let vv = vs[0].uv.y * b[0] + vs[1].uv.y * b[1] + vs[2].uv.y * b[2];
                let s = sample(t, u, vv);
                for i in 0..4 {
                    src[i] *= s[i];
                }
            }
            let d = &mut buf[(y as u32 * w + x as u32) as usize];
            let inv = 1.0 - src[3];
            for i in 0..4 {
                d[i] = src[i] + d[i] * inv;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn px(p: &Pixels, x: u32, y: u32) -> [u8; 4] {
        let i = ((y * p.width + x) * 4) as usize;
        [p.argb[i], p.argb[i + 1], p.argb[i + 2], p.argb[i + 3]]
    }

    /// The pill renders at the egui HUD's size, scale for scale, with
    /// transparent rounded corners, an opaque body in the palette's HUD fill
    /// and anti-aliased text (partial coverage somewhere in the label).
    #[test]
    fn pill_renders_with_theme_tokens_at_every_scale() {
        let mut r = PillRaster::new(None, None);
        for dark in [false, true] {
            let p = theme::palette(dark);
            let one = r.render(&Hud::Listening, 0.5, &p, 1);
            let two = r.render(&Hud::Listening, 0.5, &p, 2);
            assert_eq!(one.height, hud::PILL_H as u32);
            assert_eq!((two.width, two.height), (one.width * 2, one.height * 2));
            assert!(one.width > 80, "{}", one.width);
            assert_eq!(px(&one, 0, 0)[3], 0, "corner is transparent");
            // Body (left of the label, right of the dot) is the HUD fill.
            let [b, g, r_, a] = px(&one, 6, one.height / 2);
            let fill = p.hud_fill.to_array();
            assert!(a > 200, "opaque body: {a}");
            for (got, want) in [(r_, fill[0]), (g, fill[1]), (b, fill[2])] {
                assert!(got.abs_diff(want) <= 3, "fill {got} vs {want}");
            }
            let partial = (0..one.width)
                .flat_map(|x| (0..one.height).map(move |y| (x, y)))
                .filter(|&(x, y)| x > 40 && x < one.width - 30 && y > 6 && y < 26)
                .map(|(x, y)| px(&one, x, y))
                .any(|c| c[2] != px(&one, 6, 16)[2]);
            assert!(partial, "label pixels differ from the fill");
        }
    }
}
