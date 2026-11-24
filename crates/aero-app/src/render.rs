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
//! 图片与数学使用 [`Renderer`]；图表（merman）与 PDF（hayro）封装在 `rich` 中，
//! 共用请求、缓存、占位、失败回退与取消流程。

use std::collections::{HashMap, HashSet, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use aero_core::{Vault, VaultPath};
use gpui_kit::*;
use image::imageops::FilterType;
use image::{Frame, RgbaImage};

#[path = "rich_render.rs"]
mod rich;
pub use rich::export_diagram;
#[cfg(test)]
pub(crate) use rich::two_page_pdf;

/// 渲染种类：决定用哪个渲染器，也决定磁盘缓存的子目录。
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum Kind {
    Image,
    Math,
    Diagram,
    Pdf,
}

impl Kind {
    fn dir(self) -> &'static str {
        match self {
            Kind::Image => "images",
            Kind::Math => "math",
            Kind::Diagram => "diagrams",
            Kind::Pdf => "pdf",
        }
    }
}

/// 渲染的"源"：vault 里的文件，或一段现算的源码（公式）。
#[derive(Clone, PartialEq, Eq, Hash, Debug)]
pub enum Source {
    /// 图片等附件：磁盘缓存按文件内容指纹命中
    File(VaultPath),
    /// 数学公式：磁盘缓存按源码 + 行间 / 行内 + 颜色指纹命中
    Formula {
        latex: String,
        display: bool,
        color: u32,
    },
    Diagram {
        source: String,
        dark: bool,
        font: String,
    },
    /// page 从 1 开始，文件失效必须涵盖所有页码。
    Pdf {
        path: VaultPath,
        page: u32,
    },
}

impl Source {
    fn file(&self) -> Option<&VaultPath> {
        match self {
            Source::File(path) | Source::Pdf { path, .. } => Some(path),
            Source::Formula { .. } | Source::Diagram { .. } => None,
        }
    }
}

impl std::fmt::Display for Source {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Source::File(path) => write!(f, "{path}"),
            Source::Formula { .. } => write!(f, "formula"),
            Source::Diagram { .. } => write!(f, "diagram"),
            Source::Pdf { path, page } => write!(f, "{path} 第 {page} 页"),
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
        pages: Option<u32>,
    },
    /// 已知上次的像素尺寸时按它占位；第一次见到的内容没有尺寸可用
    Pending {
        pixels: Option<Size<u32>>,
    },
    Failed(SharedString),
}

/// 所有页面的逻辑渲染尺寸（含旋转），不依赖位图宽度或设备倍率。
#[derive(Clone, Debug)]
pub struct PdfInfo {
    pub pages: Vec<Size<f32>>,
    pub fingerprint: u64,
}

