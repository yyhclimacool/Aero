//! Mermaid / PDF 的纯渲染部分；由渲染服务的后台任务调用。
use std::sync::{Arc, OnceLock};

use gpui_kit::{FontWeight, TextRun, TextSystem, WindowTextSystem, px};
use hayro::hayro_syntax::Pdf;
use image::RgbaImage;
use merman_render::text::{
    TextMeasurer, TextMetrics, TextStyle, WrapMode, split_html_br_lines, wrap_text_lines_measurer,
};
use resvg::usvg::{self, fontdb};

#[path = "pdf_preflight.rs"]
mod pdf_preflight;

// 修改排版、字体解析或依赖版本时递增；持久缓存不能复用旧渲染结果。
pub(super) const DIAGRAM_VERSION: &str = "merman-0.7.0-resvg-0.45.1-host-v1";
pub(super) const PDF_VERSION: &str = "hayro-0.7.1-v3";
const MAX_SIDE: u32 = 16_384;
const MAX_PIXELS: u64 = 16 * 1024 * 1024;

fn fonts() -> Arc<fontdb::Database> {
    static FONTS: OnceLock<Arc<fontdb::Database>> = OnceLock::new();
    FONTS
        .get_or_init(|| {
            let mut db = fontdb::Database::new();
            db.load_system_fonts();
            Arc::new(db)
        })
        .clone()
}

/// GPUI 的私有系统字体别名不是 SVG 字体族。两条管线统一选用 fontdb 能找到的具体族名。
fn font_family(requested: &str) -> String {
    let db = fonts();
    for name in requested.split(',').map(|name| name.trim().trim_matches(['\'', '"'])) {
        let family = match name {
            "" | ".SystemUIFont" | "sans-serif" => fontdb::Family::SansSerif,
            "serif" => fontdb::Family::Serif,
            "monospace" => fontdb::Family::Monospace,
            name => fontdb::Family::Name(name),
        };
        if let Some(id) = db.query(&fontdb::Query { families: &[family], ..Default::default() })
            && let Some(face) = db.face(id)
            && let Some((name, _)) = face.families.first()
        {
            return name.clone();
        }
    }
    db.family_name(&fontdb::Family::SansSerif).to_owned()
}

struct HostMeasure {
    text: Arc<TextSystem>,
    family: String,
}

impl HostMeasure {
    fn width(&self, text: &str, style: &TextStyle) -> f64 {
        let family = style.font_family.as_deref().map(font_family).unwrap_or_else(|| self.family.clone());
        let mut font = gpui_kit::font(family);
        if let Some(weight) = &style.font_weight {
            font.weight = match weight.as_str() {
                "bold" | "bolder" => FontWeight::BOLD,
                _ => FontWeight(weight.parse::<f32>().unwrap_or(400.).clamp(100., 900.)),
            };
        }
        let run = TextRun { len: text.len(), font, ..Default::default() };
        let shaped = WindowTextSystem::new(self.text.clone()).shape_line(
            text.to_owned().into(),
            px(style.font_size.clamp(1., 512.) as f32),
            &[run],
            None,
        );
        f64::from(f32::from(shaped.width))
    }

    fn lines(&self, lines: &[String], style: &TextStyle) -> TextMetrics {
        TextMetrics {
            width: lines.iter().map(|line| self.width(line, style)).fold(0., f64::max),
            height: lines.len() as f64 * style.font_size.clamp(1., 512.) * 1.2,
            line_count: lines.len(),
        }
    }
}

impl TextMeasurer for HostMeasure {
    fn measure(&self, text: &str, style: &TextStyle) -> TextMetrics {
        // shape_line 不能接收换行；同时保留 Mermaid 的 HTML 换行。
        let normalized = text.replace("\r\n", "\n");
        let lines = normalized
            .split(['\n', '\r'])
            .flat_map(split_html_br_lines)
            .map(str::to_owned)
            .collect::<Vec<_>>();
        self.lines(&lines, style)
    }

