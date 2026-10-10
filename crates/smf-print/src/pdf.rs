//! A small single-page PDF writer: vector paths, one or more JPEG images, transparency,
//! and the standard Helvetica fonts (no embedding needed, text stays selectable).
//!
//! All coordinates are millimetres from the page's top-left corner, which matches how the
//! layout is computed; they are converted to PDF points (bottom-left origin) internally.

const PT_PER_MM: f64 = 72.0 / 25.4;

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Rgb(pub f64, pub f64, pub f64);

impl Rgb {
    pub const BLACK: Rgb = Rgb(0.0, 0.0, 0.0);
    pub const WHITE: Rgb = Rgb(1.0, 1.0, 1.0);

    pub fn from_u8(c: [u8; 3]) -> Rgb {
        Rgb(
            c[0] as f64 / 255.0,
            c[1] as f64 / 255.0,
            c[2] as f64 / 255.0,
        )
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Font {
    Regular,
    Bold,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Align {
    Left,
    Center,
    Right,
}

struct Image {
    jpeg: Vec<u8>,
    w: u32,
    h: u32,
}

pub struct Page {
    w_mm: f64,
    h_mm: f64,
    content: String,
    images: Vec<Image>,
    alphas: Vec<f64>,
}

/// Helvetica advance widths for ASCII 32..=126 in 1/1000 em (Adobe core font metrics).
const HELV: [u16; 95] = [
    278, 278, 355, 556, 556, 889, 667, 191, 333, 333, 389, 584, 278, 333, 278, 278, 556, 556, 556,
    556, 556, 556, 556, 556, 556, 556, 278, 278, 584, 584, 584, 556, 1015, 667, 667, 722, 722, 667,
    611, 778, 722, 278, 500, 667, 556, 833, 722, 778, 667, 778, 722, 667, 611, 722, 667, 944, 667,
    667, 611, 278, 278, 278, 469, 556, 333, 556, 556, 500, 556, 556, 278, 556, 556, 222, 222, 500,
    222, 833, 556, 556, 556, 556, 333, 500, 278, 556, 500, 722, 500, 500, 500, 334, 260, 334, 584,
];

/// Unicode -> WinAnsi byte. Characters outside it become '?'.
fn win_ansi(c: char) -> u8 {
    match c {
        '\u{20}'..='\u{7e}' => c as u8,
        '\u{a0}'..='\u{ff}' => c as u8,
        '€' => 0x80,
        '‘' => 0x91,
        '’' => 0x92,
        '“' => 0x93,
        '”' => 0x94,
        '•' => 0x95,
        '–' => 0x96,
        '—' => 0x97,
        '…' => 0x85,
        '\t' | '\n' | '\r' => b' ',
        _ => b'?',
    }
}

/// Approximate text width in mm. Bold is estimated as 6% wider than regular.
pub fn text_width_mm(s: &str, size_pt: f64, font: Font) -> f64 {
    let units: f64 = s
        .chars()
        .map(|c| {
            let b = win_ansi(c);
            if (32..=126).contains(&b) {
                HELV[(b - 32) as usize] as f64
            } else {
                556.0
            }
        })
        .sum();
    let w = units / 1000.0 * size_pt / PT_PER_MM;
    if font == Font::Bold { w * 1.06 } else { w }
}

fn n(v: f64) -> String {
    // Fixed precision keeps files small and diffs stable; 3 decimals of a point is far below a pixel.
    let s = format!("{v:.3}");
    s.trim_end_matches('0').trim_end_matches('.').to_owned()
}

impl Page {
    pub fn new(w_mm: f64, h_mm: f64) -> Page {
        Page {
            w_mm,
            h_mm,
            content: String::new(),
            images: vec![],
            alphas: vec![],
        }
    }

    pub fn size_mm(&self) -> (f64, f64) {
        (self.w_mm, self.h_mm)
    }

    fn pt(&self, x_mm: f64, y_mm: f64) -> (f64, f64) {
        (x_mm * PT_PER_MM, (self.h_mm - y_mm) * PT_PER_MM)
    }

    fn op(&mut self, s: &str) {
        self.content.push_str(s);
        self.content.push('\n');
    }

    pub fn save(&mut self) {
        self.op("q");
    }

    pub fn restore(&mut self) {
        self.op("Q");
    }

    pub fn fill_color(&mut self, c: Rgb) {
        self.op(&format!("{} {} {} rg", n(c.0), n(c.1), n(c.2)));
    }

    pub fn stroke_color(&mut self, c: Rgb) {
        self.op(&format!("{} {} {} RG", n(c.0), n(c.1), n(c.2)));
    }

    pub fn line_width(&mut self, mm: f64) {
        self.op(&format!("{} w", n(mm * PT_PER_MM)));
    }

    pub fn dash(&mut self, on_mm: f64, off_mm: f64) {
        self.op(&format!(
            "[{} {}] 0 d",
            n(on_mm * PT_PER_MM),
            n(off_mm * PT_PER_MM)
        ));
    }

    pub fn solid(&mut self) {
        self.op("[] 0 d");
    }

    pub fn round_joins(&mut self) {
        self.op("1 j 1 J");
    }

    /// Constant opacity for subsequent fills and strokes (until `restore`).
    pub fn alpha(&mut self, a: f64) {
        let a = a.clamp(0.0, 1.0);
        let i = match self.alphas.iter().position(|x| (*x - a).abs() < 1e-9) {
            Some(i) => i,
            None => {
                self.alphas.push(a);
                self.alphas.len() - 1
            }
        };
        self.op(&format!("/GS{i} gs"));
    }

    pub fn move_to(&mut self, x: f64, y: f64) {
        let (px, py) = self.pt(x, y);
        self.op(&format!("{} {} m", n(px), n(py)));
    }

    pub fn line_to(&mut self, x: f64, y: f64) {
        let (px, py) = self.pt(x, y);
        self.op(&format!("{} {} l", n(px), n(py)));
    }

    pub fn close(&mut self) {
        self.op("h");
    }

    pub fn rect(&mut self, x: f64, y: f64, w: f64, h: f64) {
        let (px, py) = self.pt(x, y + h);
        self.op(&format!(
            "{} {} {} {} re",
            n(px),
            n(py),
            n(w * PT_PER_MM),
            n(h * PT_PER_MM)
        ));
    }

    pub fn circle(&mut self, cx: f64, cy: f64, r: f64) {
        // Four Bezier arcs; 0.5523 is the standard quarter-circle control constant.
        let k = 0.552_284_749_8 * r;
        self.move_to(cx + r, cy);
        for (c1, c2, e) in [
            ((cx + r, cy + k), (cx + k, cy + r), (cx, cy + r)),
            ((cx - k, cy + r), (cx - r, cy + k), (cx - r, cy)),
            ((cx - r, cy - k), (cx - k, cy - r), (cx, cy - r)),
            ((cx + k, cy - r), (cx + r, cy - k), (cx + r, cy)),
        ] {
            let (a, b) = self.pt(c1.0, c1.1);
            let (c, d) = self.pt(c2.0, c2.1);
            let (e0, e1) = self.pt(e.0, e.1);
            self.op(&format!(
                "{} {} {} {} {} {} c",
                n(a),
                n(b),
                n(c),
                n(d),
                n(e0),
                n(e1)
            ));
        }
        self.close();
    }

    pub fn polyline(&mut self, pts: &[(f64, f64)], closed: bool) {
        let mut it = pts.iter();
        if let Some(p) = it.next() {
            self.move_to(p.0, p.1);
        }
        for p in it {
            self.line_to(p.0, p.1);
        }
        if closed {
            self.close();
        }
    }

    pub fn stroke(&mut self) {
        self.op("S");
    }

    pub fn fill(&mut self) {
        self.op("f");
    }

    pub fn fill_stroke(&mut self) {
        self.op("B");
    }

    /// Intersect the clip region with the current path (and end the path).
    pub fn clip(&mut self) {
        self.op("W n");
    }

    /// Draw `s` with its baseline at (x, y). Returns the text width in mm.
    pub fn text(&mut self, x: f64, y: f64, size_pt: f64, font: Font, align: Align, s: &str) -> f64 {
        let w = text_width_mm(s, size_pt, font);
        let x0 = match align {
            Align::Left => x,
            Align::Center => x - w / 2.0,
            Align::Right => x - w,
        };
        let (px, py) = self.pt(x0, y);
        let hex: String = s.chars().map(|c| format!("{:02X}", win_ansi(c))).collect();
        let f = if font == Font::Bold { "F2" } else { "F1" };
        self.op(&format!(
            "BT /{f} {} Tf {} {} Td <{hex}> Tj ET",
            n(size_pt),
            n(px),
            n(py)
        ));
        w
    }

    /// Text rotated `deg` degrees counter-clockwise about its anchor (for vertical grid labels).
    #[allow(clippy::too_many_arguments)]
    pub fn text_rotated(
        &mut self,
        x: f64,
        y: f64,
        deg: f64,
        size_pt: f64,
        font: Font,
        align: Align,
        s: &str,
    ) {
        let w = text_width_mm(s, size_pt, font);
        let shift = match align {
            Align::Left => 0.0,
            Align::Center => -w / 2.0,
            Align::Right => -w,
        };
        let (px, py) = self.pt(x, y);
        let (sn, cs) = deg.to_radians().sin_cos();
        // Shift along the text direction before rotating.
        let (ox, oy) = (shift * PT_PER_MM * cs, shift * PT_PER_MM * sn);
        let hex: String = s.chars().map(|c| format!("{:02X}", win_ansi(c))).collect();
        let f = if font == Font::Bold { "F2" } else { "F1" };
        self.op(&format!(
            "BT /{f} {} Tf {} {} {} {} {} {} Tm <{hex}> Tj ET",
            n(size_pt),
            n(cs),
            n(sn),
            n(-sn),
            n(cs),
            n(px + ox),
            n(py + oy)
        ));
    }

    /// Register a JPEG and return its index for `draw_image`.
    pub fn add_jpeg(&mut self, jpeg: Vec<u8>, w: u32, h: u32) -> usize {
        self.images.push(Image { jpeg, w, h });
        self.images.len() - 1
    }

    pub fn draw_image(&mut self, idx: usize, x: f64, y: f64, w: f64, h: f64) {
        let (px, py) = self.pt(x, y + h);
        self.op(&format!(
            "q {} 0 0 {} {} {} cm /Im{idx} Do Q",
            n(w * PT_PER_MM),
            n(h * PT_PER_MM),
            n(px),
            n(py)
        ));
    }

    /// Serialize the page as a complete PDF 1.4 file.
    pub fn finish(&self, title: &str) -> Vec<u8> {
        let mut out: Vec<u8> = Vec::new();
        let mut offsets: Vec<usize> = Vec::new();
        out.extend_from_slice(b"%PDF-1.4\n%\xE2\xE3\xCF\xD3\n");
        let obj = |out: &mut Vec<u8>, offsets: &mut Vec<usize>, body: &[u8]| {
            offsets.push(out.len());
            let id = offsets.len();
            out.extend_from_slice(format!("{id} 0 obj\n").as_bytes());
            out.extend_from_slice(body);
            out.extend_from_slice(b"\nendobj\n");
        };
        // 1 catalog, 2 pages, 3 page, 4 content, 5/6 fonts, then images.
        obj(&mut out, &mut offsets, b"<< /Type /Catalog /Pages 2 0 R >>");
        obj(
            &mut out,
            &mut offsets,
            b"<< /Type /Pages /Kids [3 0 R] /Count 1 >>",
        );
        let (w_pt, h_pt) = (self.w_mm * PT_PER_MM, self.h_mm * PT_PER_MM);
        let first_img = 7;
        let xobjects: String = (0..self.images.len())
            .map(|i| format!("/Im{i} {} 0 R ", first_img + i))
            .collect();
        let gstates: String = self
            .alphas
            .iter()
            .enumerate()
            .map(|(i, a)| format!("/GS{i} << /ca {} /CA {} >> ", n(*a), n(*a)))
            .collect();
        obj(
            &mut out,
            &mut offsets,
            format!(
                "<< /Type /Page /Parent 2 0 R /MediaBox [0 0 {} {}] /Resources << /Font << /F1 5 0 R /F2 6 0 R >> /XObject << {xobjects}>> /ExtGState << {gstates}>> >> /Contents 4 0 R >>",
                n(w_pt),
                n(h_pt)
            )
            .as_bytes(),
        );
        let mut content = Vec::new();
        content.extend_from_slice(
            format!("<< /Length {} >>\nstream\n", self.content.len()).as_bytes(),
        );
        content.extend_from_slice(self.content.as_bytes());
        content.extend_from_slice(b"endstream");
        obj(&mut out, &mut offsets, &content);
        obj(
            &mut out,
            &mut offsets,
            b"<< /Type /Font /Subtype /Type1 /BaseFont /Helvetica /Encoding /WinAnsiEncoding >>",
        );
        obj(&mut out, &mut offsets, b"<< /Type /Font /Subtype /Type1 /BaseFont /Helvetica-Bold /Encoding /WinAnsiEncoding >>");
        for im in &self.images {
            let mut body = Vec::new();
            body.extend_from_slice(
                format!(
                    "<< /Type /XObject /Subtype /Image /Width {} /Height {} /ColorSpace /DeviceRGB /BitsPerComponent 8 /Filter /DCTDecode /Length {} >>\nstream\n",
                    im.w,
                    im.h,
                    im.jpeg.len()
                )
                .as_bytes(),
            );
            body.extend_from_slice(&im.jpeg);
            body.extend_from_slice(b"\nendstream");
            obj(&mut out, &mut offsets, &body);
        }
        // Info dictionary (object after the images).
        let info_id = offsets.len() + 1;
        let title_hex: String = title
            .chars()
            .map(|c| format!("{:02X}", win_ansi(c)))
            .collect();
        obj(&mut out, &mut offsets, format!("<< /Title <{title_hex}> /Producer (ShowMeFire Field) /Creator (ShowMeFire Field) >>").as_bytes());
        let xref_at = out.len();
        out.extend_from_slice(format!("xref\n0 {}\n", offsets.len() + 1).as_bytes());
        out.extend_from_slice(b"0000000000 65535 f \n");
        for o in &offsets {
            out.extend_from_slice(format!("{o:010} 00000 n \n").as_bytes());
        }
        out.extend_from_slice(
            format!("trailer\n<< /Size {} /Root 1 0 R /Info {info_id} 0 R >>\nstartxref\n{xref_at}\n%%EOF\n", offsets.len() + 1).as_bytes(),
        );
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use lopdf::{Document, Object};

    fn jpeg(w: u32, h: u32) -> Vec<u8> {
        let img = image::RgbImage::from_pixel(w, h, image::Rgb([200, 30, 30]));
        let mut buf = Vec::new();
        image::codecs::jpeg::JpegEncoder::new_with_quality(&mut buf, 85)
            .encode(img.as_raw(), w, h, image::ExtendedColorType::Rgb8)
            .unwrap();
        buf
    }

    fn sample() -> Vec<u8> {
        let mut p = Page::new(215.9, 279.4);
        p.save();
        p.fill_color(Rgb(0.9, 0.9, 0.2));
        p.rect(10.0, 10.0, 50.0, 20.0);
        p.fill();
        p.restore();
        p.stroke_color(Rgb::BLACK);
        p.line_width(0.5);
        p.polyline(&[(20.0, 40.0), (80.0, 60.0), (60.0, 90.0)], true);
        p.stroke();
        p.circle(100.0, 100.0, 5.0);
        p.stroke();
        p.alpha(0.5);
        let im = p.add_jpeg(jpeg(8, 8), 8, 8);
        p.draw_image(im, 120.0, 20.0, 40.0, 40.0);
        p.text(
            15.0,
            25.0,
            12.0,
            Font::Bold,
            Align::Left,
            "Hello (PDF) \\ 1:24,000",
        );
        p.text(
            100.0,
            270.0,
            8.0,
            Font::Regular,
            Align::Center,
            "Caf\u{e9} \u{2013} \u{4e2d}",
        );
        p.finish("Test map")
    }

    #[test]
    fn produces_a_structurally_valid_pdf_that_lopdf_can_parse() {
        let bytes = sample();
        assert!(bytes.starts_with(b"%PDF-1.4"));
        assert!(bytes.ends_with(b"%%EOF\n"));
        let doc = Document::load_mem(&bytes).expect("valid PDF with a correct xref table");
        assert_eq!(doc.get_pages().len(), 1);
        let (_, page_id) = doc.get_pages().into_iter().next().unwrap();
        let page = doc.get_dictionary(page_id).unwrap();
        let mb: Vec<f64> = page
            .get(b"MediaBox")
            .unwrap()
            .as_array()
            .unwrap()
            .iter()
            .map(|o| match o {
                Object::Integer(i) => *i as f64,
                Object::Real(r) => *r as f64,
                _ => panic!(),
            })
            .collect();
        // Letter: 8.5 x 11 in = 612 x 792 pt.
        assert!(
            (mb[2] - 612.0).abs() < 0.1 && (mb[3] - 792.0).abs() < 0.1,
            "{mb:?}"
        );
    }

    #[test]
    fn xref_offsets_point_at_the_objects() {
        let bytes = sample();
        // Work on bytes: the binary header comment makes a lossy string's offsets differ from the file's.
        let tail_start = bytes.windows(9).rposition(|w| w == b"startxref").unwrap();
        let start: usize = String::from_utf8_lossy(&bytes[tail_start + 9..])
            .lines()
            .find(|l| !l.is_empty())
            .unwrap()
            .trim()
            .parse()
            .unwrap();
        assert!(bytes[start..].starts_with(b"xref"));
        let tail = String::from_utf8_lossy(&bytes[start..]).into_owned();
        let table: Vec<&str> = tail
            .lines()
            .skip(2)
            .take_while(|l| !l.starts_with("trailer"))
            .collect();
        assert!(table.len() >= 8, "{}", table.len());
        for (i, line) in table.iter().enumerate().skip(1) {
            let off: usize = line[..10].parse().unwrap();
            let want = format!("{i} 0 obj");
            assert!(
                bytes[off..].starts_with(want.as_bytes()),
                "object {i} at {off}"
            );
        }
    }

    #[test]
    fn text_is_extractable_and_escaped_safely() {
        let bytes = sample();
        let doc = Document::load_mem(&bytes).unwrap();
        let text = doc.extract_text(&[1]).unwrap();
        assert!(text.contains("Hello (PDF) \\ 1:24,000"), "{text:?}");
        assert!(text.contains("Caf\u{e9}"), "latin-1 survives: {text:?}");
        assert!(
            text.contains('?'),
            "characters outside WinAnsi degrade to ?"
        );
    }

    #[test]
    fn content_uses_pdf_coordinates_with_bottom_left_origin() {
        let mut p = Page::new(100.0, 200.0);
        p.rect(0.0, 0.0, 10.0, 10.0); // top-left 10x10 mm
        p.fill();
        let s = String::from_utf8_lossy(&p.finish("t")).into_owned();
        // y = (200 - 10) mm = 190 mm = 538.583 pt
        assert!(
            s.contains("0 538.583 28.346 28.346 re"),
            "{}",
            s.lines().find(|l| l.ends_with(" re")).unwrap()
        );
    }

    #[test]
    fn image_is_embedded_as_a_dct_stream_with_its_matrix() {
        let bytes = sample();
        let doc = Document::load_mem(&bytes).unwrap();
        let imgs: Vec<_> = doc
            .objects
            .values()
            .filter_map(|o| o.as_stream().ok())
            .filter(|s| {
                s.dict
                    .get(b"Subtype")
                    .map(|v| v == &Object::Name(b"Image".to_vec()))
                    .unwrap_or(false)
            })
            .collect();
        assert_eq!(imgs.len(), 1);
        assert_eq!(
            imgs[0].dict.get(b"Filter").unwrap(),
            &Object::Name(b"DCTDecode".to_vec())
        );
        assert_eq!(&imgs[0].content[..2], &[0xFF, 0xD8], "raw JPEG bytes");
        let s = String::from_utf8_lossy(&bytes);
        // 40 mm square = 113.386 pt.
        assert!(
            s.contains("113.386 0 0 113.386"),
            "image matrix scales mm to points"
        );
    }

    #[test]
    fn text_width_estimates_are_sane() {
        // "Hello" in 12 pt Helvetica: (722+556+222+222+556)=2278 units -> 27.34 pt = 9.64 mm
        let w = text_width_mm("Hello", 12.0, Font::Regular);
        assert!((w - 9.64).abs() < 0.02, "{w}");
        assert!(text_width_mm("Hello", 12.0, Font::Bold) > w);
        assert_eq!(text_width_mm("", 12.0, Font::Regular), 0.0);
    }

    #[test]
    fn alpha_states_are_deduplicated() {
        let mut p = Page::new(10.0, 10.0);
        p.alpha(0.5);
        p.alpha(0.5);
        p.alpha(0.25);
        let s = String::from_utf8_lossy(&p.finish("t")).into_owned();
        assert_eq!(s.matches("/GS0 << /ca 0.5").count(), 1);
        assert!(s.contains("/GS1 << /ca 0.25"));
        assert!(!s.contains("/GS2"));
    }
}
