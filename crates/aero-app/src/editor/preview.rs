use std::collections::HashMap;

use aero_core::{CalloutKind, Fold};
use gpui_kit::assets::IconName;
use gpui_kit::base::text::{MarkdownNode, TextView, TextViewStyle, markdown_ast};
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::{ActiveTheme as _, Icon, Sizable as _, h_flex, v_flex};
use gpui_kit::prelude::*;
use gpui_kit::*;

#[derive(Clone, Debug)]
struct Callout {
    kind: CalloutKind,
    title: Option<String>,
    fold: Fold,
    body: String,
    offset: usize,
}

fn parse_callout(source: &str, ancestor_quotes: usize, offset: usize) -> Option<Callout> {
    let mut lines = source.lines();
    let header = lines.next()?.strip_prefix('>')?.trim_start();
    if !header.starts_with("[!") {
        return None;
    }
    let (kind, fold, title, _) = aero_core::parse::callout_header(header)?;
    let body = lines
        .map(|mut line| {
            // 同一层引用的 > 后空格可逐行变化，不能按标题列宽截取。
            for _ in 0..=ancestor_quotes {
                let Some(rest) = line.trim_start().strip_prefix('>') else { break };
                line = rest.strip_prefix([' ', '\t']).unwrap_or(rest);
            }
            line
        })
        .collect::<Vec<_>>()
        .join("\n");
    Some(Callout { kind, title, fold, body, offset })
}

pub(super) fn markdown(
    id: SharedString,
    text: SharedString,
    style: TextViewStyle,
    reveal: u64,
    state: Entity<PreviewState>,
    editor: Entity<super::Editor>,
    block: aero_core::BlockId,
    depth: u8,
) -> TextView {
    let block_editor = editor.clone();
    let embed_editor = editor.clone();
    let embed_style = style.clone();
    let embed_id = id.clone();
    let highlight_editor = editor.clone();
    let highlight_style = style.clone();
    let highlight_id = id.clone();
    let image_editor = editor.clone();
    let image_style = style.clone();
    let image_id = id.clone();
    with_links(TextView::markdown(id.clone(), text), &editor)
        .style(style.clone())
        .selectable(false)
        .markdown_block_parser(|node, cx| {
            if !matches!(node, markdown_ast::Node::Paragraph(_)) {
                return None;
            }
            let source = cx.node_source(node)?;
            let embed = image_embed(source)?;
            Some(
                MarkdownNode::new("image", (node.position()?.start.offset, embed))
                    .markdown(source.to_owned()),
            )
        })
        .markdown_block_renderer("image", move |node, window, cx| {
            let (offset, embed) = node.data::<(usize, ImageEmbed)>().expect("image parser data");
            let id: SharedString = format!("{image_id}-image-{offset}").into();
            render_image(id, embed, &image_style, &image_editor, block, window, cx)
        })
        .markdown_block_parser(|node, cx| {
            if !matches!(node, markdown_ast::Node::Paragraph(_)) {
                return None;
            }
            let source = cx.node_source(node)?;
            let embed = note_embed(source)?;
            Some(
                MarkdownNode::new("note-embed", (node.position()?.start.offset, embed))
                    .markdown(source.to_owned()),
            )
        })
        .markdown_block_renderer("note-embed", move |node, _, cx| {
            let (offset, embed) = node.data::<(usize, NoteEmbed)>().expect("note embed parser data");
            let id: SharedString = format!("{embed_id}-embed-{offset}").into();
            render_note_embed(id, embed, &embed_style, &embed_editor, block, depth, cx)
        })
        .markdown_block_parser(|node, cx| {
            if !matches!(node, markdown_ast::Node::Blockquote(_)) {
                return None;
            }
            let pos = node.position()?;
            let source = cx.node_source(node)?;
            let prefix = cx.source()[..pos.start.offset].rsplit('\n').next().unwrap_or("");
            let ancestor_quotes = prefix.bytes().filter(|&b| b == b'>').count();
            let callout = parse_callout(source, ancestor_quotes, pos.start.offset)?;
            Some(MarkdownNode::new("callout", callout).markdown(source.to_owned()))
        })
        .markdown_block_renderer("callout", move |node, _, cx| {
            let callout = node.data::<Callout>().expect("callout parser data");
            let id: SharedString = format!("{id}-callout-{}", callout.offset).into();
            render_callout(id, callout, style.clone(), reveal, &state, &block_editor, block, depth, cx)
        })
        .markdown_block_parser(|node, cx| {
            if !matches!(
                node,
                markdown_ast::Node::Paragraph(_)
                    | markdown_ast::Node::Heading(_)
                    | markdown_ast::Node::Table(_)
            ) {
                return None;
            }
            let pos = node.position()?;
            let preview = aero_core::highlight::preview(cx.source(), pos.start.offset..pos.end.offset)?;
            Some(
                MarkdownNode::new("highlight", (pos.start.offset, preview))
                    .markdown(cx.node_source(node)?.to_owned()),
            )
        })
        .markdown_block_renderer("highlight", move |node, window, cx| {
            let (offset, preview) =
                node.data::<(usize, aero_core::highlight::Preview)>().expect("highlight parser data");
            let id: SharedString = format!("{highlight_id}-highlight-{offset}").into();
            match preview {
                aero_core::highlight::Preview::Html(html) => {
                    highlight_html(id, html, &highlight_style, &highlight_editor, window, cx)
                        .into_any_element()
                }
                aero_core::highlight::Preview::Table { alignments, rows } => {
                    highlight_table(id, alignments, rows, &highlight_style, &highlight_editor, window, cx)
                }
            }
        })
        .on_link_click(move |url, event, _window, cx| match link_click(url, event) {
            LinkClick::Ignore => {}
            // 外部链接保持默认行为：系统浏览器打开
            LinkClick::External => cx.open_url(url),
            LinkClick::Note { target, fragment } => editor.update(cx, |_, cx| {
                cx.emit(super::EditorEvent::LinkClicked { target, fragment });
            }),
        })
        .w_full()
}