    fn measure_wrapped(
        &self,
        text: &str,
        style: &TextStyle,
        max_width: Option<f64>,
        _: WrapMode,
    ) -> TextMetrics {
        let mut lines = Vec::new();
        let normalized = text.replace("\r\n", "\n");
        for line in normalized.split(['\n', '\r']) {
            // 上游 helper 用宿主实际 advance 折行，并按 Unicode 字符拆分无空格的中文长词。
            lines.extend(wrap_text_lines_measurer(line, self, style, max_width));
        }
        self.lines(&lines, style)
    }
}

fn diagram_svg(source: &str, dark: bool, font: &str, text_system: Arc<TextSystem>) -> Result<String, String> {
    let family = font_family(font);
    let config = merman::MermaidConfig::from_value(serde_json::json!({
        "theme": if dark { "dark" } else { "default" },
        "fontFamily": family,
        "themeVariables": { "fontFamily": family },
        "securityLevel": "strict",
        "htmlLabels": false,
        "flowchart": { "htmlLabels": false },
    }));
    merman::render::HeadlessRenderer::new()
        .with_site_config(config)
        .with_text_measurer(Arc::new(HostMeasure { text: text_system, family }))
        .render_svg_resvg_safe_sync(source)
        .map_err(|error| format!("Mermaid 语法或渲染错误：{error}"))?
        .ok_or_else(|| "Mermaid 源码为空或没有可渲染图表".into())
}

fn diagram_pixels(svg: &[u8], font: &str, device: u32) -> Result<RgbaImage, String> {
    let mut options = usvg::Options { fontdb: fonts(), font_family: font_family(font), ..Default::default() };
    // usvg 默认会打开本地路径；源码绝不能借 image href 读取 vault 之外的数据。
    options.image_href_resolver.resolve_string = Box::new(|_, _| None);
    let tree =
        usvg::Tree::from_data(svg, &options).map_err(|error| format!("Mermaid SVG 无法解析：{error}"))?;
    let size = tree.size();
    let (width, height) = render_size(f64::from(size.width()), f64::from(size.height()), device)?;
    let mut pixmap = resvg::tiny_skia::Pixmap::new(width, height).ok_or("Mermaid 位图无法分配")?;
    let scale = width as f32 / size.width();
    resvg::render(&tree, resvg::tiny_skia::Transform::from_scale(scale, scale), &mut pixmap.as_mut());
    let data = pixmap
        .pixels()
        .iter()
        .flat_map(|pixel| {
            let p = pixel.demultiply();
            [p.red(), p.green(), p.blue(), p.alpha()]
        })
        .collect();
    RgbaImage::from_raw(width, height, data).ok_or_else(|| "Mermaid 光栅化失败".into())
}

fn render_size(width: f64, height: f64, device: u32) -> Result<(u32, u32), String> {
    if !width.is_finite() || !height.is_finite() || width <= 0. || height <= 0. {
        return Err("渲染尺寸必须是有限正数".into());
    }
    let scaled_height = (height / width * f64::from(device)).ceil().max(1.);
    if device == 0 || device > MAX_SIDE || !scaled_height.is_finite() || scaled_height > f64::from(MAX_SIDE) {
        return Err("渲染尺寸过大（单边最多 16384 像素）".into());
    }
    let height = scaled_height as u32;
    if u64::from(device) * u64::from(height) > MAX_PIXELS {
        return Err("渲染尺寸过大（最多 1600 万像素）".into());
    }
    Ok((device, height))
}

fn guarded<T>(kind: &str, f: impl FnOnce() -> Result<T, String>) -> Result<T, String> {
    std::panic::catch_unwind(std::panic::AssertUnwindSafe(f))
        .unwrap_or_else(|_| Err(format!("{kind} 渲染失败：源内容不受支持或已损坏")))
}

pub(super) fn diagram(
    source: &str,
    dark: bool,
    font: &str,
    text_system: Arc<TextSystem>,
    device: u32,
) -> Result<RgbaImage, String> {
    guarded("Mermaid", || {
        diagram_pixels(diagram_svg(source, dark, font, text_system)?.as_bytes(), font, device)
    })
}

