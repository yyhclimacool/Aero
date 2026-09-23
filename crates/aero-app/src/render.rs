//! 重内容渲染服务（方案 9.9）：图片、数学、图表、PDF 页面都不在 UI 线程渲染，走同一条路。
//!
//! ```text
//! Request { kind, source, width, scale }
//!   → 内存缓存命中？→ 直接返回
//!   → 磁盘缓存 cache/<vault-id>/<kind>/<内容指纹>-w<宽>@<倍率>.png 命中？→ 解码返回
//!   → 后台线程渲染（这里是图片解码 + 等比缩放）→ 写回两级缓存 → 通知块元素重绘
//! ```
//!
//! 内存缓存按请求命中，磁盘缓存按**内容指纹**命中：改名、复制、重装应用都还能复用，文件内容一变就自然失效。
//! 首帧用上次已知的像素尺寸占位，渲染完成后替换，列表不抖动。渲染失败不写文件、不碰笔记，
//! 块元素显示源码 + 一行错误 + 重试。磁盘缓存是可删除的缓存，删掉只会重新渲染。
//!
//! 具体格式藏在 [`Renderer`] 后面：之后的数学（RaTeX）、图表（merman）、PDF（hayro）各加一个
//! [`Kind`] 与实现即可，缓存、占位、失败回退与取消都不用再写一遍。

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use aero_core::{Vault, VaultPath};
use gpui_kit::*;
use image::imageops::FilterType;
use image::{Frame, RgbaImage};

/// 渲染种类：决定用哪个渲染器，也决定磁盘缓存的子目录。
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum Kind {
    Image,
    Math,
}

impl Kind {
    fn dir(self) -> &'static str {
        match self {
            Kind::Image => "images",
            Kind::Math => "math",
        }
    }
}

/// 渲染的"源"：vault 里的文件，或一段现算的源码（公式）。
#[derive(Clone, PartialEq, Eq, Hash, Debug)]
pub enum Source {
    /// 图片等附件：磁盘缓存按文件内容指纹命中
    File(VaultPath),
    /// 数学公式：磁盘缓存按源码 + 行间 / 行内 + 颜色指纹命中
    Formula { latex: String, display: bool, color: u32 },
}

impl Source {
    fn file(&self) -> Option<&VaultPath> {
        match self {
            Source::File(path) => Some(path),
            Source::Formula { .. } => None,
        }
    }
}

impl std::fmt::Display for Source {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Source::File(path) => write!(f, "{path}"),
            Source::Formula { latex, .. } => write!(f, "${latex}$"),
        }
    }
}

/// 一次渲染请求，同时是内存缓存的键。
#[derive(Clone, PartialEq, Eq, Hash, Debug)]
pub struct Request {
    pub kind: Kind,
    pub source: Source,
    /// 图片是目标显示宽度，数学是字号；都按逻辑像素
    pub size: u32,
    /// HiDPI 倍率 ×100；缓存键不用浮点
    pub scale: u32,
}

/// 块元素据此决定画图、画占位，还是画源码 + 错误 + 重试。
#[derive(Clone, Debug)]
pub enum State {
    Ready {
        image: Arc<RenderImage>,
        pixels: Size<u32>,
    },
    /// 已知上次的像素尺寸时按它占位；第一次见到的内容没有尺寸可用
    Pending {
        pixels: Option<Size<u32>>,
    },
    Failed(SharedString),
}

/// 渲染器接口（9.9）：服务负责缓存、占位、失败回退，渲染器只管把源变成位图。
trait Renderer {
    /// `bytes` 是源内容（图片文件的字节 / 公式的 UTF-8），`device` 是算好的设备像素尺寸。
    fn rasterize(&self, request: &Request, bytes: &[u8], device: u32) -> Result<RgbaImage, String>;
}

struct Images;

impl Renderer for Images {
    /// 只等比缩小、不放大：原图比目标窄时保持原尺寸，由元素按自然宽度显示。
    fn rasterize(&self, _: &Request, bytes: &[u8], target: u32) -> Result<RgbaImage, String> {
        if is_svg(bytes) {
            return rasterize_svg(bytes, target);
        }
        let decoded = image::load_from_memory(bytes).map_err(|e| readable(&e.to_string()))?;
        let (width, height) = (decoded.width(), decoded.height());
        if width == 0 || height == 0 {
            return Err("图片尺寸为 0".into());
        }
        if width <= target {
            return Ok(decoded.to_rgba8());
        }
        let scaled = u32::try_from(u64::from(height) * u64::from(target) / u64::from(width))
            .map_err(|_| "图片尺寸超出范围".to_owned())?;
        Ok(decoded.resize_exact(target, scaled.max(1), FilterType::Lanczos3).to_rgba8())
    }
}

/// `image` 的英文错误面向开发者，这里只给用户一句能看懂的。
fn readable(error: &str) -> String {
    if error.contains("not supported") || error.contains("unsupported") {
        format!("暂不支持这种图片格式：{error}")
    } else {
        format!("图片无法解码：{error}")
    }
}