/// 图片嵌入块（`ui_design.md` 5.6）：`![[x.png]]` / `![[x.png|400]]`。
#[derive(Clone, Debug, PartialEq, Eq)]
struct ImageEmbed {
    target: String,
    /// `|400` 的显示宽度；`|小` 之类的非数字参数忽略，按默认宽度显示
    width: Option<u32>,
    source: String,
}

/// 正文列宽内的默认显示宽度（正文列 720，两侧留白）。
const IMAGE_WIDTH: u32 = 680;

/// 整段只有一个图片嵌入时才当图片块渲染；与文字混排的嵌入仍按源码显示。
fn image_embed(source: &str) -> Option<ImageEmbed> {
    let trimmed = source.trim();
    let embeds = aero_core::links::scan_embeds(trimmed);
    let [embed] = embeds.as_slice() else { return None };
    if embed.range != (0..trimmed.len()) || embed.fragment.is_some() {
        return None;
    }
    if aero_core::attachments::Kind::for_path(&embed.target) != Some(aero_core::attachments::Kind::Image) {
        return None;
    }
    Some(ImageEmbed {
        target: embed.target.clone(),
        width: embed.parameter.as_deref().and_then(|p| p.trim().parse().ok()),
        source: trimmed.to_owned(),
    })
}

/// 笔记 / 章节 / 块嵌入（`ui_design.md` 5.6）：`![[笔记]]`、`![[笔记#章节]]`、`![[笔记#^块]]`。
#[derive(Clone, Debug, PartialEq, Eq)]
struct NoteEmbed {
    target: String,
    fragment: Option<String>,
    source: String,
}

/// 嵌入里还能再嵌入，但只渲染一层：再深就显示源码，免得互相嵌入转不出来。
const EMBED_DEPTH: u8 = 1;