/// 同步纯渲染，由调用方放在后台执行；不会创建或改写 vault 文件。
pub fn export_diagram(
    source: &str,
    dark: bool,
    font: &str,
    text_system: Arc<TextSystem>,
    png: bool,
) -> Result<Vec<u8>, String> {
    guarded("Mermaid", || {
        let svg = diagram_svg(source, dark, font, text_system)?;
        if !png {
            return Ok(svg.into_bytes());
        }
        let rgba = diagram_pixels(svg.as_bytes(), font, 680 * 2)?;
        let mut bytes = std::io::Cursor::new(Vec::new());
        rgba.write_to(&mut bytes, image::ImageFormat::Png)
            .map_err(|error| format!("PNG 导出失败：{error}"))?;
        Ok(bytes.into_inner())
    })
}

/// 每次磁盘命中前也打开 PDF：页码校验和总页数不依赖只存在于内存的状态。
pub(super) struct PdfPage {
    pdf: Pdf,
    index: usize,
    pub pages: u32,
    pub size: (u32, u32),
}

impl PdfPage {
    pub fn new(bytes: Vec<u8>, page: u32, device: u32) -> Result<Self, String> {
        guarded("PDF", || {
            if page == 0 {
                return Err("PDF 页码从 1 开始".into());
            }
            // catch_unwind 不能捕获依赖的栈溢出，必须在任何递归语法解析前限制源数据。
            pdf_preflight::source(&bytes)?;
            let pdf = Pdf::new(bytes).map_err(|error| match error {
                hayro::hayro_syntax::LoadPdfError::Decryption(_) => "PDF 无法解密，请先移除密码保护",
                hayro::hayro_syntax::LoadPdfError::Invalid => "PDF 无法打开：文件已损坏或格式无效",
            })?;
            pdf_preflight::images(&pdf)?;
            let pages = u32::try_from(pdf.pages().len()).map_err(|_| "PDF 页数超出范围")?;
            let index = (page - 1) as usize;
            let selected =
                pdf.pages().get(index).ok_or_else(|| format!("PDF 页码 {page} 超出范围（共 {pages} 页）"))?;
            // hayro 会把零面积页自动换成 A4；在调用它之前拒绝非法原始页框。
            let bounds = selected.intersected_crop_box();
            render_size(bounds.width(), bounds.height(), 1)?;
            let (width, height) = selected.render_dimensions();
            let size = render_size(f64::from(width), f64::from(height), device)?;
            Ok(Self { pdf, index, pages, size })
        })
    }

    pub fn rasterize(&self) -> Result<RgbaImage, String> {
        guarded("PDF", || {
            let page = &self.pdf.pages()[self.index];
            let (width, height) = self.size;
            let scale = width as f32 / page.render_dimensions().0;
            // RenderCache 含 Rc，必须在当前后台任务内构造，不能跨任务发送。
            let pixmap = hayro::render(
                page,
                &hayro::RenderCache::new(),
                &Default::default(),
                &hayro::RenderSettings {
                    x_scale: scale,
                    y_scale: scale,
                    width: Some(width as u16),
                    height: Some(height as u16),
                    bg_color: hayro::vello_cpu::color::palette::css::WHITE,
                },
            );
            let data = pixmap.take_unpremultiplied().into_iter().flat_map(|p| [p.r, p.g, p.b, p.a]).collect();
            RgbaImage::from_raw(width, height, data).ok_or_else(|| "PDF 页面光栅化失败".into())
        })
    }
}

#[cfg(test)]
fn pdf_page(bytes: Vec<u8>, page: u32, device: u32) -> Result<(RgbaImage, u32), String> {
    let page = PdfPage::new(bytes, page, device)?;
    Ok((page.rasterize()?, page.pages))
}

/// 无外链的两页 PDF：第一页红色，第二页蓝色；供编辑器集成测试复用。
#[cfg(test)]
pub(crate) fn two_page_pdf() -> Vec<u8> {
    test_pdf("200", "100")
}

#[cfg(test)]
fn test_pdf(width: &str, height: &str) -> Vec<u8> {
    test_pdf_extra(width, height, "", &[])
}