fn is_svg(bytes: &[u8]) -> bool {
    let head = &bytes[..bytes.len().min(512)];
    let text = String::from_utf8_lossy(head).trim_start().to_ascii_lowercase();
    text.starts_with("<svg") || (text.starts_with("<?xml") && text.contains("<svg"))
}

fn rasterize_svg(bytes: &[u8], target: u32) -> Result<RgbaImage, String> {
    let tree = resvg::usvg::Tree::from_data(bytes, &resvg::usvg::Options::default())
        .map_err(|error| format!("SVG 无法解析：{error}"))?;
    let size = tree.size();
    let src_w = size.width().ceil() as u32;
    let src_h = size.height().ceil() as u32;
    if src_w == 0 || src_h == 0 {
        return Err("图片尺寸为 0".into());
    }
    let width = src_w.min(target.max(1));
    let height = u32::try_from(u64::from(src_h) * u64::from(width) / u64::from(src_w))
        .map_err(|_| "图片尺寸超出范围".to_owned())?
        .max(1);
    let mut pixmap = resvg::tiny_skia::Pixmap::new(width, height).ok_or("无法分配位图")?;
    let scale = width as f32 / size.width();
    resvg::render(&tree, resvg::tiny_skia::Transform::from_scale(scale, scale), &mut pixmap.as_mut());
    RgbaImage::from_raw(width, height, pixmap.take()).ok_or_else(|| "SVG 光栅化失败".into())
}

/// 数学公式（方案 6.5）：LaTeX → AST → 排版（带主题色）→ 位图。
/// 颜色在排版阶段给定，所以不需要单色 SVG + alpha 蒙版那条路。
struct Math;

impl Renderer for Math {
    fn rasterize(&self, request: &Request, bytes: &[u8], device: u32) -> Result<RgbaImage, String> {
        use ratex_layout::{LayoutOptions, layout, to_display_list};
        use ratex_types::{Color, MathStyle};

        let Source::Formula { display, color, .. } = &request.source else {
            return Err("数学渲染器只接受公式".into());
        };
        let latex = std::str::from_utf8(bytes).map_err(|_| "公式不是有效的 UTF-8".to_owned())?;
        let ast = ratex_parser::parse(latex).map_err(|e| format!("公式语法错误：{e}"))?;
        let style = if *display { MathStyle::Display } else { MathStyle::Text };
        let [r, g, b, a] = color.to_be_bytes().map(|c| f32::from(c) / 255.);
        let options = LayoutOptions::default().with_style(style).with_color(Color::new(r, g, b, a));
        // 排版单位是 em。正文列宽 720px，超宽的公式按这个宽度折行，而不是画成一条横过去的图。
        // 左右各留一点给渲染 padding，避免折行后的位图仍然超出列宽。
        let max_em = if request.size == 0 { f64::MAX } else { (720.0 - 8.0) / f64::from(request.size) };
        let laid_out = wrap_formula(layout(&ast, &options), max_em);
        let list = to_display_list(&laid_out);
        let png = ratex_render::render_to_png(
            &list,
            &ratex_render::RenderOptions {
                font_size: device as f32,
                padding: 2.,
                // 背景透明：公式贴在正文里，跟着主题走
                background_color: Color::new(0., 0., 0., 0.),
                font_dir: String::new(),
                device_pixel_ratio: 1.,
            },
        )
        .map_err(|e| format!("公式渲染失败：{e}"))?;
        image::load_from_memory(&png).map(|image| image.to_rgba8()).map_err(|e| readable(&e.to_string()))
    }
}

/// 把超宽公式折成若干行。只在水平盒子的子项之间断开，分数、上下标不会被从中切开。
fn wrap_formula(mut laid_out: ratex_layout::LayoutBox, max_em: f64) -> ratex_layout::LayoutBox {
    use ratex_layout::layout_box::{BoxContent, VBoxChild, VBoxChildKind};
    use ratex_layout::vbox::make_vbox;

    if max_em <= 1.0 || laid_out.width <= max_em {
        return laid_out;
    }
    match &mut laid_out.content {
        BoxContent::HBox(_) => {}
        BoxContent::VBox(children) => {
            for child in children.iter_mut() {
                if let VBoxChildKind::Box(inner) = &mut child.kind {
                    let wrapped = wrap_formula(
                        std::mem::replace(inner.as_mut(), ratex_layout::LayoutBox::new_empty()),
                        max_em,
                    );
                    *inner = Box::new(wrapped);
                }
            }
            return make_vbox(std::mem::take(children));
        }
        _ => return laid_out,
    }
    let BoxContent::HBox(children) = laid_out.content else { return laid_out };
    let pieces = children.into_iter().flat_map(|child| expand_wide(child, max_em)).collect::<Vec<_>>();
    let mut lines: Vec<Vec<ratex_layout::LayoutBox>> = Vec::new();
    let mut line: Vec<ratex_layout::LayoutBox> = Vec::new();
    let mut width = 0.0;
    for piece in pieces {
        let glue = matches!(piece.content, BoxContent::Kern);
        if !line.is_empty() && width + piece.width > max_em && !glue {
            lines.push(std::mem::take(&mut line));
            width = 0.0;
        }
        if line.is_empty() && glue {
            continue;
        }
        width += piece.width;
        line.push(piece);
    }
    if !line.is_empty() {
        lines.push(line);
    }
    if lines.len() <= 1 {
        let only = lines.pop().unwrap_or_default();
        return hbox(only, laid_out.color);
    }
    let gap = 0.35;
    let mut stacked = Vec::new();
    for (i, row) in lines.into_iter().enumerate() {
        if i > 0 {
            stacked.push(VBoxChild { kind: VBoxChildKind::Kern(gap), shift: 0.0 });
        }
        stacked.push(VBoxChild { kind: VBoxChildKind::Box(Box::new(hbox(row, laid_out.color))), shift: 0.0 });
    }
    let mut wrapped = make_vbox(stacked);
    wrapped.color = laid_out.color;
    wrapped
}