/// 整段只有一个笔记嵌入时才渲染内容；附件交给图片渲染器，PDF 暂时保持源码。
fn note_embed(source: &str) -> Option<NoteEmbed> {
    let trimmed = source.trim();
    let embeds = aero_core::links::scan_embeds(trimmed);
    let [embed] = embeds.as_slice() else { return None };
    if embed.range != (0..trimmed.len()) || aero_core::attachments::Kind::for_path(&embed.target).is_some() {
        return None;
    }
    Some(NoteEmbed {
        target: embed.target.clone(),
        fragment: embed.fragment.clone(),
        source: trimmed.to_owned(),
    })
}

/// 只读渲染：左侧竖线 + 来源标题，内容用同一套渲染器画（嵌入里的表格、callout、图片都照常）。
fn render_note_embed(
    id: SharedString,
    embed: &NoteEmbed,
    style: &TextViewStyle,
    editor: &Entity<super::Editor>,
    block: aero_core::BlockId,
    depth: u8,
    cx: &mut App,
) -> AnyElement {
    use super::embeds_ui::EmbedView;

    let theme = cx.theme();
    let selector = {
        let (target, fragment) = (embed.target.clone(), embed.fragment.clone());
        move || match &fragment {
            Some(f) => format!("embed-{target}#{f}"),
            None => format!("embed-{target}"),
        }
    };
    let view = editor.read(cx).embed_view(&embed.target, embed.fragment.as_deref());
    let body: AnyElement = match view {
        EmbedView::Loading => {
            div().text_xs().text_color(theme.muted_foreground).child("正在载入…").into_any_element()
        }
        EmbedView::Failed(error) => v_flex()
            .gap_1()
            .child(embed_source(format!("{id}-source").into(), &embed.source, cx))
            .child(
                div()
                    .debug_selector(|| "embed-error".to_owned())
                    .text_xs()
                    .text_color(theme.danger)
                    .child(error),
            )
            .into_any_element(),
        EmbedView::Ready { path, fragment, title, source } => {
            let click = {
                let (editor, target, fragment) = (editor.clone(), embed.target.clone(), fragment.clone());
                move |event: &ClickEvent, _: &mut Window, cx: &mut App| {
                    if event.is_right_click() {
                        return;
                    }
                    cx.stop_propagation();
                    // 与点 wikilink 同一条路：工作区负责打开并定位
                    editor.update(cx, |_, cx| {
                        cx.emit(super::EditorEvent::LinkClicked {
                            target: target.clone(),
                            fragment: fragment.clone(),
                        });
                    });
                }
            };
            let label = match &fragment {
                Some(f) if !f.is_empty() => format!("{title} › {f}"),
                _ => title,
            };
            let _ = path;
            let content: AnyElement = if depth < EMBED_DEPTH {
                markdown(
                    format!("{id}-body").into(),
                    aero_core::links::to_markdown(&source).into(),
                    style.clone(),
                    0,
                    editor.read(cx).preview.clone(),
                    editor.clone(),
                    block,
                    depth + 1,
                )
                .into_any_element()
            } else {
                embed_source(format!("{id}-nested").into(), &source, cx).into_any_element()
            };
            v_flex()
                .id(id.clone())
                .gap_1()
                .cursor_pointer()
                .on_click(click)
                .child(div().text_xs().text_color(theme.muted_foreground).child(label))
                .child(content)
                .into_any_element()
        }
    };
    div()
        .w_full()
        .pb(rems(0.5))
        .child(
            v_flex()
                .debug_selector(selector)
                .w_full()
                .min_w_0()
                .border_l_3()
                .border_color(theme.border)
                .pl_3()
                .child(body),
        )
        .into_any_element()
}

/// 渲染不了就显示源码：内容永不丢失。
fn embed_source(id: SharedString, source: &str, cx: &App) -> Stateful<Div> {
    div()
        .id(id)
        .w_full()
        .font_family(cx.theme().mono_font_family.clone())
        .text_size(px(13.))
        .text_color(cx.theme().muted_foreground)
        .child(source.to_owned())
}