#[cfg(test)]
fn test_pdf_extra(width: &str, height: &str, catalog: &str, extra: &[String]) -> Vec<u8> {
    let streams = ["1 0 0 rg 0 0 200 100 re f", "0 0 1 rg 0 0 200 100 re f"];
    let objects = [
        format!("<< /Type /Catalog /Pages 2 0 R {catalog} >>"),
        "<< /Type /Pages /Kids [3 0 R 4 0 R] /Count 2 >>".to_owned(),
        format!(
            "<< /Type /Page /Parent 2 0 R /MediaBox [0 0 {width} {height}] /Resources << >> /Contents 5 0 R >>"
        ),
        format!(
            "<< /Type /Page /Parent 2 0 R /MediaBox [0 0 {width} {height}] /Resources << >> /Contents 6 0 R >>"
        ),
        format!("<< /Length {} >>\nstream\n{}\nendstream", streams[0].len(), streams[0]),
        format!("<< /Length {} >>\nstream\n{}\nendstream", streams[1].len(), streams[1]),
    ];
    let mut pdf = String::from("%PDF-1.4\n");
    let mut offsets = vec![0];
    for (i, object) in objects.iter().chain(extra).enumerate() {
        offsets.push(pdf.len());
        pdf.push_str(&format!("{} 0 obj\n{object}\nendobj\n", i + 1));
    }
    let xref = pdf.len();
    let count = offsets.len();
    pdf.push_str(&format!("xref\n0 {count}\n0000000000 65535 f \n"));
    for offset in &offsets[1..] {
        pdf.push_str(&format!("{offset:010} 00000 n \n"));
    }
    pdf.push_str(&format!("trailer\n<< /Size {count} /Root 1 0 R >>\nstartxref\n{xref}\n%%EOF\n"));
    pdf.into_bytes()
}

#[cfg(test)]
mod tests {
    use super::{
        HostMeasure, PdfPage, diagram_pixels, export_diagram, pdf_page, test_pdf, test_pdf_extra,
        two_page_pdf,
    };
    use merman_render::text::{TextMeasurer, TextStyle, WrapMode};

    // 这些恶意声明只打开 PDF，绝不调用 rasterize 或解码图像。
    fn image_object(width: &str, height: &str, fields: &str) -> String {
        format!(
            "<< /Type /XObject /Subtype /Image /Width {width} /Height {height} {fields} /Length 0 >>\nstream\n\nendstream"
        )
    }

    fn pdf_open_error(bytes: Vec<u8>) -> String {
        PdfPage::new(bytes, 1, 200).err().expect("预检应在渲染前拒绝此 PDF")
    }

    #[test]
    fn pdf_preflight_rejects_deep_catalog_arrays() {
        let catalog = format!("/Unused {}0{}", "[".repeat(160), "]".repeat(160));
        let error = pdf_open_error(test_pdf_extra("200", "100", &catalog, &[]));
        assert!(error.contains("嵌套"), "{error}");
    }

    #[test]
    fn pdf_preflight_rejects_oversized_image_declarations() {
        for (width, height) in [("16385", "1"), ("4097", "4096"), ("0", "1"), ("-1", "1")] {
            let image = image_object(width, height, "/BitsPerComponent 8 /ColorSpace /DeviceRGB");
            let error = pdf_open_error(test_pdf_extra("200", "100", "", &[image]));
            assert!(error.contains("图像") && error.contains("尺寸"), "{error}");
        }
    }

    #[test]
    fn pdf_preflight_rejects_invalid_image_component_depth() {
        for bpc in ["0", "3", "32", "-1", "256"] {
            let image = image_object("1", "1", &format!("/BitsPerComponent {bpc}"));
            let error = pdf_open_error(test_pdf_extra("200", "100", "", &[image]));
            assert!(error.contains("图像") && error.contains("位深"), "{error}");
        }
    }

    #[test]
    fn pdf_preflight_budgets_all_images_including_masks() {
        let image = image_object("4096", "2049", "/BitsPerComponent 8");
        let error = pdf_open_error(test_pdf_extra("200", "100", "", &[image.clone(), image]));
        assert!(error.contains("图像") && error.contains("总像素"), "{error}");
        for mask_key in ["Mask", "SMask"] {
            let image = image_object("1", "1", &format!("/BitsPerComponent 8 /{mask_key} 8 0 R"));
            let mask = image_object("16385", "1", "/ImageMask true");
            let error = pdf_open_error(test_pdf_extra("200", "100", "", &[image, mask]));
            assert!(error.contains("图像") && error.contains("尺寸"), "{error}");
        }
    }