fn expand_wide(box_: ratex_layout::LayoutBox, max_em: f64) -> Vec<ratex_layout::LayoutBox> {
    use ratex_layout::layout_box::BoxContent;
    if box_.width <= max_em {
        return vec![box_];
    }
    if let BoxContent::HBox(children) = box_.content {
        children.into_iter().flat_map(|child| expand_wide(child, max_em)).collect()
    } else {
        vec![box_]
    }
}

fn hbox(children: Vec<ratex_layout::LayoutBox>, color: ratex_types::Color) -> ratex_layout::LayoutBox {
    let mut width = 0.0_f64;
    let mut height = 0.0_f64;
    let mut depth = 0.0_f64;
    for child in &children {
        width += child.width;
        height = height.max(child.height);
        depth = depth.max(child.depth);
    }
    ratex_layout::LayoutBox {
        width,
        height,
        depth,
        content: ratex_layout::layout_box::BoxContent::HBox(children),
        color,
    }
}

fn renderer(kind: Kind) -> &'static dyn Renderer {
    match kind {
        Kind::Image => &Images,
        Kind::Math => &Math,
    }
}

/// `![[target]]` 指向的附件。目录在后台枚举，加载完成后通知重绘。
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Attachment {
    Loading,
    Found(VaultPath),
    Missing,
    /// 多个同名附件：和补全一样不猜，让用户写限定路径
    Ambiguous,
}

pub struct RenderService {
    vault: Arc<Vault>,
    cache: PathBuf,
    entries: HashMap<Request, Entry>,
    /// 上次渲染出的像素尺寸，占位用；同一源换尺寸时也不用从零等
    pixels: HashMap<Source, Size<u32>>,
    /// 附件目录：与 `![[` 补全同一套解析规则，保证插进去的写法渲染时也能解析
    catalog: Option<aero_core::attachments::Catalog>,
    catalog_task: Option<Task<()>>,
}

enum Entry {
    /// 持有任务句柄：服务被丢掉（关闭 / 切换 vault）时后台渲染随之取消
    Pending(#[allow(dead_code, reason = "句柄只为持有任务，丢弃即取消")] Task<()>),
    Ready {
        image: Arc<RenderImage>,
        pixels: Size<u32>,
    },
    Failed(SharedString),
}

impl RenderService {
    pub fn new(vault: Arc<Vault>, cache: PathBuf) -> Self {
        Self {
            vault,
            cache,
            entries: HashMap::new(),
            pixels: HashMap::new(),
            catalog: None,
            catalog_task: None,
        }
    }

    /// 解析嵌入里的附件目标。首次调用触发后台枚举（整库只走一次，之后按文件变化增量维护）。
    pub fn attachment(&mut self, target: &str, source: &VaultPath, cx: &mut Context<Self>) -> Attachment {
        let Some(catalog) = &self.catalog else {
            if self.catalog_task.is_none() {
                let vault = self.vault.clone();
                self.catalog_task = Some(cx.spawn(async move |this, cx| {
                    let read = cx
                        .background_spawn(async move {
                            aero_core::attachments::Catalog::read(&vault).map_err(|e| e.to_string())
                        })
                        .await;
                    let _ = this.update(cx, |this, cx| {
                        match read {
                            // 枚举失败不留空目录：下次显示再试，不把"找不到"写在脸上
                            Err(error) => log::warn!("attachment catalog: {error}"),
                            Ok(catalog) => this.catalog = Some(catalog),
                        }
                        this.catalog_task = None;
                        cx.notify();
                    });
                }));
            }
            return Attachment::Loading;
        };
        match catalog.resolve(target, source) {
            aero_core::links::Resolution::Resolved { path, .. } => Attachment::Found(path),
            aero_core::links::Resolution::Ambiguous { .. } => Attachment::Ambiguous,
            aero_core::links::Resolution::Unresolved => Attachment::Missing,
        }
    }