/// 位图 / 占位 / 源码 + 错误 + 重试（9.9 的统一表现）；渲染永远不改文件。
fn render_image(
    id: SharedString,
    embed: &ImageEmbed,
    style: &TextViewStyle,
    editor: &Entity<super::Editor>,
    block: aero_core::BlockId,
    window: &mut Window,
    cx: &mut App,
) -> AnyElement {
    use crate::render::{Attachment, State};

    let (source, service) = {
        let editor = editor.read(cx);
        (editor.path.clone(), editor.images.clone())
    };
    // 没接渲染服务（无 vault 的测试上下文）时退回源码，不是错误
    let Some(service) = service else { return image_source(id, embed, style, cx) };
    let width = embed.width.unwrap_or(IMAGE_WIDTH).clamp(24, 4096);
    let scale = window.scale_factor();
    let state = service.update(cx, |service, cx| match service.attachment(&embed.target, &source, cx) {
        Attachment::Loading => State::Pending { pixels: None },
        Attachment::Found(file) => service.image(file, width, scale, cx),
        Attachment::Missing => State::Failed(format!("找不到附件 {}", embed.target).into()),
        Attachment::Ambiguous => {
            State::Failed(format!("有多个同名附件 {}，请写出所在目录", embed.target).into())
        }
    });
    let selector = {
        let target = embed.target.clone();
        move || format!("image-{target}")
    };
    let placeholder_selector = {
        let target = embed.target.clone();
        move || format!("image-placeholder-{target}")
    };
    match state {
        State::Ready { image, pixels } => {
            // 原图比目标窄时按自然宽度显示，不放大
            let logical = (pixels.width as f32 / scale).min(width as f32);
            let height = logical * pixels.height as f32 / pixels.width as f32;
            let click = image_click(editor, block, embed.target.clone());
            div()
                .w_full()
                .pb(rems(0.5))
                .child(
                    div()
                        .id(id.clone())
                        .debug_selector(selector)
                        .w(px(logical))
                        .h(px(height))
                        .max_w_full()
                        // 窗口窄时裁掉而不是压扁
                        .overflow_hidden()
                        .rounded(px(6.))
                        .cursor_pointer()
                        .on_click(click)
                        .child(img(ImageSource::Render(image)).w(px(logical)).h(px(height))),
                )
                .into_any_element()
        }
        // 首帧占位用上次已知比例，渲染完成后替换，列表不抖动
        State::Pending { pixels } => {
            let logical = (width as f32).min(IMAGE_WIDTH as f32);
            let ratio = pixels.map_or(0.5625, |p| p.height as f32 / p.width.max(1) as f32);
            div()
                .w_full()
                .pb(rems(0.5))
                .child(
                    div()
                        .debug_selector(placeholder_selector)
                        .w(px(logical))
                        .h(px(logical * ratio))
                        .max_w_full()
                        .rounded(px(6.))
                        .bg(cx.theme().muted),
                )
                .into_any_element()
        }
        State::Failed(error) => {
            let service = service.clone();
            v_flex()
                .w_full()
                .gap_1()
                .pb(rems(0.5))
                .child(image_source(format!("{id}-source").into(), embed, style, cx))
                .child(
                    h_flex()
                        .gap_2()
                        .items_center()
                        .child(
                            div()
                                .debug_selector(|| "image-error".to_owned())
                                .text_xs()
                                .text_color(cx.theme().danger)
                                .child(error),
                        )
                        .child(
                            div()
                                .debug_selector(|| "image-retry".to_owned())
                                // 不让点击冒泡成"进编辑态"：那会把整块换成源码输入框
                                .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
                                .child(Button::new("image-retry").ghost().xsmall().label("重试").on_click(
                                    move |_, _, cx| {
                                        cx.stop_propagation();
                                        service.update(cx, |service, cx| service.retry(cx));
                                    },
                                )),
                        ),
                )
                .into_any_element()
        }
    }
}

/// 单击选中这一块（要改 `|宽度` 就按 ⏎ 进编辑态），双击全屏看图。
fn image_click(
    editor: &Entity<super::Editor>,
    block: aero_core::BlockId,
    target: String,
) -> impl Fn(&ClickEvent, &mut Window, &mut App) + 'static {
    let editor = editor.clone();
    move |event: &ClickEvent, window, cx| {
        if event.is_right_click() {
            return;
        }
        let double = matches!(event, ClickEvent::Mouse(click) if click.down.click_count >= 2);
        cx.stop_propagation();
        let target = target.clone();
        editor.update(cx, |editor, cx| {
            if double {
                editor.open_image_viewer(target, window, cx);
            } else {
                editor.select_block_by_id(block, window, cx);
            }
        });
    }
}