    #[test]
    fn pdf_preflight_accepts_strings_comments_and_binary_streams() {
        let brackets = "[<<".repeat(160);
        let catalog = format!(
            "/Text (escaped \\( \\) \\\\ (nested) {brackets}) /Hex <5b3c3c> % {brackets}\n /stream /endstream"
        );
        // 包括伪 endstream、NUL 和不平衡括号；只能按 stream 边界跳过，不能当语法读取。
        let payload = format!("\0\nendstream\n{brackets}(\\\0");
        let stream = format!("<< /Length {} >>\nstream\n{payload}\nendstream", payload.len());
        let mut bytes = test_pdf_extra("200", "100", &catalog, &[stream]);
        *bytes.iter_mut().find(|b| **b == 0).unwrap() = 0xff;
        for page in [1, 2] {
            let opened = PdfPage::new(bytes.clone(), page, 200).expect("字符串、注释和载荷不计入结构深度");
            assert_eq!(opened.pages, 2);
            assert_eq!(opened.size, (200, 100));
        }
    }

    #[test]
    fn pdf_preflight_checks_referenced_image_metadata_and_aliases() {
        let image = image_object("8 0 R", "9 0 R", "/BitsPerComponent 10 0 R");
        for (width, bpc, error_kind) in [("16385", "8", "尺寸"), ("1", "32", "位深")] {
            let extra = [image.clone(), width.into(), "1".into(), bpc.into()];
            let error = pdf_open_error(test_pdf_extra("200", "100", "", &extra));
            assert!(error.contains(error_kind), "{error}");
        }
        for (fields, error_kind) in [("/W 16385", "尺寸"), ("/H 16385", "尺寸"), ("/BPC 32", "位深")] {
            let image = image_object("1", "1", fields).replace("/Image", "/Im#61ge");
            let error = pdf_open_error(test_pdf_extra("200", "100", "", &[image]));
            assert!(error.contains(error_kind), "{error}");
        }
    }

    #[test]
    fn pdf_preflight_accepts_bounded_images_without_decoding() {
        for fields in [
            "",
            "/BitsPerComponent 1",
            "/BitsPerComponent 2",
            "/BitsPerComponent 4",
            "/BitsPerComponent 8",
            "/BitsPerComponent 16",
            "/ImageMask true",
            "/Filter /JPXDecode",
        ] {
            let image = image_object("1", "1", fields);
            // JPX 空载荷不能解码，但只检查声明应通过。
            assert!(PdfPage::new(test_pdf_extra("200", "100", "", &[image]), 1, 200).is_ok());
        }
        let image = image_object("4096", "4096", "/BitsPerComponent 8");
        assert!(PdfPage::new(test_pdf_extra("200", "100", "", &[image]), 1, 200).is_ok());
        let image = image_object("16384", "1", "/BitsPerComponent 8");
        assert!(PdfPage::new(test_pdf_extra("200", "100", "", &[image]), 1, 200).is_ok());
    }

    #[test]
    fn pdf_preflight_accepts_indirect_stream_length() {
        let payload = format!("\0{}((", "[<<".repeat(160));
        let stream = format!("<< /Length 8 0 R >>\nstream\r\n{payload}\nendstream");
        let bytes = test_pdf_extra("200", "100", "", &[stream, payload.len().to_string()]);
        assert!(PdfPage::new(bytes, 1, 200).is_ok());
    }

    #[test]
    fn render_dimensions_are_finite_positive_and_bounded() {
        assert_eq!(super::render_size(200., 100., 400).unwrap(), (400, 200));
        for (width, height) in [(f64::NAN, 100.), (200., f64::INFINITY), (-1., 100.), (0., 100.), (1., 1e12)]
        {
            assert!(super::render_size(width, height, 400).is_err());
        }
        assert!(super::render_size(1., 1., 5000).is_err(), "总像素数量应受限");
    }