    /// 取一张图。没有就排队渲染并先返回占位，渲染完成后 `notify` 触发重绘。
    pub fn image(&mut self, source: VaultPath, width: u32, scale: f32, cx: &mut Context<Self>) -> State {
        let scale = (scale.max(0.5) * 100.).round() as u32;
        let request = Request { kind: Kind::Image, source: Source::File(source), size: width.max(1), scale };
        self.state(request, cx)
    }

    /// 取一条公式的位图。`em` 是字号、`color` 是主题前景色，都进缓存键——主题一换就重新排版。
    pub fn math(
        &mut self,
        latex: &str,
        display: bool,
        em: f32,
        scale: f32,
        color: Hsla,
        cx: &mut Context<Self>,
    ) -> State {
        let rgba = color.to_rgb();
        let channel = |c: f32| (c.clamp(0., 1.) * 255.).round() as u32;
        let packed = (channel(rgba.r) << 24) | (channel(rgba.g) << 16) | (channel(rgba.b) << 8) | 255;
        let source = Source::Formula { latex: latex.to_owned(), display, color: packed };
        let request = Request {
            kind: Kind::Math,
            source,
            size: (em.max(6.).round() as u32).min(200),
            scale: (scale.max(0.5) * 100.).round() as u32,
        };
        self.state(request, cx)
    }

    fn state(&mut self, request: Request, cx: &mut Context<Self>) -> State {
        let placeholder = || State::Pending { pixels: self.pixels.get(&request.source).copied() };
        match self.entries.get(&request) {
            Some(Entry::Ready { image, pixels }) => State::Ready { image: image.clone(), pixels: *pixels },
            Some(Entry::Failed(error)) => State::Failed(error.clone()),
            Some(Entry::Pending(_)) => placeholder(),
            None => {
                let state = placeholder();
                let task = self.render(request.clone(), cx);
                self.entries.insert(request, Entry::Pending(task));
                state
            }
        }
    }

    fn render(&self, request: Request, cx: &mut Context<Self>) -> Task<()> {
        let (vault, cache, key) = (self.vault.clone(), self.cache.clone(), request.clone());
        cx.spawn(async move |this, cx| {
            let produced = cx.background_spawn(async move { produce(&vault, &cache, &key) }).await;
            let _ = this.update(cx, |this, cx| {
                this.entries.insert(
                    request.clone(),
                    match produced {
                        Ok(Produced { frame, pixels, origin }) => {
                            log::debug!(
                                "render {} {}×{} ({origin:?})",
                                request.source,
                                pixels.width,
                                pixels.height
                            );
                            this.pixels.insert(request.source, pixels);
                            Entry::Ready { image: Arc::new(RenderImage::new(vec![frame])), pixels }
                        }
                        Err(error) => {
                            log::debug!("render {}: {error}", request.source);
                            Entry::Failed(error.into())
                        }
                    },
                );
                cx.notify();
            });
        })
    }

    /// 文件被改动 / 删除 / 改名 / 新导入：丢掉相关缓存并增量更新附件目录，下次显示重新渲染。
    /// 磁盘缓存按内容指纹存放，不会把旧图当成新图。失败项一律清掉——新文件可能正好补上了缺的附件。
    pub fn invalidate(&mut self, paths: &[VaultPath], cx: &mut Context<Self>) {
        if paths.is_empty() {
            return;
        }
        self.entries.retain(|request, entry| {
            !request.source.file().is_some_and(|file| paths.contains(file))
                && !matches!(entry, Entry::Failed(_))
        });
        for path in paths {
            self.pixels.remove(&Source::File(path.clone()));
            if let Some(catalog) = &mut self.catalog {
                catalog.update(path, aero_core::attachments::available(&self.vault, path));
            }
        }
        cx.notify();
    }