/// 渲染不了就显示源码：内容永不丢失，也不写回文件。
fn image_source(id: SharedString, embed: &ImageEmbed, style: &TextViewStyle, cx: &App) -> AnyElement {
    let _ = style;
    div()
        .id(id)
        .debug_selector({
            let target = embed.target.clone();
            move || format!("image-source-{target}")
        })
        .w_full()
        .font_family(cx.theme().mono_font_family.clone())
        .text_size(px(13.))
        .text_color(cx.theme().muted_foreground)
        .child(embed.source.clone())
        .into_any_element()
}

fn with_links(view: TextView, editor: &Entity<super::Editor>) -> TextView {
    let style_editor = editor.clone();
    let hover_editor = editor.clone();
    view.link_style(move |url, cx| style_editor.read(cx).preview_link_style(url, cx)).on_link_hover(
        move |url, entered, bounds, window, cx| {
            hover_editor.update(cx, |editor, cx| editor.hover_link(url, entered, bounds, window, cx));
        },
    )
}

fn highlight_html(
    id: SharedString,
    lines: &[String],
    style: &TextViewStyle,
    editor: &Entity<super::Editor>,
    window: &Window,
    cx: &App,
) -> Div {
    let color = super::highlight_ui::color_hex(cx);
    let line_height = window.text_style().line_height_in_pixels(window.rem_size());
    v_flex().w_full().items_start().children(lines.iter().enumerate().map(|(ix, html)| {
        let editor = editor.clone();
        // TextView 内部使用 h_full；最小高度放在独立容器，避免换行内容被按一行测量。
        div().min_h(line_height).max_w_full().child(
            with_links(
                TextView::html(
                    SharedString::from(format!("{id}-line-{ix}")),
                    html.replace("<mark>", &format!("<mark color=\"{color}\">")),
                ),
                &editor,
            )
            .style(style.clone())
            .selectable(false)
            .on_link_click(move |url, event, _, cx| match link_click(url, event) {
                LinkClick::Ignore => {}
                LinkClick::External => cx.open_url(url),
                LinkClick::Note { target, fragment } => editor.update(cx, |_, cx| {
                    cx.emit(super::EditorEvent::LinkClicked { target, fragment });
                }),
            })
            .max_w_full(),
        )
    }))
}