    #[gpui_kit::test]
    fn host_measurement_preserves_breaks_and_wraps_chinese(cx: &mut gpui_kit::TestAppContext) {
        let host = HostMeasure { text: cx.text_system().clone(), family: "PingFang SC".into() };
        let style = TextStyle::default();
        let single = host.measure("中文", &style);
        assert!(single.width > 0. && single.height > 0.);
        let multiline = host.measure("中文\n第二行<br/>末行", &style);
        assert_eq!(multiline.line_count, 3);
        assert!(multiline.height >= single.height * 3.);
        assert_eq!(host.measure("中文\r\n第二行", &style).line_count, 2, "CRLF 只算一次换行");
        let wrapped = host.measure_wrapped(
            "中文需要正确折行中文需要正确折行",
            &style,
            Some(single.width * 2.),
            WrapMode::HtmlLike,
        );
        assert!(wrapped.line_count > 1);
        assert!(wrapped.width <= single.width * 2. + 0.1);
    }

    #[test]
    fn diagram_svg_disables_external_images_and_unpremultiplies_alpha() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("secret.png");
        image::RgbaImage::from_pixel(8, 8, image::Rgba([255, 0, 0, 255])).save(&file).unwrap();
        for href in [file.to_str().unwrap(), "https://example.invalid/secret.png"] {
            let svg = format!(
                r#"<svg xmlns="http://www.w3.org/2000/svg" xmlns:xlink="http://www.w3.org/1999/xlink" width="8" height="8"><image width="8" height="8" xlink:href="{href}"/></svg>"#
            );
            let rgba = diagram_pixels(svg.as_bytes(), "Arial", 8).unwrap();
            assert!(rgba.pixels().all(|p| p[3] == 0), "外部图像不得读取");
        }
        let svg = br#"<svg xmlns="http://www.w3.org/2000/svg" width="8" height="8"><rect width="8" height="8" fill="red" opacity="0.5"/></svg>"#;
        let rgba = diagram_pixels(svg, "Arial", 8).unwrap();
        assert_eq!(&rgba.get_pixel(4, 4).0[..3], &[255, 0, 0]);
        assert!((127..=128).contains(&rgba.get_pixel(4, 4)[3]));
        assert!(diagram_pixels(svg, "Arial", u32::MAX).is_err());
    }

    #[test]
    fn diagram_svg_rasterizes_chinese_glyphs() {
        let svg = r#"<svg xmlns="http://www.w3.org/2000/svg" width="160" height="50"><text x="5" y="35" font-family="PingFang SC" font-size="28">中文</text></svg>"#;
        let rgba = diagram_pixels(svg.as_bytes(), "PingFang SC", 320).unwrap();
        assert!(rgba.pixels().filter(|p| p[3] > 0).count() > 100, "不能丢掉中文字形");
    }

    #[test]
    fn pdf_renders_distinct_pages_with_total_and_hidpi() {
        let (red, pages) = pdf_page(two_page_pdf(), 1, 200).expect("有效 PDF 第一页应渲染");
        assert_eq!(pages, 2);
        assert_eq!(red.dimensions(), (200, 100));
        assert_eq!(red.get_pixel(50, 50).0, [255, 0, 0, 255]);
        let (blue, pages) = pdf_page(two_page_pdf(), 2, 400).unwrap();
        assert_eq!(pages, 2);
        assert_eq!(blue.dimensions(), (400, 200));
        assert_eq!(blue.get_pixel(100, 100).0, [0, 0, 255, 255]);
    }

    #[test]
    fn pdf_has_white_paper_and_preserves_rotation() {
        let bytes = String::from_utf8(two_page_pdf()).unwrap().replace("0 0 200 100 re", "0 0 100 100 re");
        let (rgba, _) = pdf_page(bytes.into_bytes(), 1, 200).unwrap();
        assert_eq!(rgba.get_pixel(150, 50).0, [255, 255, 255, 255], "PDF 纸张在深色主题中也应为白色");
        // 添加旋转会改变 xref 偏移，hayro 可从对象恢复；两页数量仍可验证。
        let rotated =
            String::from_utf8(two_page_pdf()).unwrap().replace("/Resources", "/Rotate 90 /Resources");
        let (rgba, pages) = pdf_page(rotated.into_bytes(), 1, 100).unwrap();
        assert_eq!(pages, 2);
        assert_eq!(rgba.dimensions(), (100, 200));
    }

    #[test]
    fn pdf_rejects_invalid_pages_bytes_and_unsafe_sizes() {
        for page in [0, 3, u32::MAX] {
            let error = pdf_page(two_page_pdf(), page, 200).unwrap_err();
            assert!(error.contains("页码"), "{error}");
        }
        assert!(pdf_page(b"not PDF".to_vec(), 1, 200).unwrap_err().contains("PDF"));
        for (width, height) in [("0", "100"), ("200", "0"), ("1", "1000000000")] {
            let error = pdf_page(test_pdf(width, height), 1, 200).unwrap_err();
            assert!(error.contains("尺寸"), "{error}");
        }
        assert!(pdf_page(two_page_pdf(), 1, u32::MAX).unwrap_err().contains("尺寸"));
    }

    #[gpui_kit::test]
    fn diagram_exports_chinese_multiline_svg_and_high_resolution_png(cx: &mut gpui_kit::TestAppContext) {
        let source = "flowchart TD\n A[\"中文标题<br/>第二行\"] --> B[\"完成\"]";
        let text = cx.text_system().clone();
        let svg = export_diagram(source, false, "PingFang SC", text.clone(), false)
            .expect("有效 Mermaid 应导出 SVG");
        let svg = String::from_utf8(svg).unwrap();
        assert!(svg.contains("<svg") && svg.contains("中文标题") && svg.contains("第二行"));
        let png = export_diagram(source, false, "PingFang SC", text.clone(), true).unwrap();
        let rgba = image::load_from_memory(&png).unwrap().to_rgba8();
        assert_eq!(rgba.width(), 1360);
        assert!(rgba.height() > 20);
        assert!(rgba.pixels().any(|p| p[3] > 0));
        let dark = export_diagram(source, true, "PingFang SC", text, true).unwrap();
        assert_ne!(png, dark, "主题应改变渲染像素");
    }

    #[gpui_kit::test]
    fn diagram_wraps_long_chinese_markdown_labels(cx: &mut gpui_kit::TestAppContext) {
        let dimensions = |label: &str| {
            let source = format!("flowchart TD\n A[\"`{label}`\"]");
            let svg = export_diagram(&source, false, "PingFang SC", cx.text_system().clone(), false).unwrap();
            resvg::usvg::Tree::from_data(&svg, &Default::default()).unwrap().size()
        };
        let short = dimensions("中文");
        let wrapped = dimensions(&"中文需要正确折行".repeat(12));
        assert!(wrapped.height() > short.height() * 2., "中文长标签应排成多行: {short:?} / {wrapped:?}");
        assert!(wrapped.width() < 600., "不能把所有中文排成一条超长单行");
    }

    #[gpui_kit::test]
    fn diagram_svg_export_drops_external_image_references(cx: &mut gpui_kit::TestAppContext) {
        for location in ["https://example.invalid/a.png", "/tmp/private-image.png"] {
            let source = format!(
                "%%{{init: {{\"securityLevel\":\"loose\",\"flowchart\":{{\"htmlLabels\":true}}}}}}%%\nflowchart LR\nA@{{img:\"{location}\",h:60}}"
            );
            let svg = export_diagram(&source, false, "Arial", cx.text_system().clone(), false).unwrap();
            let svg = String::from_utf8(svg).unwrap();
            assert!(!svg.contains(location), "导出不能保留外部图片地址: {svg}");
        }
    }

    #[gpui_kit::test]
    fn diagram_rejects_invalid_syntax_without_panicking(cx: &mut gpui_kit::TestAppContext) {
        for source in ["", "not a mermaid diagram", "flowchart TD\n A[unterminated"] {
            let error = export_diagram(source, false, "Arial", cx.text_system().clone(), false).unwrap_err();
            assert!(error.contains("Mermaid"), "{error}");
        }
    }
}