    /// 失败块上的「重试」：忘掉所有失败记录并重扫附件目录，下一帧重新解析与渲染。
    /// 附件可能是在别处补上的（Finder 里放进去、同步下来），所以重试不限于某一个文件。
    pub fn retry(&mut self, cx: &mut Context<Self>) {
        self.entries.retain(|_, entry| !matches!(entry, Entry::Failed(_)));
        self.catalog = None;
        cx.notify();
    }
}

struct Produced {
    frame: Frame,
    pixels: Size<u32>,
    origin: Origin,
}

#[derive(Debug, PartialEq, Eq)]
enum Origin {
    Disk,
    Rendered,
}

/// 磁盘缓存文件：内容指纹 + 目标宽度 + 倍率。主题相关的渲染（数学）之后在这里加主题段。
fn cache_file(cache: &Path, request: &Request, hash: u64) -> PathBuf {
    cache.join(request.kind.dir()).join(format!("{hash:016x}-w{}@{}.png", request.size, request.scale))
}

/// 磁盘缓存命中就只解码缓存；否则渲染原文件并写回缓存。缓存写失败只影响下次的速度，不影响这次显示。
fn produce(vault: &Vault, cache: &Path, request: &Request) -> Result<Produced, String> {
    let (bytes, hash) = match &request.source {
        Source::File(path) => {
            let (bytes, stamp) = vault.read_bytes(path).map_err(|e| e.to_string())?;
            (bytes, stamp.hash)
        }
        // 公式没有磁盘文件：指纹取源码 + 行间 / 行内 + 颜色
        Source::Formula { latex, display, color } => {
            // `w720` 把折行宽度算进指纹，旧的单行缓存不会被当成现在的多行结果。
            let key = format!("{latex}\u{1}{display}\u{1}{color}\u{1}w720");
            (latex.clone().into_bytes(), aero_core::hash_bytes(key.as_bytes()))
        }
    };
    let file = cache_file(cache, request, hash);
    // 缓存被截断 / 手工改过不能让图打不开：当作未命中重新渲染。
    if let Ok(cached) = std::fs::read(&file)
        && let Ok(rgba) = image::load_from_memory(&cached).map(|image| image.to_rgba8())
    {
        return Ok(produced(rgba, Origin::Disk));
    }
    let device = u32::try_from(u64::from(request.size) * u64::from(request.scale) / 100).unwrap_or(u32::MAX);
    let rgba = renderer(request.kind).rasterize(request, &bytes, device.max(1))?;
    if let Err(error) = write_cache(&file, &rgba) {
        log::warn!("render cache {}: {error}", file.display());
    }
    Ok(produced(rgba, Origin::Rendered))
}

fn produced(rgba: RgbaImage, origin: Origin) -> Produced {
    let pixels = size(rgba.width(), rgba.height());
    let mut rgba = rgba;
    // GPUI 的 RenderImage 吃 BGRA
    for pixel in rgba.as_flat_samples_mut().samples.chunks_exact_mut(4) {
        pixel.swap(0, 2);
    }
    Produced { frame: Frame::new(rgba), pixels, origin }
}

/// 先写临时文件再 rename：并发渲染同一张图不会读到半个 PNG。
fn write_cache(file: &Path, rgba: &RgbaImage) -> Result<(), String> {
    let dir = file.parent().ok_or("缓存路径没有父目录")?;
    std::fs::create_dir_all(dir).map_err(|e| e.to_string())?;
    let mut png = Vec::new();
    image::DynamicImage::ImageRgba8(rgba.clone())
        .write_to(&mut std::io::Cursor::new(&mut png), image::ImageFormat::Png)
        .map_err(|e| e.to_string())?;
    let temp = dir.join(format!(".{}-{}", std::process::id(), file.file_name().unwrap().to_string_lossy()));
    std::fs::write(&temp, &png).map_err(|e| e.to_string())?;
    std::fs::rename(&temp, file).map_err(|e| {
        let _ = std::fs::remove_file(&temp);
        e.to_string()
    })
}

#[cfg(test)]
mod tests {
    // 不能 `use super::*`：它会把 GPUI 的 `test` 属性宏带进作用域，遮住标准 `#[test]`
    use super::{
        Attachment, Images, Kind, Math, Origin, RenderService, Renderer as _, Request, Source, State,
        cache_file, produce, produced,
    };
    use aero_core::{Vault, VaultPath};
    use gpui_kit::{AppContext as _, size};
    use image::RgbaImage;
    use std::sync::Arc;

    /// 纯色 PNG，宽高可控；解码后每个像素都能检查通道顺序。
    fn png(width: u32, height: u32, rgba: [u8; 4]) -> Vec<u8> {
        let image = RgbaImage::from_pixel(width, height, image::Rgba(rgba));
        let mut out = Vec::new();
        image::DynamicImage::ImageRgba8(image)
            .write_to(&mut std::io::Cursor::new(&mut out), image::ImageFormat::Png)
            .unwrap();
        out
    }

    fn vault_with(bytes: &[u8], name: &str) -> (tempfile::TempDir, Vault, VaultPath) {
        let dir = tempfile::tempdir().unwrap();
        let vault = Vault::open(dir.path()).unwrap();
        let path = VaultPath::new(name).unwrap();
        std::fs::create_dir_all(vault.fs_path(&path).parent().unwrap()).unwrap();
        std::fs::write(vault.fs_path(&path), bytes).unwrap();
        (dir, vault, path)
    }

    fn req(source: &VaultPath, size: u32, scale: u32) -> Request {
        Request { kind: Kind::Image, source: Source::File(source.clone()), size, scale }
    }

    fn formula(latex: &str, display: bool, size: u32) -> Request {
        let source = Source::Formula { latex: latex.to_owned(), display, color: 0x1122_33ff };
        Request { kind: Kind::Math, source, size, scale: 100 }
    }