fn highlight_table(
    id: SharedString,
    alignments: &[aero_core::highlight::Alignment],
    rows: &[Vec<aero_core::highlight::Cell>],
    style: &TextViewStyle,
    editor: &Entity<super::Editor>,
    window: &mut Window,
    cx: &mut App,
) -> AnyElement {
    use aero_core::highlight::Alignment;
    use gpui_kit::base::{ScrollableMask, StyledExt as _};

    let text_style = window.text_style();
    let font_size = text_style.font_size.to_pixels(window.rem_size());
    let mut widths = vec![48.0_f32; alignments.len()];
    for (row_ix, row) in rows.iter().enumerate() {
        for (ix, cell) in row.iter().enumerate() {
            let mut font = text_style.font();
            if row_ix == 0 {
                font.weight = FontWeight::SEMIBOLD;
            }
            for line in cell.text.lines() {
                let run = TextRun {
                    len: line.len(),
                    font: font.clone(),
                    color: cx.theme().foreground,
                    ..Default::default()
                };
                let width = window.text_system().layout_line(line, font_size, &[run], None).width;
                widths[ix] = widths[ix].max(f32::from(width) + 17.);
            }
        }
    }
    let floors: Vec<f32> = widths.iter().map(|w| (w / 2.).clamp(160., 480.).min(*w)).collect();
    let scroll = window.use_keyed_state(id.clone(), cx, |_, _| ScrollHandle::default()).read(cx).clone();
    let cell_style = style.clone().with_paragraph_gap(rems(0.));
    let track = div().min_w_full().w(px(floors.iter().sum::<f32>() + 2.)).children(
        rows.iter().enumerate().map(|(row_ix, row)| {
            h_flex()
                .w_full()
                .items_stretch()
                .border_color(cx.theme().border)
                .when(row_ix + 1 < rows.len(), |el| el.border_b_1())
                .when(row_ix == 0, |el| {
                    el.bg(cx.theme().muted).font_weight(FontWeight::SEMIBOLD).refine_style(style.table_head())
                })
                .children(row.iter().enumerate().map(|(ix, cell)| {
                    div()
                        .debug_selector(move || format!("highlight-cell-{row_ix}-{ix}"))
                        .flex_basis(px(widths[ix]))
                        .flex_grow(widths[ix])
                        .flex_shrink(1.)
                        .min_w(px(floors[ix]))
                        .overflow_hidden()
                        .px_2()
                        .py_1()
                        .when(ix + 1 < alignments.len(), |el| el.border_r_1().border_color(cx.theme().border))
                        .refine_style(style.table_cell())
                        .child(
                            // GPUI 对齐文字时背景终点不含偏移；用容器对齐自然宽度的文字。
                            highlight_html(
                                format!("{id}-{row_ix}-{ix}").into(),
                                &cell.html,
                                &cell_style,
                                editor,
                                window,
                                cx,
                            )
                            .text_left()
                            .when(matches!(alignments[ix], Alignment::Center), |el| el.items_center())
                            .when(matches!(alignments[ix], Alignment::Right), |el| el.items_end()),
                        )
                }))
        }),
    );
    // HTML 表格解析会跳过空单元格且丢弃 align，容器结构必须保留在原生布局中。
    div()
        .w_full()
        .pb(rems(1.))
        .relative()
        .child(
            div()
                .id(id.clone())
                .debug_selector(|| "highlight-table".to_owned())
                .w_full()
                .border_1()
                .border_color(cx.theme().border)
                .refine_style(style.table())
                .overflow_hidden()
                .track_scroll(&scroll)
                .child(track),
        )
        .child(ScrollableMask::new(Axis::Horizontal, &scroll).id(id))
        .into_any_element()
}

/// 预览里一次链接点击该怎么处理。
#[derive(Debug, PartialEq, Eq)]
pub(super) enum LinkClick {
    /// 右键 / 长按等不算"打开"
    Ignore,
    /// 不是 `aero://note/`：交给系统
    External,
    /// wikilink：已解码的目标与片段，发给工作区解析
    Note { target: String, fragment: Option<String> },
}

/// 与 TextView 无 handler 时的默认判定一致：左键 / 中键 / 键盘 / 非长按触摸才算点击。
/// `aero://note/` 在这里就解码——工作区拿到的是目标与片段，不再二次解析 URL。
pub(super) fn link_click(url: &str, event: &ClickEvent) -> LinkClick {
    let actionable = match event {
        ClickEvent::Mouse(click) => matches!(click.up.button, MouseButton::Left | MouseButton::Middle),
        ClickEvent::Keyboard(_) => true,
        ClickEvent::Touch(click) => !click.long_press,
    };
    if !actionable {
        return LinkClick::Ignore;
    }
    match aero_core::links::decode_note_url(url) {
        Some((target, fragment)) => LinkClick::Note { target, fragment },
        None if url.starts_with(aero_core::links::NOTE_URL_PREFIX) => LinkClick::Ignore,
        None => LinkClick::External,
    }
}

fn appearance(kind: &CalloutKind, cx: &App) -> (&'static str, IconName, Hsla) {
    let theme = cx.theme();
    match kind {
        CalloutKind::Note | CalloutKind::Other(_) => ("Note", IconName::Info, theme.info),
        CalloutKind::Tip => ("Tip", IconName::Lightbulb, theme.success),
        CalloutKind::Warning => ("Warning", IconName::TriangleAlert, theme.warning),
        CalloutKind::Danger => ("Danger", IconName::CircleAlert, theme.danger),
        CalloutKind::Info => ("Info", IconName::Info, theme.info),
        CalloutKind::Question => ("Question", IconName::CircleQuestionMark, theme.warning),
        CalloutKind::Example => ("Example", IconName::List, theme.primary),
        CalloutKind::Quote => ("Quote", IconName::Quote, theme.muted_foreground),
    }
}