#[derive(Clone, Debug)]
pub enum PdfInfoState {
    Pending,
    Ready(Arc<PdfInfo>),
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
                    **inner = wrapped;
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

/// `![[target]]` 指向的附件。目录在后台枚举，加载完成后通知重绘。
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Attachment {
    Loading,
    Found(VaultPath),
    Missing,
    /// 多个同名附件：和补全一样不猜，让用户写限定路径
    Ambiguous,
}

const PDF_PAGE_CACHE_LIMIT: usize = 12;
const NON_PDF_CACHE_LIMIT: usize = 64;
const RENDER_CACHE_BYTES: u64 = 128 * 1024 * 1024;
const RENDER_CONCURRENCY: usize = 2;
const RENDER_QUEUE_LIMIT: usize = 32;

pub struct RenderService {
    vault: Arc<Vault>,
    cache: PathBuf,
    entries: HashMap<Request, Entry>,
    /// 元数据只按 vault 相对路径缓存，不随页面位图 LRU 淘汰。
    pdf_infos: HashMap<VaultPath, PdfInfoEntry>,
    access: HashMap<Request, u64>,
    tick: u64,
    eviction_scheduled: bool,
    active_since: u64,
    /// 队列只保留最近请求；溢出的可见项保留轻量 Waiting 占位，不创建 future。
    queue: VecDeque<Request>,
    /// 同步解码开始后无法中断：即使 entry 被淘汰，仍持有任务直到完成才释放并发名额。
    running: HashMap<u64, Task<()>>,
    next_token: u64,
    /// 上次渲染出的像素尺寸，占位用；同一源换尺寸时也不用从零等
    pixels: HashMap<Source, Size<u32>>,
    /// 附件目录：与 `![[` 补全同一套解析规则，保证插进去的写法渲染时也能解析
    catalog: Option<aero_core::attachments::Catalog>,
    catalog_task: Option<Task<()>>,
}

enum PdfInfoEntry {
    Pending { token: u64, _task: Task<()> },
    Ready(Arc<PdfInfo>),
    Failed(SharedString),
}

enum Entry {
    Waiting,
    Pending { token: u64 },
    Ready { image: Arc<RenderImage>, pixels: Size<u32>, pages: Option<u32> },
    Failed(SharedString),
}

impl RenderService {
    pub fn new(vault: Arc<Vault>, cache: PathBuf) -> Self {
        Self {
            vault,
            cache,
            entries: HashMap::new(),
            pdf_infos: HashMap::new(),
            access: HashMap::new(),
            tick: 0,
            eviction_scheduled: false,
            active_since: 0,
            queue: VecDeque::new(),
            running: HashMap::new(),
            next_token: 0,
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
        // 向上取 64 逻辑像素一档；窗口逐像素缩放复用位图，HiDPI 下也不会低于请求分辨率。
        let width = width.max(1).div_ceil(64).saturating_mul(64);
        let request = Request { kind: Kind::Image, source: Source::File(source), size: width, scale };
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

    /// 源码、主题、字体变化都是新请求；解析、测量和光栅化只在后台执行。
    pub fn diagram(
        &mut self,
        source: &str,
        width: u32,
        scale: f32,
        dark: bool,
        font: &str,
        cx: &mut Context<Self>,
    ) -> State {
        let request = Request {
            kind: Kind::Diagram,
            source: Source::Diagram { source: source.to_owned(), dark, font: font.to_owned() },
            size: width.max(1),
            scale: (scale.max(0.5) * 100.).round() as u32,
        };
        self.state(request, cx)
    }

    /// page 为 1-based；非法页码在后台返回错误，绝不改成另一页。
    pub fn pdf(
        &mut self,
        path: VaultPath,
        page: u32,
        width: u32,
        scale: f32,
        cx: &mut Context<Self>,
    ) -> State {
        let request = Request {
            kind: Kind::Pdf,
            source: Source::Pdf { path, page },
            size: width.max(1),
            scale: (scale.max(0.5) * 100.).round() as u32,
        };
        self.state(request, cx)
    }

    /// 首次仅排队；读取、指纹与全页尺寸解析都在后台完成，不触发页面光栅化。
    pub fn pdf_info(&mut self, path: VaultPath, cx: &mut Context<Self>) -> PdfInfoState {
        match self.pdf_infos.get(&path) {
            Some(PdfInfoEntry::Pending { .. }) => PdfInfoState::Pending,
            Some(PdfInfoEntry::Ready(info)) => PdfInfoState::Ready(info.clone()),
            Some(PdfInfoEntry::Failed(error)) => PdfInfoState::Failed(error.clone()),
            None => {
                let token = self.token();
                let (vault, key) = (self.vault.clone(), path.clone());
                let task = cx.spawn(async move |this, cx| {
                    let read_path = key.clone();
                    let result = cx
                        .background_spawn(async move {
                            let (bytes, stamp) =
                                vault.read_bytes(&read_path).map_err(|e| format!("PDF 无法读取：{e}"))?;
                            Ok::<_, String>(PdfInfo {
                                pages: rich::pdf_dimensions(bytes)?,
                                fingerprint: stamp.hash,
                            })
                        })
                        .await;
                    let _ = this.update(cx, |this, cx| {
                        if !matches!(this.pdf_infos.get(&key), Some(PdfInfoEntry::Pending { token: current, .. }) if *current == token) {
                            return;
                        }
                        this.pdf_infos.insert(
                            key,
                            match result {
                                Ok(info) => PdfInfoEntry::Ready(Arc::new(info)),
                                Err(error) => PdfInfoEntry::Failed(error.into()),
                            },
                        );
                        cx.notify();
                    });
                });
                self.pdf_infos.insert(path, PdfInfoEntry::Pending { token, _task: task });
                PdfInfoState::Pending
            }
        }
    }

    fn token(&mut self) -> u64 {
        self.next_token = self.next_token.checked_add(1).expect("渲染任务 token 耗尽");
        self.next_token
    }

    fn state(&mut self, request: Request, cx: &mut Context<Self>) -> State {
        self.tick = self.tick.checked_add(1).expect("渲染访问序号耗尽");
        self.access.insert(request.clone(), self.tick);
        if !self.eviction_scheduled {
            self.eviction_scheduled = true;
            let active_since = self.tick;
            self.active_since = active_since;
            let service = cx.weak_entity();
            // 收齐同一轮绘制再淘汰 / 派发，不能让可见项互相淘汰而永久重排。
            cx.defer(move |cx| {
                let _ = service.update(cx, |this, cx| {
                    this.eviction_scheduled = false;
                    this.evict(active_since);
                    this.dispatch(cx);
                });
            });
        }
        let placeholder = || State::Pending { pixels: self.pixels.get(&request.source).copied() };
        match self.entries.get(&request) {
            Some(Entry::Ready { image, pixels, pages }) => {
                State::Ready { image: image.clone(), pixels: *pixels, pages: *pages }
            }
            Some(Entry::Failed(error)) => State::Failed(error.clone()),
            Some(Entry::Pending { .. } | Entry::Waiting) => placeholder(),
            None => {
                let state = placeholder();
                self.entries.insert(request, Entry::Waiting);
                state
            }
        }
    }

    fn evict(&mut self, active_since: u64) {
        let active_images = self
            .access
            .iter()
            .filter(|(key, tick)| key.kind == Kind::Image && **tick >= active_since)
            .map(|(key, _)| &key.source)
            .collect::<HashSet<_>>();
        // 连续缩放时只取消不再显示的旧尺寸；同帧不同位置显示的多个尺寸仍受保护。
        let superseded = self
            .entries
            .iter()
            .filter(|(key, entry)| {
                key.kind == Kind::Image
                    && self.access[*key] < active_since
                    && active_images.contains(&key.source)
                    && matches!(entry, Entry::Waiting | Entry::Pending { .. })
            })
            .map(|(key, _)| key.clone())
            .collect::<Vec<_>>();
        for key in superseded {
            self.remove_entry(&key);
        }
        for (pdf, limit) in [(true, PDF_PAGE_CACHE_LIMIT), (false, NON_PDF_CACHE_LIMIT)] {
            let count = self.access.keys().filter(|key| (key.kind == Kind::Pdf) == pdf).count();
            let mut inactive = self
                .access
                .iter()
                .filter(|(key, tick)| (key.kind == Kind::Pdf) == pdf && **tick < active_since)
                .map(|(key, tick)| (key.clone(), *tick))
                .collect::<Vec<_>>();
            inactive.sort_unstable_by_key(|(_, tick)| *tick);
            for (key, _) in inactive.into_iter().take(count.saturating_sub(limit)) {
                self.remove_entry(&key);
            }
        }
        // All bitmap kinds share the byte budget. Current-frame resources stay alive:
        // evicting those would create a render/evict loop when many images are visible.
        let bytes = |entry: &Entry| match entry {
            Entry::Ready { pixels, .. } => u64::from(pixels.width) * u64::from(pixels.height) * 4,
            _ => 0,
        };
        let mut total: u64 = self.entries.values().map(bytes).sum();
        let mut inactive = self
            .access
            .iter()
            .filter(|(_, tick)| **tick < active_since)
            .map(|(key, tick)| (key.clone(), *tick))
            .collect::<Vec<_>>();
        inactive.sort_unstable_by_key(|(_, tick)| *tick);
        for (key, _) in inactive {
            if total <= RENDER_CACHE_BYTES {
                break;
            }
            total -= self.entries.get(&key).map_or(0, bytes);
            self.remove_entry(&key);
        }
    }

    fn remove_entry(&mut self, key: &Request) {
        self.entries.remove(key);
        self.access.remove(key);
        if !self.entries.keys().any(|other| other.source == key.source) {
            self.pixels.remove(&key.source);
        }
    }

    fn dispatch(&mut self, cx: &mut Context<Self>) {
        // 未派发的旧队列可直接重建：最新 / 再次请求的内容优先，不积累后台 future。
        let mut waiting = self
            .entries
            .iter()
            .filter(|(_, entry)| matches!(entry, Entry::Waiting))
            .map(|(key, _)| (key.clone(), self.access[key]))
            .collect::<Vec<_>>();
        waiting.sort_unstable_by_key(|(_, tick)| std::cmp::Reverse(*tick));
        self.queue = waiting.into_iter().take(RENDER_QUEUE_LIMIT).map(|(key, _)| key).collect();
        while self.running.len() < RENDER_CONCURRENCY {
            let Some(request) = self.queue.pop_front() else { break };
            let token = self.token();
            self.entries.insert(request.clone(), Entry::Pending { token });
            let task = self.render(request, token, cx);
            self.running.insert(token, task);
        }
    }

    fn render(&self, request: Request, token: u64, cx: &mut Context<Self>) -> Task<()> {
        let (vault, cache, key) = (self.vault.clone(), self.cache.clone(), request.clone());
        let text_system = cx.text_system().clone();
        let operation = crate::diagnostics::Operation::new(
            match request.kind {
                Kind::Image => "render_image",
                Kind::Math => "render_math",
                Kind::Diagram => "render_diagram",
                Kind::Pdf => "render_pdf",
            },
            crate::diagnostics::Facts { count: self.running.len() as u64, ..Default::default() },
        );
        let queued = std::time::Instant::now();
        cx.spawn(async move |this, cx| {
            let worker_operation = operation.clone();
            let produced = cx
                .background_spawn(async move {
                    worker_operation.stage("running", crate::diagnostics::Facts { queue_us: queued.elapsed().as_micros() as u64, ..Default::default() });
                    let result = worker_operation.scope(|| produce_with_text(&vault, &cache, &key, Some(text_system)));
                    worker_operation.stage("produced", Default::default());
                    result
                })
                .await;
            let _ = this.update(cx, |this, cx| {
                this.running.remove(&token);
                // 淘汰 / 失效后的回调不能复活 entry，也不能覆盖同 key 的新任务。
                if !matches!(this.entries.get(&request), Some(Entry::Pending { token: current }) if *current == token) {
                    operation.finish(crate::diagnostics::Outcome::Stale);
                    this.dispatch(cx);
                    return;
                }
                this.entries.insert(
                    request.clone(),
                    match produced {
                        Ok(Produced { frame, pixels, origin, pages }) => {
                            operation.stage(match origin { Origin::Disk => "disk_hit", Origin::Rendered => "rendered" }, Default::default());
                            operation.finish(crate::diagnostics::Outcome::Ok);
                            log::debug!(
                                "render {} {}×{} ({origin:?})",
                                request.source,
                                pixels.width,
                                pixels.height
                            );
                            this.pixels.insert(request.source, pixels);
                            Entry::Ready { image: Arc::new(RenderImage::new(vec![frame])), pixels, pages }
                        }
                        Err(error) => {
                            operation.finish(crate::diagnostics::Outcome::Failed);
                            log::debug!("render {}: {error}", request.source);
                            Entry::Failed(error.into())
                        }
                    },
                );
                this.evict(this.active_since);
                this.dispatch(cx);
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
        self.pdf_infos
            .retain(|path, entry| !paths.contains(path) && !matches!(entry, PdfInfoEntry::Failed(_)));
        self.access.retain(|request, _| self.entries.contains_key(request));
        self.queue.retain(|request| self.entries.contains_key(request));
        self.pixels.retain(|source, _| !source.file().is_some_and(|file| paths.contains(file)));
        for path in paths {
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
        self.pdf_infos.clear();
        self.access.retain(|request, _| self.entries.contains_key(request));
        self.queue.retain(|request| self.entries.contains_key(request));
        self.catalog = None;
        cx.notify();
    }
}

struct Produced {
    frame: Frame,
    pixels: Size<u32>,
    origin: Origin,
    pages: Option<u32>,
}

#[derive(Debug, PartialEq, Eq)]
enum Origin {
    Disk,
    Rendered,
}

/// 磁盘缓存文件：源内容指纹（含主题、字体、页码、渲染版本）+ 目标宽度 + 倍率。
fn cache_file(cache: &Path, request: &Request, hash: u64) -> PathBuf {
    cache.join(request.kind.dir()).join(format!("{hash:016x}-w{}@{}.png", request.size, request.scale))
}

#[cfg(test)]
fn produce(vault: &Vault, cache: &Path, request: &Request) -> Result<Produced, String> {
    produce_with_text(vault, cache, request, None)
}

/// 磁盘命中解码缓存；PDF 仍校验元数据以可靠返回总页数。所有 I/O 和渲染都由后台调用。
fn produce_with_text(
    vault: &Vault,
    cache: &Path,
    request: &Request,
    text: Option<Arc<TextSystem>>,
) -> Result<Produced, String> {
    let (mut bytes, hash) = match &request.source {
        Source::File(path) => {
            let (bytes, stamp) = vault.read_bytes(path).map_err(|e| e.to_string())?;
            (bytes, stamp.hash)
        }
        Source::Pdf { path, page } => {
            let (bytes, stamp) = vault.read_bytes(path).map_err(|e| format!("PDF 无法读取：{e}"))?;
            let key =
                serde_json::to_vec(&(rich::PDF_VERSION, stamp.hash, page)).map_err(|e| e.to_string())?;
            (bytes, aero_core::hash_bytes(&key))
        }
        Source::Diagram { source, dark, font } => {
            let key = serde_json::to_vec(&(rich::DIAGRAM_VERSION, source, dark, font))
                .map_err(|e| e.to_string())?;
            (source.as_bytes().to_vec(), aero_core::hash_bytes(&key))
        }
        // 公式没有磁盘文件：指纹取源码 + 行间 / 行内 + 颜色
        Source::Formula { latex, display, color } => {
            // `w720` 把折行宽度算进指纹，旧的单行缓存不会被当成现在的多行结果。
            let key = format!("{latex}\u{1}{display}\u{1}{color}\u{1}w720");
            (latex.clone().into_bytes(), aero_core::hash_bytes(key.as_bytes()))
        }
    };
    let device =
        u32::try_from(u64::from(request.size) * u64::from(request.scale) / 100).unwrap_or(u32::MAX).max(1);
    let pdf = if let Source::Pdf { page, .. } = &request.source {
        // PDF 解析器拥有源字节；其它渲染器不会再用到它，避免整份文档再复制一次。
        Some(rich::PdfPage::new(std::mem::take(&mut bytes), *page, device)?)
    } else {
        None
    };
    let pages = pdf.as_ref().map(|pdf| pdf.pages);
    let file = cache_file(cache, request, hash);
    // 缓存被截断 / 手工改过不能让图打不开：当作未命中重新渲染。
    if let Ok(cached) = std::fs::read(&file)
        && let Ok(rgba) = image::load_from_memory(&cached).map(|image| image.to_rgba8())
        && pdf.as_ref().is_none_or(|pdf| rgba.dimensions() == pdf.size)
    {
        return Ok(Produced { pages, ..produced(rgba, Origin::Disk) });
    }
    let rgba = match (&request.kind, &request.source) {
        (Kind::Image, _) => Images.rasterize(request, &bytes, device)?,
        (Kind::Math, _) => Math.rasterize(request, &bytes, device)?,
        (Kind::Diagram, Source::Diagram { source, dark, font }) => {
            rich::diagram(source, *dark, font, text.ok_or("Mermaid 缺少宿主文本系统")?, device)?
        }
        (Kind::Pdf, Source::Pdf { .. }) => pdf.as_ref().ok_or("PDF 页面不可用")?.rasterize()?,
        _ => return Err("渲染种类与源内容不匹配".into()),
    };
    if let Err(error) = write_cache(&file, &rgba) {
        log::warn!("render cache {}: {error}", file.display());
    }
    Ok(Produced { pages, ..produced(rgba, Origin::Rendered) })
}

fn produced(rgba: RgbaImage, origin: Origin) -> Produced {
    let pixels = size(rgba.width(), rgba.height());
    let mut rgba = rgba;
    // GPUI 的 RenderImage 吃 BGRA
    for pixel in rgba.as_flat_samples_mut().samples.as_chunks_mut::<4>().0 {
        pixel.swap(0, 2);
    }
    Produced { frame: Frame::new(rgba), pixels, origin, pages: None }
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
        Attachment, Images, Kind, Math, Origin, PdfInfo, PdfInfoState, RenderService, Renderer as _, Request,
        Source, State, cache_file, produce, produced,
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
        assert_eq!(pixels, size(512, 256));

        // 换宽度档位：新请求要重新渲染，但占位能用已知尺寸，列表不会从零跳
        let other = service.update(cx, |s, cx| s.image(path.clone(), 100, 2., cx));
        assert!(matches!(other, State::Pending { pixels: Some(p) } if p == size(512, 256)));
        cx.run_until_parked();

        // 文件改了：缓存项丢弃，重新渲染出新尺寸
        std::fs::write(vault.fs_path(&path), png(800, 800, [9, 9, 9, 255])).unwrap();
        service.update(cx, |s, cx| s.invalidate(std::slice::from_ref(&path), cx));
        assert!(matches!(
            service.update(cx, |s, cx| s.image(path.clone(), 200, 2., cx)),
            State::Pending { pixels: None }
        ));
        cx.run_until_parked();
        let ready = service.update(cx, |s, cx| s.image(path.clone(), 200, 2., cx));
        let State::Ready { pixels, .. } = ready else { panic!("改动后应重新渲染: {ready:?}") };
        assert_eq!(pixels, size(512, 512));
    }

    #[gpui_kit::test]
    fn service_render_image_widths_reuse_an_upward_bucket(cx: &mut gpui_kit::TestAppContext) {
        let (_dir, vault, path) = vault_with(&png(1000, 500, [1, 2, 3, 255]), "photo.png");
        let cache = tempfile::tempdir().unwrap();
        let service = cx.new(|_| RenderService::new(Arc::new(vault), cache.path().to_path_buf()));
        service.update(cx, |s, cx| s.image(path.clone(), 257, 2., cx));
        cx.run_until_parked();
        let State::Ready { image, pixels, .. } =
            service.update(cx, |s, cx| s.image(path.clone(), 257, 2., cx))
        else {
            panic!("first image must finish");
        };
        for width in 258..=319 {
            let state = service.update(cx, |s, cx| s.image(path.clone(), width, 2., cx));
            let State::Ready { image: cached, .. } = state else {
                panic!("nearby widths should not enqueue another decode: {width}");
            };
            assert!(Arc::ptr_eq(&image, &cached));
            assert!(pixels.width >= width * 2, "bucket must not undersample the display");
        }
        service.update(cx, |s, _| assert_eq!(s.entries.len(), 1));
        assert_eq!(std::fs::read_dir(cache.path().join("images")).unwrap().count(), 1);
    }

    fn image_paths(vault: &Vault, count: usize) -> Vec<VaultPath> {
        let bytes = png(8, 4, [1, 2, 3, 255]);
        (0..count)
            .map(|i| {
                let path = VaultPath::new(&format!("image-{i}.png")).unwrap();
                vault.create_bytes(&path, &bytes).unwrap();
                path
            })
            .collect()
    }

    #[gpui_kit::test]
    fn service_render_non_pdf_lru_bounds_entries_and_pixels(cx: &mut gpui_kit::TestAppContext) {
        let (_dir, vault, _) = vault_with(b"unused", "a.txt");
        let paths = image_paths(&vault, 80);
        let cache = tempfile::tempdir().unwrap();
        let vault = Arc::new(vault);
        for drain in [false, true] {
            let service = cx.new(|_| RenderService::new(vault.clone(), cache.path().to_path_buf()));
            for path in &paths[..64] {
                service.update(cx, |s, cx| s.image(path.clone(), 128, 1., cx));
                if drain {
                    cx.run_until_parked();
                }
            }
            // A hit (including a pending hit) must refresh recency.
            service.update(cx, |s, cx| s.image(paths[0].clone(), 128, 1., cx));
            for path in &paths[64..] {
                service.update(cx, |s, cx| s.image(path.clone(), 128, 1., cx));
                service.update(cx, |s, _| {
                    assert!(s.entries.len() <= 64, "non-PDF cache must have a finite budget");
                    assert!(s.entries.contains_key(&req(&paths[0], 128, 100)));
                    assert!(s.entries.contains_key(&req(path, 128, 100)));
                    assert!(!s.entries.contains_key(&req(&paths[1], 128, 100)));
                    assert!(!s.pixels.contains_key(&Source::File(paths[1].clone())));
                });
                if drain {
                    cx.run_until_parked();
                }
            }
            cx.run_until_parked();
            service.update(cx, |s, _| {
                assert_eq!(s.entries.len(), 64);
                assert_eq!(s.pixels.len(), 64);
            });
        }
    }

    #[gpui_kit::test]
    fn service_render_byte_budget_is_shared_and_preserves_visible_images(cx: &mut gpui_kit::TestAppContext) {
        let (_dir, vault, path) = vault_with(&png(1, 1, [1, 2, 3, 255]), "photo.png");
        let cache = tempfile::tempdir().unwrap();
        let service = cx.new(|_| RenderService::new(Arc::new(vault), cache.path().to_path_buf()));
        service.update(cx, |s, _| {
            let keys = [req(&path, 64, 100), req(&path, 128, 100), pdf_request(&path, 1, 100)];
            for (i, key) in keys.iter().enumerate() {
                s.entries.insert(
                    key.clone(),
                    super::Entry::Ready {
                        // Synthetic dimensions exercise accounting without allocating 192 MiB in the test.
                        image: Arc::new(gpui_kit::RenderImage::new(vec![])),
                        pixels: size(4096, 4096),
                        pages: None,
                    },
                );
                s.access.insert(key.clone(), i as u64 + 1);
            }
            s.evict(1);
            assert_eq!(s.entries.len(), 3, "visible resources cannot evict one another");
            s.evict(3);
            assert!(!s.entries.contains_key(&keys[0]), "evict the oldest inactive bitmap by bytes");
            assert!(s.entries.contains_key(&keys[1]));
            assert!(s.entries.contains_key(&keys[2]));
        });
    }

    #[gpui_kit::test]
    fn service_render_protects_all_current_frame_images(cx: &mut gpui_kit::TestAppContext) {
        let (_dir, vault, _) = vault_with(b"unused", "a.txt");
        let paths = image_paths(&vault, 80);
        let cache = tempfile::tempdir().unwrap();
        let service = cx.new(|_| RenderService::new(Arc::new(vault), cache.path().to_path_buf()));
        service.update(cx, |s, cx| {
            for path in &paths {
                s.image(path.clone(), 128, 1., cx);
            }
        });
        cx.run_until_parked();
        for _ in 0..3 {
            service.update(cx, |s, cx| {
                for path in &paths {
                    assert!(
                        matches!(s.image(path.clone(), 128, 1., cx), State::Ready { .. }),
                        "visible images must not evict one another"
                    );
                }
            });
            cx.run_until_parked();
        }
        service.update(cx, |s, cx| s.image(paths[79].clone(), 128, 1., cx));
        service.update(cx, |s, _| {
            assert_eq!(s.entries.len(), 64);
            assert_eq!(s.pixels.len(), 64);
        });
    }

    #[gpui_kit::test]
    fn service_render_bounds_expensive_jobs_during_a_request_storm(cx: &mut gpui_kit::TestAppContext) {
        let (_dir, vault, _) = vault_with(b"unused", "a.txt");
        let paths = image_paths(&vault, 100);
        let cache = tempfile::tempdir().unwrap();
        let service = cx.new(|_| RenderService::new(Arc::new(vault), cache.path().to_path_buf()));
        for path in &paths {
            service.update(cx, |s, cx| s.image(path.clone(), 128, 1., cx));
            service.update(cx, |s, _| {
                let jobs =
                    s.entries.values().filter(|entry| matches!(entry, super::Entry::Pending { .. })).count();
                assert!(jobs <= 2, "request storms must not spawn one task per image: {jobs}");
                assert!(s.running.len() <= 2);
                assert!(s.queue.len() <= 32);
            });
        }
        cx.run_until_parked();
        assert!(matches!(
            service.update(cx, |s, cx| s.image(paths[99].clone(), 128, 1., cx)),
            State::Ready { .. }
        ));
    }

    #[gpui_kit::test]
    fn service_render_prioritizes_latest_width_before_decoding_backlog(cx: &mut gpui_kit::TestAppContext) {
        let (_dir, vault, path) = vault_with(&png(8, 4, [1, 2, 3, 255]), "photo.png");
        let hash = vault.read_bytes(&path).unwrap().1.hash;
        let cache = tempfile::tempdir().unwrap();
        let service = cx.new(|_| RenderService::new(Arc::new(vault), cache.path().to_path_buf()));
        service.update(cx, |s, cx| {
            for width in (128..=12800).step_by(128) {
                s.image(path.clone(), width, 1., cx);
            }
        });
        park_background_reply(cx);
        let files = std::fs::read_dir(cache.path().join("images")).unwrap().count();
        assert!(files <= 2, "only the admitted jobs may perform I/O: {files}");
        assert!(
            cache_file(cache.path(), &req(&path, 12800, 100), hash).is_file(),
            "the latest width must be admitted before the old backlog"
        );
        cx.run_until_parked();
    }

    #[gpui_kit::test]
    fn service_render_skips_superseded_widths_without_releasing_busy_slots(
        cx: &mut gpui_kit::TestAppContext,
    ) {
        let (_dir, vault, path) = vault_with(&png(8, 4, [1, 2, 3, 255]), "photo.png");
        let hash = vault.read_bytes(&path).unwrap().1.hash;
        let cache = tempfile::tempdir().unwrap();
        let service = cx.new(|_| RenderService::new(Arc::new(vault), cache.path().to_path_buf()));
        for width in (128..=12800).step_by(128) {
            service.update(cx, |s, cx| s.image(path.clone(), width, 1., cx));
            service.update(cx, |s, _| {
                assert!(s.running.len() <= 2, "obsolete synchronous jobs still occupy their slots");
                assert!(s.queue.len() <= 32);
            });
        }
        cx.run_until_parked();
        assert!(cache_file(cache.path(), &req(&path, 12800, 100), hash).is_file());
        assert!(
            std::fs::read_dir(cache.path().join("images")).unwrap().count() <= 3,
            "only the two started jobs and the latest width should decode"
        );
        service.update(cx, |s, _| {
            assert_eq!(s.entries.len(), 1);
            assert!(s.running.is_empty() && s.queue.is_empty());
        });
    }

    #[gpui_kit::test]
    fn service_render_invalidation_keeps_running_jobs_accounted_for(cx: &mut gpui_kit::TestAppContext) {
        let (_dir, vault, path) = vault_with(&png(8, 4, [1, 2, 3, 255]), "photo.png");
        let cache = tempfile::tempdir().unwrap();
        let service = cx.new(|_| RenderService::new(Arc::new(vault), cache.path().to_path_buf()));
        for _ in 0..100 {
            service.update(cx, |s, cx| {
                s.invalidate(std::slice::from_ref(&path), cx);
                s.image(path.clone(), 128, 1., cx);
            });
            service.update(cx, |s, _| {
                assert!(s.running.len() <= 2);
                assert!(s.next_token <= 2, "invalidation must not admit replacement work early");
                assert!(s.queue.len() <= 1);
            });
        }
        cx.run_until_parked();
        assert!(matches!(service.update(cx, |s, cx| s.image(path, 128, 1., cx)), State::Ready { .. }));
        service.update(cx, |s, _| assert!(s.running.is_empty() && s.queue.is_empty()));
    }

    #[gpui_kit::test]
    fn service_render_budget_includes_math_diagrams_and_failures(cx: &mut gpui_kit::TestAppContext) {
        let (_dir, vault, _) = vault_with(b"unused", "a.txt");
        let paths = image_paths(&vault, 80);
        let cache = tempfile::tempdir().unwrap();
        let service = cx.new(|_| RenderService::new(Arc::new(vault), cache.path().to_path_buf()));
        for (i, path) in paths.iter().enumerate() {
            let request = match i % 3 {
                0 => req(path, 128, 100),
                1 => formula(&format!("\\frac{{{i}"), false, 16),
                _ => diagram_request(&format!("invalid mermaid {i}"), false, "Arial", 100),
            };
            service.update(cx, |s, cx| s.state(request, cx));
            cx.run_until_parked();
            service.update(cx, |s, _| assert!(s.entries.len() <= 64));
        }
        service.update(cx, |s, _| {
            assert_eq!(s.entries.len(), 64);
            for kind in [Kind::Image, Kind::Math, Kind::Diagram] {
                assert!(s.entries.keys().any(|key| key.kind == kind));
            }
            assert!(s.entries.values().any(|entry| matches!(entry, super::Entry::Failed(_))));
            assert!(s.pixels.keys().all(|source| s.entries.keys().any(|key| &key.source == source)));
        });
    }

    #[gpui_kit::test]
    fn service_render_evicted_reply_cannot_revive_or_replace_an_image(cx: &mut gpui_kit::TestAppContext) {
        let (_dir, vault, _) = vault_with(b"unused", "a.txt");
        let paths = image_paths(&vault, 65);
        let cache = tempfile::tempdir().unwrap();
        let vault = Arc::new(vault);
        for rerequest in [false, true] {
            let service = cx.new(|_| RenderService::new(vault.clone(), cache.path().to_path_buf()));
            let request = req(&paths[0], 128, 100);
            service.update(cx, |s, cx| s.image(paths[0].clone(), 128, 1., cx));
            park_background_reply(cx);
            let obsolete = service.update(cx, |s, _| {
                let super::Entry::Pending { token } = s.entries.get(&request).unwrap() else { panic!() };
                std::mem::replace(s.running.get_mut(token).unwrap(), gpui_kit::Task::ready(()))
            });
            service.update(cx, |s, cx| {
                for path in &paths[1..] {
                    s.image(path.clone(), 128, 1., cx);
                }
            });
            service.update(cx, |s, _| assert!(!s.entries.contains_key(&request)));
            if rerequest {
                std::fs::write(vault.fs_path(&paths[0]), png(8, 8, [9, 9, 9, 255])).unwrap();
                service.update(cx, |s, cx| s.image(paths[0].clone(), 128, 1., cx));
            }
            finish_obsolete(&obsolete, cx);
            service.update(cx, |s, _| {
                if rerequest {
                    assert!(matches!(s.entries.get(&request), Some(super::Entry::Pending { .. } | super::Entry::Waiting))
                        || matches!(s.entries.get(&request), Some(super::Entry::Ready { pixels, .. }) if *pixels == size(8, 8)));
                } else {
                    assert!(!s.entries.contains_key(&request));
                    assert!(!s.pixels.contains_key(&request.source));
                }
            });
            cx.run_until_parked();
        }
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
        service.update(cx, |s, cx| s.invalidate(std::slice::from_ref(&imported), cx));
        assert_eq!(resolve(cx, "new.png", &note), Attachment::Found(imported.clone()));
        std::fs::remove_file(dir.path().join(imported.as_str())).unwrap();
        service.update(cx, |s, cx| s.invalidate(std::slice::from_ref(&imported), cx));
        assert_eq!(resolve(cx, "new.png", &note), Attachment::Missing);
    }

    fn pdf_request(path: &VaultPath, page: u32, scale: u32) -> Request {
        Request { kind: Kind::Pdf, source: Source::Pdf { path: path.clone(), page }, size: 200, scale }
    }

    fn diagram_request(source: &str, dark: bool, font: &str, scale: u32) -> Request {
        Request {
            kind: Kind::Diagram,
            source: Source::Diagram { source: source.into(), dark, font: font.into() },
            size: 200,
            scale,
        }
    }

    #[test]
    fn pdf_disk_cache_keeps_page_count_and_keys_content_page_and_density() {
        let (_dir, vault, path) = vault_with(&super::two_page_pdf(), "pages.pdf");
        let cache = tempfile::tempdir().unwrap();
        let one = pdf_request(&path, 1, 100);
        let first = produce(&vault, cache.path(), &one).unwrap();
        assert_eq!(first.pages, Some(2));
        assert_eq!(first.origin, Origin::Rendered);
        let fingerprint = aero_core::hash_bytes(
            &serde_json::to_vec(&(super::rich::PDF_VERSION, vault.read_bytes(&path).unwrap().1.hash, 1u32))
                .unwrap(),
        );
        assert!(cache_file(cache.path(), &one, fingerprint).is_file(), "PDF 渲染版本与页码必须进入指纹");
        let disk = produce(&vault, cache.path(), &one).unwrap();
        assert_eq!(disk.origin, Origin::Disk);
        assert_eq!(disk.pages, Some(2), "新调用从磁盘命中时仍有总页数");
        let second = produce(&vault, cache.path(), &pdf_request(&path, 2, 100)).unwrap();
        assert_eq!(second.origin, Origin::Rendered);
        assert_ne!(first.frame.buffer(), second.frame.buffer());
        let retina = produce(&vault, cache.path(), &pdf_request(&path, 1, 200)).unwrap();
        assert_eq!(retina.pixels, size(400, 200));
        assert_eq!(retina.pages, Some(2));
        assert_eq!(std::fs::read_dir(cache.path().join("pdf")).unwrap().count(), 3);
        let renamed = VaultPath::new("renamed.pdf").unwrap();
        std::fs::copy(vault.fs_path(&path), vault.fs_path(&renamed)).unwrap();
        assert_eq!(
            produce(&vault, cache.path(), &pdf_request(&renamed, 1, 100)).unwrap().origin,
            Origin::Disk
        );
        // 修改页面内容后不能取旧像素；再损坏 PDF 也不能绕过元数据验证复用 PNG。
        let changed = String::from_utf8(super::two_page_pdf()).unwrap().replace("1 0 0 rg", "0 1 0 rg");
        std::fs::write(vault.fs_path(&path), changed).unwrap();
        let new = produce(&vault, cache.path(), &one).unwrap();
        assert_eq!(new.origin, Origin::Rendered);
        assert_ne!(new.frame.buffer(), first.frame.buffer());
        std::fs::write(vault.fs_path(&path), b"broken PDF").unwrap();
        assert!(produce(&vault, cache.path(), &one).is_err());
        for page in [0, 3] {
            assert!(produce(&vault, cache.path(), &pdf_request(&renamed, page, 100)).is_err());
        }
    }

    #[gpui_kit::test]
    fn diagram_disk_cache_keys_source_theme_font_and_hidpi(cx: &mut gpui_kit::TestAppContext) {
        let (_dir, vault, _) = vault_with(b"unused", "a.txt");
        let cache = tempfile::tempdir().unwrap();
        let request = diagram_request("flowchart LR\n A[中文] --> B[完成]", false, "PingFang SC", 100);
        let render = |request: &Request| {
            super::produce_with_text(&vault, cache.path(), request, Some(cx.text_system().clone())).unwrap()
        };
        let first = render(&request);
        assert_eq!(first.origin, Origin::Rendered);
        assert_eq!(first.pages, None);
        let fingerprint = aero_core::hash_bytes(
            &serde_json::to_vec(&(
                super::rich::DIAGRAM_VERSION,
                "flowchart LR\n A[中文] --> B[完成]",
                false,
                "PingFang SC",
            ))
            .unwrap(),
        );
        assert!(cache_file(cache.path(), &request, fingerprint).is_file(), "Mermaid 渲染版本必须进入指纹");
        assert_eq!(render(&request).origin, Origin::Disk);
        for other in [
            diagram_request("flowchart LR\n A[改动] --> B[完成]", false, "PingFang SC", 100),
            diagram_request("flowchart LR\n A[中文] --> B[完成]", true, "PingFang SC", 100),
            diagram_request("flowchart LR\n A[中文] --> B[完成]", false, "Arial", 100),
            diagram_request("flowchart LR\n A[中文] --> B[完成]", false, "PingFang SC", 200),
        ] {
            assert_eq!(render(&other).origin, Origin::Rendered);
        }
        assert_eq!(std::fs::read_dir(cache.path().join("diagrams")).unwrap().count(), 5);
        let retina = render(&Request { scale: 200, ..request.clone() });
        assert_eq!(retina.pixels.width, first.pixels.width * 2);
        // 截断的磁盘缓存会自愈。
        for file in std::fs::read_dir(cache.path().join("diagrams")).unwrap() {
            std::fs::write(file.unwrap().path(), b"truncated").unwrap();
        }
        assert_eq!(render(&request).origin, Origin::Rendered);
    }

    fn ready_info(state: PdfInfoState) -> Arc<PdfInfo> {
        let PdfInfoState::Ready(info) = state else { panic!("元数据应已完成：{state:?}") };
        info
    }

    #[gpui_kit::test]
    fn service_pdf_info_is_background_cached_and_uses_source_fingerprint(cx: &mut gpui_kit::TestAppContext) {
        let (_dir, vault, path) = vault_with(&super::two_page_pdf(), "assets/pages.pdf");
        let fingerprint = vault.read_bytes(&path).unwrap().1.hash;
        let cache = tempfile::tempdir().unwrap();
        let service = cx.new(|_| RenderService::new(Arc::new(vault), cache.path().to_path_buf()));
        for _ in 0..2 {
            assert!(matches!(
                service.update(cx, |s, cx| s.pdf_info(path.clone(), cx)),
                PdfInfoState::Pending
            ));
        }
        cx.run_until_parked();
        let info = ready_info(service.update(cx, |s, cx| s.pdf_info(path.clone(), cx)));
        assert_eq!(info.pages, vec![size(200., 100.); 2]);
        assert_eq!(info.fingerprint, fingerprint);
        let cached = ready_info(service.update(cx, |s, cx| s.pdf_info(path.clone(), cx)));
        assert!(Arc::ptr_eq(&info, &cached));
        service.update(cx, |s, _| assert!(s.entries.is_empty() && s.pixels.is_empty()));
        assert_eq!(std::fs::read_dir(cache.path()).unwrap().count(), 0, "元数据不生成任何位图或持久缓存");
    }

    #[gpui_kit::test]
    fn service_pdf_info_invalidates_replacements_and_retries_failures(cx: &mut gpui_kit::TestAppContext) {
        let (_dir, vault, path) = vault_with(&super::two_page_pdf(), "assets/pages.pdf");
        let other = VaultPath::new("other.pdf").unwrap();
        std::fs::write(vault.fs_path(&other), super::two_page_pdf()).unwrap();
        let cache = tempfile::tempdir().unwrap();
        let vault = Arc::new(vault);
        let service = cx.new(|_| RenderService::new(vault.clone(), cache.path().to_path_buf()));
        for path in [&path, &other] {
            service.update(cx, |s, cx| s.pdf_info(path.clone(), cx));
        }
        cx.run_until_parked();
        let old = ready_info(service.update(cx, |s, cx| s.pdf_info(path.clone(), cx)));
        let untouched = ready_info(service.update(cx, |s, cx| s.pdf_info(other.clone(), cx)));
        let replacement = super::rich::pages_pdf(&["/MediaBox [0 0 300 700] /Rotate 90"]);
        std::fs::write(vault.fs_path(&path), replacement).unwrap();
        service.update(cx, |s, cx| s.invalidate(std::slice::from_ref(&path), cx));
        assert!(Arc::ptr_eq(
            &untouched,
            &ready_info(service.update(cx, |s, cx| s.pdf_info(other.clone(), cx)))
        ));
        assert!(matches!(service.update(cx, |s, cx| s.pdf_info(path.clone(), cx)), PdfInfoState::Pending));
        cx.run_until_parked();
        let updated = ready_info(service.update(cx, |s, cx| s.pdf_info(path.clone(), cx)));
        assert_eq!(updated.pages, vec![size(700., 300.)]);
        assert_ne!(updated.fingerprint, old.fingerprint);
        assert_eq!(updated.fingerprint, vault.read_bytes(&path).unwrap().1.hash);
        assert!(!Arc::ptr_eq(&updated, &old));
        for broken in [b"broken PDF".to_vec(), super::rich::pages_pdf(&[])] {
            std::fs::write(vault.fs_path(&path), broken).unwrap();
            service.update(cx, |s, cx| s.invalidate(std::slice::from_ref(&path), cx));
            assert!(matches!(
                service.update(cx, |s, cx| s.pdf_info(path.clone(), cx)),
                PdfInfoState::Pending
            ));
            cx.run_until_parked();
            assert!(matches!(
                service.update(cx, |s, cx| s.pdf_info(path.clone(), cx)),
                PdfInfoState::Failed(_)
            ));
        }
        std::fs::write(vault.fs_path(&path), super::two_page_pdf()).unwrap();
        service.update(cx, |s, cx| s.retry(cx));
        for path in [&path, &other] {
            assert!(matches!(
                service.update(cx, |s, cx| s.pdf_info(path.clone(), cx)),
                PdfInfoState::Pending
            ));
        }
        cx.run_until_parked();
        assert_eq!(ready_info(service.update(cx, |s, cx| s.pdf_info(path.clone(), cx))).pages.len(), 2);
        std::fs::remove_file(vault.fs_path(&path)).unwrap();
        service.update(cx, |s, cx| s.invalidate(std::slice::from_ref(&path), cx));
        service.update(cx, |s, cx| s.pdf_info(path.clone(), cx));
        cx.run_until_parked();
        assert!(matches!(service.update(cx, |s, cx| s.pdf_info(path.clone(), cx)), PdfInfoState::Failed(_)));
    }

    /// 完成真实后台 I/O，但暂不运行已排队的 UI 回调；不靠 sleep 或假渲染器制造竞态。
    fn park_background_reply(cx: &mut gpui_kit::TestAppContext) {
        let executor = cx.executor();
        let dispatcher = executor.dispatcher().as_test().unwrap();
        while !dispatcher.tick(true) {
            assert!(executor.tick(), "应有一个后台任务等待执行");
        }
        while dispatcher.tick(true) {}
    }

    fn finish_obsolete(task: &gpui_kit::Task<()>, cx: &mut gpui_kit::TestAppContext) {
        while !task.is_ready() {
            assert!(cx.executor().tick(), "旧任务回调应能执行完毕");
        }
    }

    #[gpui_kit::test]
    fn service_pdf_info_stale_reply_cannot_replace_a_new_task(cx: &mut gpui_kit::TestAppContext) {
        let (_dir, vault, path) = vault_with(&super::two_page_pdf(), "pages.pdf");
        let cache = tempfile::tempdir().unwrap();
        let vault = Arc::new(vault);
        let service = cx.new(|_| RenderService::new(vault.clone(), cache.path().to_path_buf()));
        for (retry, broken) in [(false, false), (true, false), (false, true), (true, true)] {
            service.update(cx, |s, cx| s.invalidate(std::slice::from_ref(&path), cx));
            let bytes = if broken { b"broken PDF".to_vec() } else { super::two_page_pdf() };
            std::fs::write(vault.fs_path(&path), bytes).unwrap();
            service.update(cx, |s, cx| s.pdf_info(path.clone(), cx));
            park_background_reply(cx);
            // 保留旧任务句柄模拟已无法取消的回调，失效仍走真实的公开接口。
            let obsolete = service.update(cx, |s, _| {
                let super::PdfInfoEntry::Pending { _task, .. } = s.pdf_infos.get_mut(&path).unwrap() else {
                    panic!()
                };
                std::mem::replace(_task, gpui_kit::Task::ready(()))
            });
            std::fs::write(vault.fs_path(&path), super::rich::pages_pdf(&["/MediaBox [0 0 300 700]"]))
                .unwrap();
            let expected_hash = vault.read_bytes(&path).unwrap().1.hash;
            service.update(cx, |s, cx| {
                if retry {
                    s.retry(cx);
                } else {
                    s.invalidate(std::slice::from_ref(&path), cx);
                }
                assert!(matches!(s.pdf_info(path.clone(), cx), PdfInfoState::Pending));
            });
            finish_obsolete(&obsolete, cx);
            let state = service.update(cx, |s, cx| s.pdf_info(path.clone(), cx));
            assert!(
                matches!(&state, PdfInfoState::Pending)
                    || matches!(&state, PdfInfoState::Ready(info) if info.fingerprint == expected_hash),
                "旧回调覆盖新任务：{state:?}"
            );
            cx.run_until_parked();
            let info = ready_info(service.update(cx, |s, cx| s.pdf_info(path.clone(), cx)));
            assert_eq!(info.fingerprint, expected_hash);
            assert_eq!(info.pages, vec![size(300., 700.)]);
        }
    }

    #[gpui_kit::test]
    fn service_pdf_lru_stale_reply_cannot_resurrect_or_replace_entries(cx: &mut gpui_kit::TestAppContext) {
        let (_dir, vault, path) =
            vault_with(&super::rich::pages_pdf(&["/MediaBox [0 0 200 100]"; 26]), "pages.pdf");
        let cache = tempfile::tempdir().unwrap();
        let vault = Arc::new(vault);
        let service = cx.new(|_| RenderService::new(vault.clone(), cache.path().to_path_buf()));
        let request = pdf_request(&path, 1, 100);
        for rerequest in [false, true] {
            service.update(cx, |s, cx| s.invalidate(std::slice::from_ref(&path), cx));
            std::fs::write(vault.fs_path(&path), super::rich::pages_pdf(&["/MediaBox [0 0 200 100]"; 26]))
                .unwrap();
            service.update(cx, |s, cx| s.pdf(path.clone(), 1, 200, 1., cx));
            park_background_reply(cx);
            let obsolete = service.update(cx, |s, _| {
                let super::Entry::Pending { token } = s.entries.get(&request).unwrap() else { panic!() };
                std::mem::replace(s.running.get_mut(token).unwrap(), gpui_kit::Task::ready(()))
            });
            service.update(cx, |s, cx| {
                for page in 2..=13 {
                    s.pdf(path.clone(), page, 200, 1., cx);
                }
            });
            service.update(cx, |s, _| assert!(!s.entries.contains_key(&request)));
            if rerequest {
                std::fs::write(
                    vault.fs_path(&path),
                    super::rich::pages_pdf(&["/MediaBox [0 0 200 300]"; 26]),
                )
                .unwrap();
                service.update(cx, |s, cx| s.pdf(path.clone(), 1, 200, 1., cx));
            }
            finish_obsolete(&obsolete, cx);
            service.update(cx, |s, _| {
                if rerequest {
                    assert!(matches!(s.entries.get(&request), Some(super::Entry::Pending { .. } | super::Entry::Waiting))
                        || matches!(s.entries.get(&request), Some(super::Entry::Ready { pixels, .. }) if *pixels == size(200, 300)), "旧回调覆盖重新请求的页面");
                } else {
                    assert!(!s.entries.contains_key(&request), "旧回调不能复活已淘汰 entry");
                    assert!(!s.pixels.contains_key(&request.source), "旧回调不能复活占位尺寸");
                }
            });
            cx.run_until_parked();
            service.update(cx, |s, _| assert_eq!(s.entries.len(), 12));
        }
    }

    #[gpui_kit::test]
    fn service_pdf_keeps_simultaneously_visible_pages_across_documents(cx: &mut gpui_kit::TestAppContext) {
        let bytes = super::rich::pages_pdf(&["/MediaBox [0 0 200 100]"; 7]);
        let (_dir, vault, a) = vault_with(&bytes, "a.pdf");
        let b = VaultPath::new("b.pdf").unwrap();
        vault.create_bytes(&b, &bytes).unwrap();
        let cache = tempfile::tempdir().unwrap();
        let service = cx.new(|_| RenderService::new(Arc::new(vault), cache.path().to_path_buf()));
        service.update(cx, |s, cx| {
            for path in [&a, &b] {
                for page in 1..=7 {
                    s.pdf(path.clone(), page, 200, 1., cx);
                }
            }
        });
        cx.run_until_parked();
        for _ in 0..3 {
            service.update(cx, |s, cx| {
                for path in [&a, &b] {
                    for page in 1..=7 {
                        assert!(
                            matches!(s.pdf(path.clone(), page, 200, 1., cx), State::Ready { .. }),
                            "active pages must not evict one another"
                        );
                    }
                }
            });
            cx.run_until_parked();
        }
        service.update(cx, |s, cx| {
            s.pdf(b.clone(), 7, 200, 1., cx);
        });
        cx.run_until_parked();
        service.update(cx, |s, _| assert_eq!(s.access.len(), 12));
    }

    #[gpui_kit::test]
    fn service_pdf_lru_bounds_pending_without_evicting_other_kinds(cx: &mut gpui_kit::TestAppContext) {
        let (_dir, vault, path) =
            vault_with(&super::rich::pages_pdf(&["/MediaBox [0 0 200 100]"; 26]), "pages.pdf");
        let cache = tempfile::tempdir().unwrap();
        let service = cx.new(|_| RenderService::new(Arc::new(vault), cache.path().to_path_buf()));
        service.update(cx, |s, cx| {
            s.image(path.clone(), 200, 1., cx);
            s.math("a", false, 16., 1., gpui_kit::black(), cx);
            s.diagram("flowchart LR\n A --> B", 200, 1., false, "Arial", cx);
        });
        for page in 1..=26 {
            service.update(cx, |s, cx| {
                assert!(matches!(s.pdf(path.clone(), page, 200, 1., cx), State::Pending { .. }));
            });
            service.update(cx, |s, _| {
                assert!(s.entries.contains_key(&pdf_request(&path, page, 100)), "不能淘汰刚请求的页面");
                assert!(s.entries.keys().filter(|r| r.kind == Kind::Pdf).count() <= 12);
                assert_eq!(s.entries.keys().filter(|r| r.kind != Kind::Pdf).count(), 3);
            });
        }
        cx.run_until_parked();
        service.update(cx, |s, _| {
            assert_eq!(s.entries.keys().filter(|r| r.kind == Kind::Pdf).count(), 12);
            assert_eq!(s.pixels.keys().filter(|source| matches!(source, Source::Pdf { .. })).count(), 12);
            assert!(!s.entries.contains_key(&pdf_request(&path, 1, 100)));
        });
    }

    #[gpui_kit::test]
    fn service_pdf_lru_updates_hits_prunes_pixels_and_keeps_disk_cache(cx: &mut gpui_kit::TestAppContext) {
        let (_dir, vault, path) =
            vault_with(&super::rich::pages_pdf(&["/MediaBox [0 0 200 100]"; 26]), "pages.pdf");
        let cache = tempfile::tempdir().unwrap();
        let vault = Arc::new(vault);
        let service = cx.new(|_| RenderService::new(vault.clone(), cache.path().to_path_buf()));
        for page in 1..=12 {
            service.update(cx, |s, cx| s.pdf(path.clone(), page, 200, 1., cx));
        }
        cx.run_until_parked();
        assert!(matches!(
            service.update(cx, |s, cx| s.pdf(path.clone(), 1, 200, 1., cx)),
            State::Ready { .. }
        ));
        service.update(cx, |s, cx| s.pdf(path.clone(), 13, 200, 1., cx));
        service.update(cx, |s, _| {
            assert!(s.entries.contains_key(&pdf_request(&path, 1, 100)), "访问命中必须刷新 LRU");
            assert!(!s.entries.contains_key(&pdf_request(&path, 2, 100)));
            assert!(!s.pixels.contains_key(&pdf_request(&path, 2, 100).source));
        });
        for page in 14..=26 {
            service.update(cx, |s, cx| s.pdf(path.clone(), page, 200, 1., cx));
            cx.run_until_parked();
        }
        service.update(cx, |s, _| {
            assert_eq!(s.entries.len(), 12);
            assert_eq!(s.pixels.len(), 12);
        });
        assert_eq!(produce(&vault, cache.path(), &pdf_request(&path, 2, 100)).unwrap().origin, Origin::Disk);
        assert!(matches!(
            service.update(cx, |s, cx| s.pdf(path.clone(), 2, 200, 1., cx)),
            State::Pending { pixels: None }
        ));
        cx.run_until_parked();
        assert!(matches!(
            service.update(cx, |s, cx| s.pdf(path.clone(), 2, 200, 1., cx)),
            State::Ready { pages: Some(26), .. }
        ));
    }

    #[gpui_kit::test]
    fn service_pdf_caches_and_invalidates_every_page_and_placeholder(cx: &mut gpui_kit::TestAppContext) {
        let (_dir, vault, path) = vault_with(&super::two_page_pdf(), "pages.pdf");
        let cache = tempfile::tempdir().unwrap();
        let vault = Arc::new(vault);
        let service = cx.new(|_| RenderService::new(vault.clone(), cache.path().to_path_buf()));
        for page in [1, 2] {
            assert!(matches!(
                service.update(cx, |s, cx| s.pdf(path.clone(), page, 200, 1., cx)),
                State::Pending { pixels: None }
            ));
        }
        cx.run_until_parked();
        let first = service.update(cx, |s, cx| s.pdf(path.clone(), 1, 200, 1., cx));
        let State::Ready { image, pages, .. } = first else { panic!("{first:?}") };
        assert_eq!(pages, Some(2));
        let again = service.update(cx, |s, cx| s.pdf(path.clone(), 1, 200, 1., cx));
        let State::Ready { image: cached, .. } = again else { panic!("{again:?}") };
        assert!(Arc::ptr_eq(&image, &cached), "内存命中复用 RenderImage");
        service.update(cx, |s, cx| s.invalidate(std::slice::from_ref(&path), cx));
        for page in [1, 2] {
            assert!(matches!(
                service.update(cx, |s, cx| s.pdf(path.clone(), page, 200, 1., cx)),
                State::Pending { pixels: None }
            ));
        }
        cx.run_until_parked();
        std::fs::remove_file(vault.fs_path(&path)).unwrap();
        service.update(cx, |s, cx| s.invalidate(std::slice::from_ref(&path), cx));
        service.update(cx, |s, cx| s.pdf(path.clone(), 1, 200, 1., cx));
        cx.run_until_parked();
        assert!(matches!(service.update(cx, |s, cx| s.pdf(path.clone(), 1, 200, 1., cx)), State::Failed(_)));
    }

    #[gpui_kit::test]
    fn service_pdf_rejects_invalid_pages_without_clamping_or_caching(cx: &mut gpui_kit::TestAppContext) {
        let (_dir, vault, path) = vault_with(&super::two_page_pdf(), "pages.pdf");
        let cache = tempfile::tempdir().unwrap();
        let service = cx.new(|_| RenderService::new(Arc::new(vault), cache.path().to_path_buf()));
        for page in [0, 3, u32::MAX] {
            service.update(cx, |s, cx| s.pdf(path.clone(), page, 200, 1., cx));
        }
        cx.run_until_parked();
        for page in [0, 3, u32::MAX] {
            let result = service.update(cx, |s, cx| s.pdf(path.clone(), page, 200, 1., cx));
            assert!(matches!(result, State::Failed(ref error) if error.contains("页码")), "{result:?}");
        }
        assert!(!cache.path().join("pdf").exists(), "非法页码不写入缓存");
    }

    #[gpui_kit::test]
    fn service_diagram_reacts_to_live_source_and_reports_errors(cx: &mut gpui_kit::TestAppContext) {
        let (_dir, vault, _) = vault_with(b"unused", "a.txt");
        let cache = tempfile::tempdir().unwrap();
        let service = cx.new(|_| RenderService::new(Arc::new(vault), cache.path().to_path_buf()));
        let source = "flowchart LR\n A --> B";
        assert!(matches!(
            service.update(cx, |s, cx| s.diagram(source, 200, 2., false, "Arial", cx)),
            State::Pending { .. }
        ));
        cx.run_until_parked();
        let first = service.update(cx, |s, cx| s.diagram(source, 200, 2., false, "Arial", cx));
        assert!(
            matches!(first, State::Ready { pages: None, pixels, .. } if pixels.width == 400),
            "{first:?}"
        );
        assert!(matches!(
            service.update(cx, |s, cx| s.diagram("flowchart LR\n A --> C", 200, 2., false, "Arial", cx)),
            State::Pending { .. }
        ));
        service.update(cx, |s, cx| s.diagram("invalid mermaid", 200, 1., false, "Arial", cx));
        cx.run_until_parked();
        assert!(matches!(
            service.update(cx, |s, cx| s.diagram("invalid mermaid", 200, 1., false, "Arial", cx)),
            State::Failed(_)
        ));
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