    #[test]
    fn rasterize_scales_down_to_device_pixels_and_never_upscales() {
        let request = req(&VaultPath::new("a.png").unwrap(), 300, 100);
        let wide = Images.rasterize(&request, &png(1000, 400, [1, 2, 3, 255]), 300).unwrap();
        assert_eq!((wide.width(), wide.height()), (300, 120));
        let small = Images.rasterize(&request, &png(80, 40, [1, 2, 3, 255]), 300).unwrap();
        assert_eq!((small.width(), small.height()), (80, 40));
        // 极端比例仍至少 1 像素高，不会变成 0 尺寸纹理
        let strip = Images.rasterize(&request, &png(1000, 1, [1, 2, 3, 255]), 10).unwrap();
        assert_eq!((strip.width(), strip.height()), (10, 1));
    }

    #[test]
    fn rasterize_reports_unreadable_images_in_chinese() {
        let request = req(&VaultPath::new("a.png").unwrap(), 100, 100);
        let error = Images.rasterize(&request, b"not an image at all", 100).unwrap_err();
        assert!(error.starts_with("图片"), "{error}");
        assert!(
            Images.rasterize(&request, &png(4, 4, [0, 0, 0, 0])[..20], 100).is_err(),
            "截断的 PNG 必须报错"
        );
    }

    /// 首次渲染写磁盘缓存，之后只解码缓存；缓存坏了要能自愈。
    #[test]
    fn produce_fills_then_reuses_the_disk_cache_and_recovers_from_a_corrupt_one() {
        let (_dir, vault, path) = vault_with(&png(600, 300, [10, 20, 30, 255]), "note.assets/photo.png");
        let cache = tempfile::tempdir().unwrap();
        let request = req(&path, 200, 200);

        let first = produce(&vault, cache.path(), &request).unwrap();
        assert_eq!(first.origin, Origin::Rendered);
        assert_eq!(first.pixels, size(400, 200));
        let hash = vault.read_bytes(&path).unwrap().1.hash;
        let file = cache_file(cache.path(), &request, hash);
        assert!(file.is_file(), "磁盘缓存应落在 {}", file.display());
        assert_eq!(file.parent().unwrap().file_name().unwrap(), "images");
        assert_eq!(std::fs::read_dir(file.parent().unwrap()).unwrap().count(), 1, "不留临时文件");

        let second = produce(&vault, cache.path(), &request).unwrap();
        assert_eq!(second.origin, Origin::Disk);
        assert_eq!(second.pixels, first.pixels);

        std::fs::write(&file, b"truncated").unwrap();
        let third = produce(&vault, cache.path(), &request).unwrap();
        assert_eq!(third.origin, Origin::Rendered);
        assert_eq!(third.pixels, first.pixels);

        // 不同宽度 / 倍率是不同的缓存项，互不覆盖
        let retina = req(&path, 200, 400);
        assert_eq!(produce(&vault, cache.path(), &retina).unwrap().pixels, size(600, 300));
        assert_eq!(std::fs::read_dir(file.parent().unwrap()).unwrap().count(), 2);
    }

    /// 内容一变指纹就变，旧缓存不会被当成新图。
    #[test]
    fn produce_keys_the_disk_cache_by_content_not_by_path() {
        let (_dir, vault, path) = vault_with(&png(400, 200, [10, 20, 30, 255]), "a.png");
        let cache = tempfile::tempdir().unwrap();
        let request = req(&path, 100, 100);
        assert_eq!(produce(&vault, cache.path(), &request).unwrap().pixels, size(100, 50));
        std::fs::write(vault.fs_path(&path), png(400, 400, [10, 20, 30, 255])).unwrap();
        let after = produce(&vault, cache.path(), &request).unwrap();
        assert_eq!(after.origin, Origin::Rendered);
        assert_eq!(after.pixels, size(100, 100));
        assert_eq!(std::fs::read_dir(cache.path().join("images")).unwrap().count(), 2);
    }

    #[test]
    fn produce_reports_a_missing_file_instead_of_panicking() {
        let (_dir, vault, _) = vault_with(b"x", "a.png");
        let cache = tempfile::tempdir().unwrap();
        let missing = VaultPath::new("gone.png").unwrap();
        assert!(produce(&vault, cache.path(), &req(&missing, 100, 100)).is_err());
    }

    /// BGRA：GPUI 直接把字节丢给 GPU，通道顺序错了就是蓝红互换。
    #[test]
    fn frames_handed_to_gpui_are_bgra() {
        let rgba = RgbaImage::from_pixel(2, 2, image::Rgba([10, 20, 30, 255]));
        let produced = produced(rgba, Origin::Rendered);
        assert_eq!(produced.pixels, size(2, 2));
        assert_eq!(produced.frame.buffer().as_raw()[..4], [30, 20, 10, 255]);
    }