struct FoldState {
    fold: Fold,
    open: bool,
    reveal: u64,
}

#[derive(Default)]
pub(super) struct PreviewState {
    folds: HashMap<SharedString, FoldState>,
}

fn render_callout(
    id: SharedString,
    callout: &Callout,
    style: TextViewStyle,
    reveal: u64,
    state: &Entity<PreviewState>,
    editor: &Entity<super::Editor>,
    block: aero_core::BlockId,
    depth: u8,
    cx: &mut App,
) -> AnyElement {
    let (label, icon, color) = appearance(&callout.kind, cx);
    let title = callout.title.clone().unwrap_or_else(|| label.to_owned());
    let fold = callout.fold;
    let open = state.update(cx, |state, _| {
        let state = state.folds.entry(id.clone()).or_insert(FoldState {
            fold,
            open: fold != Fold::Collapsed,
            reveal: 0,
        });
        if state.fold != fold {
            state.fold = fold;
            state.open = fold != Fold::Collapsed;
        }
        if reveal != 0 && state.reveal != reveal {
            state.reveal = reveal;
            state.open = true;
        }
        fold == Fold::None || state.open
    });
    let toggle_state = state.clone();
    let toggle_id = id.clone();
    let header = h_flex()
        .w_full()
        .gap_2()
        .text_color(color)
        .child(Icon::new(icon).size(px(18.)).flex_none())
        .child(
            div()
                .debug_selector({
                    let title = title.clone();
                    move || format!("callout-title-{title}")
                })
                .flex_1()
                .min_w_0()
                .font_weight(FontWeight::SEMIBOLD)
                .child(title.clone()),
        )
        .when(fold != Fold::None, |el| {
            el.child(
                div()
                    .debug_selector({
                        let title = title.clone();
                        move || format!("callout-toggle-{title}")
                    })
                    .flex_none()
                    .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
                    .child(
                        Button::new("fold")
                            .ghost()
                            .xsmall()
                            .icon(
                                Icon::new(if open { IconName::ChevronDown } else { IconName::ChevronRight })
                                    .text_color(color),
                            )
                            .tooltip(if open { "折叠" } else { "展开" })
                            .accessibility_label(format!("{} {title}", if open { "折叠" } else { "展开" }))
                            .on_click(move |_, _, cx| {
                                cx.stop_propagation();
                                toggle_state.update(cx, |state, cx| {
                                    if let Some(fold) = state.folds.get_mut(&toggle_id) {
                                        fold.open = !fold.open;
                                        cx.notify();
                                    }
                                });
                            }),
                    ),
            )
        });
    v_flex()
        .id(id.clone())
        .w_full()
        .min_w_0()
        .gap_2()
        .px_3()
        .py_2()
        .rounded(px(6.))
        .border_l_3()
        .border_color(color)
        .bg(color.opacity(0.08))
        .text_color(cx.theme().foreground)
        .child(header)
        .when(open && !callout.body.is_empty(), |el| {
            el.child(div().debug_selector(move || format!("callout-body-{title}")).w_full().min_w_0().child(
                markdown(
                    format!("{id}-body").into(),
                    aero_core::links::to_markdown(&callout.body).into(),
                    style,
                    reveal,
                    state.clone(),
                    editor.clone(),
                    block,
                    depth,
                ),
            ))
        })
        .into_any_element()
}

#[cfg(test)]
mod tests {
    use super::{LinkClick, link_click, parse_callout};
    use aero_core::{BlockKind, CalloutKind, Fold};
    use gpui_kit::{
        ClickEvent, Modifiers, MouseButton, MouseClickEvent, MouseDownEvent, MouseUpEvent, point, px,
    };