    /// 公式：排版 + 光栅化在同一条流水线上，颜色与行间 / 行内进缓存键，语法错误落到失败表现。
    #[test]
    fn math_rasterizes_formulas_and_reports_syntax_errors() {
        let request = formula("x^2 + \\sqrt{y}", true, 16);
        let rgba = Math.rasterize(&request, b"x^2 + \\sqrt{y}", 32).unwrap();
        assert!(rgba.width() > 8 && rgba.height() > 8, "应有可见的排版结果: {:?}", rgba.dimensions());
        // 背景透明、笔画带上了指定颜色（0x112233）
        assert!(rgba.pixels().any(|p| p.0[3] == 0), "背景应透明");
        let ink = rgba.pixels().filter(|p| p.0[3] > 200).count();
        assert!(ink > 0, "应有不透明的笔画像素");
        // 语法错误不 panic，报中文
        let bad = formula("\\frac{", true, 16);
        let error = Math.rasterize(&bad, b"\\frac{", 32).unwrap_err();
        assert!(error.starts_with("公式"), "{error}");
        // 图片渲染器不接受公式源，反之亦然
        assert!(Math.rasterize(&req(&VaultPath::new("a.png").unwrap(), 10, 100), b"x", 16).is_err());
    }

    /// 比正文列宽更长的公式要折成多行；`\\` 仍然是另起一行，不能被折行逻辑挤回同一行。
    #[test]
    fn long_formula_wraps_to_the_column_and_explicit_breaks_stay_stacked() {
        let piece = "a + b + c + d + e + f + g + h";
        let latex = format!("{piece} + {piece} + {piece} + {piece}");
        let request = formula(&latex, false, 16);
        let rgba = Math.rasterize(&request, latex.as_bytes(), 16).unwrap();
        assert!(rgba.width() <= 720, "折行后应落在列宽内，实际 {}", rgba.width());
        let one = Math.rasterize(&formula("a", false, 16), b"a", 16).unwrap();
        assert!(rgba.height() > one.height() * 2, "长公式应占多行: {} vs {}", rgba.height(), one.height());

        let broken = "a \\\\ b";
        let rows = Math.rasterize(&formula(broken, true, 16), broken.as_bytes(), 16).unwrap();
        let single = Math.rasterize(&formula("a", true, 16), b"a", 16).unwrap();
        assert!(rows.height() > single.height() + 4, "\\\\ 应保留第二行");
    }

    /// 公式没有磁盘文件：磁盘缓存按源码 + 行间 / 行内 + 颜色的指纹命中。
    #[test]
    fn math_disk_cache_is_keyed_by_formula_style_and_color() {
        let (_dir, vault, _) = vault_with(b"unused", "a.png");
        let cache = tempfile::tempdir().unwrap();
        let display = formula("a^2", true, 16);
        let first = produce(&vault, cache.path(), &display).unwrap();
        assert_eq!(first.origin, Origin::Rendered);
        assert_eq!(produce(&vault, cache.path(), &display).unwrap().origin, Origin::Disk);
        // 行内样式、换颜色都是不同的缓存项
        let inline = formula("a^2", false, 16);
        assert_eq!(produce(&vault, cache.path(), &inline).unwrap().origin, Origin::Rendered);
        let Source::Formula { latex, display: d, .. } = &display.source else { unreachable!() };
        let other_color = Request {
            source: Source::Formula { latex: latex.clone(), display: *d, color: 0xffff_ffff },
            ..display.clone()
        };
        assert_eq!(produce(&vault, cache.path(), &other_color).unwrap().origin, Origin::Rendered);
        assert_eq!(std::fs::read_dir(cache.path().join("math")).unwrap().count(), 3);
    }

    #[gpui_kit::test]
    fn service_placeholds_then_caches_and_invalidates_on_change(cx: &mut gpui_kit::TestAppContext) {
        let (_dir, vault, path) = vault_with(&png(800, 400, [9, 9, 9, 255]), "note.assets/photo.png");
        let cache = tempfile::tempdir().unwrap();
        let vault = Arc::new(vault);
        let service = cx.new(|_| RenderService::new(vault.clone(), cache.path().to_path_buf()));

        // 首帧：没有任何尺寸可用的占位
        let first = service.update(cx, |s, cx| s.image(path.clone(), 200, 2., cx));
        assert!(matches!(first, State::Pending { pixels: None }));
        cx.run_until_parked();
        let ready = service.update(cx, |s, cx| s.image(path.clone(), 200, 2., cx));
        let State::Ready { pixels, .. } = ready else { panic!("渲染后应有位图: {ready:?}") };
        assert_eq!(pixels, size(400, 200));

        // 换宽度：新请求要重新渲染，但占位能用已知尺寸，列表不会从零跳
        let other = service.update(cx, |s, cx| s.image(path.clone(), 100, 2., cx));
        assert!(matches!(other, State::Pending { pixels: Some(p) } if p == size(400, 200)));
        cx.run_until_parked();

        // 文件改了：缓存项丢弃，重新渲染出新尺寸
        std::fs::write(vault.fs_path(&path), png(800, 800, [9, 9, 9, 255])).unwrap();
        service.update(cx, |s, cx| s.invalidate(&[path.clone()], cx));
        assert!(matches!(
            service.update(cx, |s, cx| s.image(path.clone(), 200, 2., cx)),
            State::Pending { pixels: None }
        ));
        cx.run_until_parked();
        let ready = service.update(cx, |s, cx| s.image(path.clone(), 200, 2., cx));
        let State::Ready { pixels, .. } = ready else { panic!("改动后应重新渲染: {ready:?}") };
        assert_eq!(pixels, size(400, 400));
    }