    #[test]
    fn callout_body_preserves_markdown_structure_and_lazy_lines() {
        let source = "> [!TiP]+ 标题\r\n> first\r\nlazy continuation\r\n>\r\n>     indented code\r\n>\r\n> - one\r\n>   - two\r\n>\r\n> > [!note] nested\r\n> > text\r\n>\r\n> | A | B |\r\n> | --- | --- |\r\n> | `x` | **y** |";
        let callout = parse_callout(source, 0, 7).unwrap();
        assert_eq!(callout.kind, CalloutKind::Tip);
        assert_eq!(callout.fold, Fold::Expanded);
        assert_eq!(callout.title.as_deref(), Some("标题"));
        assert_eq!(callout.offset, 7);
        assert_eq!(
            callout.body,
            "first\nlazy continuation\n\n    indented code\n\n- one\n  - two\n\n> [!note] nested\n> text\n\n| A | B |\n| --- | --- |\n| `x` | **y** |"
        );
        let parsed = aero_core::parse(&callout.body);
        assert!(parsed.blocks.iter().any(|b| b.kind == BlockKind::CodeBlock));
        assert!(parsed.blocks.iter().any(|b| b.kind == BlockKind::Table));
        assert!(parsed.blocks.iter().any(|b| matches!(b.kind, BlockKind::Callout { .. })));
    }

    #[test]
    fn mixed_quote_spacing_preserves_nested_body() {
        for line in [">>> deeper", "> > > deeper", ">> > deeper", "> >> deeper"] {
            let source = format!("> [!note] Nested\n{line}");
            let callout = parse_callout(&source, 1, 2).unwrap();
            assert_eq!(callout.body, "> deeper", "{line}");
        }
    }

    #[test]
    fn nested_quote_prefixes_are_removed_once() {
        let callout =
            parse_callout("> [!note]\n> > body\n> >\n> >     code\n> > > ordinary quote", 1, 2).unwrap();
        assert_eq!(callout.body, "body\n\n    code\n> ordinary quote");
        for source in ["> quote", "> [!note]x", "> [!]", "> [!note2]", "> > [!note]", "> \\[!note]"] {
            assert!(parse_callout(source, 0, 0).is_none(), "{source}");
        }
    }

    fn mouse_click(button: MouseButton) -> ClickEvent {
        let position = point(px(0.), px(0.));
        let modifiers = Modifiers::default();
        ClickEvent::Mouse(MouseClickEvent {
            down: MouseDownEvent { button, position, modifiers, click_count: 1, first_mouse: false },
            up: MouseUpEvent { button, position, modifiers, click_count: 1 },
        })
    }

    /// 预览发给工作区的必须是 `to_markdown` 生成的 URL 解码后的目标与片段（两端共用 links 模块，不能各解析一套）。
    #[test]
    fn link_click_decodes_note_urls_and_passes_external_ones_through() {
        let url = aero_core::links::encode_note_url("rust/cargo", Some("安装"));
        assert_eq!(
            link_click(&url, &mouse_click(MouseButton::Left)),
            LinkClick::Note { target: "rust/cargo".into(), fragment: Some("安装".into()) }
        );
        assert_eq!(
            link_click(&aero_core::links::encode_note_url("", Some("h")), &mouse_click(MouseButton::Middle)),
            LinkClick::Note { target: String::new(), fragment: Some("h".into()) }
        );
        assert_eq!(link_click("https://example.com", &mouse_click(MouseButton::Left)), LinkClick::External);
        assert_eq!(link_click(&url, &mouse_click(MouseButton::Right)), LinkClick::Ignore);
        assert_eq!(link_click("aero://note/%zz", &mouse_click(MouseButton::Left)), LinkClick::Ignore);
        // 预览渲染出的每个链接都能被原样解回
        let rendered = aero_core::links::to_markdown("见 [[a b|别名]] 与 [[c#d]]");
        for url in ["aero://note/a%20b", "aero://note/c#d"] {
            assert!(rendered.contains(&format!("]({url})")), "{rendered}");
            assert!(matches!(link_click(url, &mouse_click(MouseButton::Left)), LinkClick::Note { .. }));
        }
    }
}