    /// 解析走的是补全同一套规则；导入 / 删除只增量维护目录，不重扫整库。
    #[gpui_kit::test]
    fn service_resolves_attachments_like_completion_does(cx: &mut gpui_kit::TestAppContext) {
        let (dir, vault, _) = vault_with(&png(10, 10, [1, 1, 1, 255]), "notes/a.assets/photo.png");
        for folder in ["assets", "other"] {
            std::fs::create_dir_all(dir.path().join(folder)).unwrap();
        }
        std::fs::write(dir.path().join("assets/shared.png"), png(10, 10, [2, 2, 2, 255])).unwrap();
        std::fs::write(dir.path().join("notes/dup.png"), png(10, 10, [3, 3, 3, 255])).unwrap();
        std::fs::write(dir.path().join("other/dup.png"), png(10, 10, [4, 4, 4, 255])).unwrap();
        let note = VaultPath::new("other/note.md").unwrap();
        let cache = tempfile::tempdir().unwrap();
        let service = cx.new(|_| RenderService::new(Arc::new(vault), cache.path().to_path_buf()));

        // 目录还在后台枚举时是"加载中"，不能显示成"找不到附件"
        assert_eq!(service.update(cx, |s, cx| s.attachment("shared.png", &note, cx)), Attachment::Loading);
        cx.run_until_parked();
        let resolve = |cx: &mut gpui_kit::TestAppContext, target: &str, source: &VaultPath| {
            let (target, source) = (target.to_owned(), source.clone());
            service.update(cx, |s, cx| s.attachment(&target, &source, cx))
        };
        let found = |path: &str| Attachment::Found(VaultPath::new(path).unwrap());
        // vault 内唯一文件名（根 assets/ 也是设计允许的附件位置）
        assert_eq!(resolve(cx, "shared.png", &note), found("assets/shared.png"));
        // 本篇 .assets/ 优先
        let owned = VaultPath::new("notes/a.md").unwrap();
        assert_eq!(resolve(cx, "photo.png", &owned), found("notes/a.assets/photo.png"));
        // 笔记同级优先于别处的同名文件；两边都不沾时不猜，让用户写限定路径
        assert_eq!(resolve(cx, "dup.png", &owned), found("notes/dup.png"));
        assert_eq!(resolve(cx, "dup.png", &note), found("other/dup.png"));
        assert_eq!(resolve(cx, "dup.png", &VaultPath::new("third/note.md").unwrap()), Attachment::Ambiguous);
        assert_eq!(resolve(cx, "missing.png", &note), Attachment::Missing);
        assert_eq!(resolve(cx, "a.md", &note), Attachment::Missing, "笔记不是附件");

        // 导入与删除只改一条记录
        let imported = VaultPath::new("other/note.assets/new.png").unwrap();
        std::fs::create_dir_all(dir.path().join("other/note.assets")).unwrap();
        std::fs::write(dir.path().join(imported.as_str()), png(4, 4, [5, 5, 5, 255])).unwrap();
        service.update(cx, |s, cx| s.invalidate(&[imported.clone()], cx));
        assert_eq!(resolve(cx, "new.png", &note), Attachment::Found(imported.clone()));
        std::fs::remove_file(dir.path().join(imported.as_str())).unwrap();
        service.update(cx, |s, cx| s.invalidate(&[imported.clone()], cx));
        assert_eq!(resolve(cx, "new.png", &note), Attachment::Missing);
    }

    #[gpui_kit::test]
    fn service_surfaces_failures_and_retries_on_demand(cx: &mut gpui_kit::TestAppContext) {
        let (_dir, vault, path) = vault_with(b"still not an image", "broken.png");
        let cache = tempfile::tempdir().unwrap();
        let vault = Arc::new(vault);
        let service = cx.new(|_| RenderService::new(vault.clone(), cache.path().to_path_buf()));
        service.update(cx, |s, cx| s.image(path.clone(), 200, 1., cx));
        cx.run_until_parked();
        let failed = service.update(cx, |s, cx| s.image(path.clone(), 200, 1., cx));
        assert!(matches!(&failed, State::Failed(error) if error.starts_with("图片")), "{failed:?}");
        assert!(!cache.path().join("images").exists(), "失败不写缓存");

        // 修好文件后「重试」：不需要重开笔记
        std::fs::write(vault.fs_path(&path), png(100, 50, [1, 1, 1, 255])).unwrap();
        service.update(cx, |s, cx| s.retry(cx));
        service.update(cx, |s, cx| s.image(path.clone(), 200, 1., cx));
        cx.run_until_parked();
        assert!(matches!(
            service.update(cx, |s, cx| s.image(path.clone(), 200, 1., cx)),
            State::Ready { .. }
        ));
    }
}
