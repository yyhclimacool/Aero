//! 文字类块不分浏览和编辑两套画法（方案 9.1）：段落、标题、列表项、引用、callout 的标题与正文、
//! 表格单元格，都由同一个文本控件来画，没聚焦时只是没有光标。点进去不换控件，字的位置、粗细、
//! 折行都不会变；按下鼠标就是在这个控件里放光标，拖动直接选字。

use aero_core::markup::{Analysis, Mark};
use gpui_kit::base::input::Markup;
use gpui_kit::base::text::{LinkStyle, LinkUnderline};
use gpui_kit::component::WindowExt as _;

use super::*;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(super) enum Part {
    Unit,
    CalloutTitle,
    CalloutBody,
    Cell(usize, usize),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(super) struct Key {
    pub id: BlockId,
    pub part: Part,
}

impl Key {
    pub fn unit(id: BlockId) -> Self {
        Key { id, part: Part::Unit }
    }
}

/// 同一段源码在界面上额外的样子。
#[derive(Clone, Debug, Default, PartialEq)]
pub(super) struct Extra {
    /// 已勾选的任务：整段淡色加删除线。
    pub checked: bool,
    /// 空文字时显示的提示（callout 的默认标题）。
    pub placeholder: Option<SharedString>,
    /// 代码块里的代码：原样显示，不当 Markdown 解析。
    pub code: bool,
}

pub(super) struct Presenter {
    pub textarea: Entity<TextareaState>,
    /// 控件文字对应的文档区间（上次同步时）
    pub range: Range<usize>,
    pub enter_newline: bool,
    /// 链接文字在控件文字里的区间与地址
    pub links: Vec<(Range<usize>, String)>,
    /// 不折行时文字的宽度。只给居中、靠右的单元格量，它们按这个宽度摆位置。
    pub natural_width: Option<Pixels>,
    text: String,
    extra: Extra,
    styled: Option<StyleStamp>,
    _focus: Subscription,
}

/// 决定标记样式的输入。任何一项变了才重算，免得每帧重新解析每一段。
#[derive(Clone, PartialEq)]
struct StyleStamp {
    text: String,
    extra: Extra,
    dark: bool,
    /// 编辑中那一段的选区：落进链接地址时地址要露出来
    selection: Option<Range<usize>>,
    links: Vec<LinkStyle>,
}

pub(super) struct Spec {
    pub key: Key,
    pub range: Range<usize>,
    pub enter_newline: bool,
    pub extra: Extra,
    /// 要量不折行宽度：`Some(字重)`
    pub measure: Option<FontWeight>,
}

impl Editor {
    /// 每帧渲染前：给每段文字类块备好控件，文字与样式跟文档对齐。
    pub(super) fn sync_presenters(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if !self.hides_markers(cx) {
            self.presenters.clear();
            return;
        }
        let specs = self.presenter_specs(cx);
        let mut live = HashSet::new();
        for spec in specs {
            live.insert(spec.key);
            self.ensure_presenter(spec, window, cx);
        }
        let focused = self.focused.as_ref().map(|f| f.textarea.clone());
        self.presenters.retain(|key, p| live.contains(key) || focused.as_ref() == Some(&p.textarea));
    }

    fn presenter_specs(&self, cx: &App) -> Vec<Spec> {
        let mut specs = Vec::new();
        let spec =
            |key, range, enter_newline, extra| Spec { key, range, enter_newline, extra, measure: None };
        for unit in self.viewport.unit_ranges.iter().flat_map(|range| &self.units[range.clone()]) {
            if self.block_shows_source(unit.id) {
                continue;
            }
            let text = self.doc.slice(unit.range.clone()).to_string();
            let key = Key::unit(unit.id);
            let range = unit.range.clone();
            match &unit.kind {
                BlockKind::Paragraph | BlockKind::Heading { .. }
                    if self.presents_text(unit.id, &text, cx) =>
                {
                    let checked = unit.list.as_ref().is_some_and(|list| list.checked == Some(true));
                    specs.push(spec(key, range, false, Extra { checked, ..Default::default() }));
                }
                BlockKind::ListItem { .. } => specs.push(spec(key, range, false, Extra::default())),
                BlockKind::BlockQuote if simple_quote(&text) => {
                    specs.push(spec(key, range, true, Extra::default()))
                }
                BlockKind::CodeFence { info } => {
                    if let Some(inner) = self.editable_fence_body(unit.id, info, &text) {
                        let inner = unit.range.start + inner.start..unit.range.start + inner.end;
                        specs.push(spec(key, inner, true, Extra { code: true, ..Default::default() }));
                    }
                }
                BlockKind::Callout { .. } => {
                    let Some(parts) = callout_edit_parts(&text) else { continue };
                    let (label, _, _) = preview::appearance(&parts.kind, cx);
                    let base = unit.range.start;
                    let title = base + parts.title.start..base + parts.title.end;
                    let extra = Extra { placeholder: Some(label.into()), ..Default::default() };
                    specs.push(spec(Key { id: unit.id, part: Part::CalloutTitle }, title, false, extra));
                    if parts.simple_body && !has_rich_block(&parts.body_text) {
                        let body = base + parts.body.start..base + parts.body.end;
                        specs.push(spec(
                            Key { id: unit.id, part: Part::CalloutBody },
                            body,
                            true,
                            Extra::default(),
                        ));
                    }
                }
                BlockKind::Table if presents_table(&text) => {
                    let Some(grid) = table::parse(&text) else { continue };
                    let align = grid.align();
                    for (row, cells) in grid.data_rows().enumerate() {
                        for (col, cell) in cells.iter().enumerate() {
                            let aligned = align.get(col).is_some_and(|a| *a != table::Align::Left);
                            let weight = if row == 0 { FontWeight::SEMIBOLD } else { FontWeight::NORMAL };
                            specs.push(Spec {
                                key: Key { id: unit.id, part: Part::Cell(row, col) },
                                range: unit.range.start + cell.start..unit.range.start + cell.end,
                                enter_newline: false,
                                extra: Extra::default(),
                                measure: aligned.then_some(weight),
                            });
                        }
                    }
                }
                _ => {}
            }
        }
        specs
    }

    /// 这一段按文字画。独占一段的图片 / 笔记嵌入与带行内公式的段落另有画法。
    fn presents_text(&self, id: BlockId, text: &str, cx: &App) -> bool {
        let trimmed = text.trim();
        !preview::is_image_embed(trimmed)
            && !preview::is_note_embed(trimmed)
            && aero_core::rich_content::pdf_embed(text).is_none()
            && !has_html_break(text)
            && !self.renders_inline_formulas(id, text, cx)
            && (self.is_focused_block(id) || !self.has_navigation_link(text))
    }

    fn ensure_presenter(&mut self, spec: Spec, window: &mut Window, cx: &mut Context<Self>) {
        let text = self.doc.slice(spec.range.clone()).to_string();
        let focused = self
            .focused
            .as_ref()
            .filter(|f| f.id == Some(spec.key.id) && f.range == spec.range)
            .map(|f| f.textarea.clone());
        let stale = self
            .presenters
            .get(&spec.key)
            .is_none_or(|p| focused.as_ref().is_some_and(|ta| p.textarea != *ta));
        if stale {
            let textarea = focused.clone().unwrap_or_else(|| {
                cx.new(|cx| {
                    let mut ta = TextareaState::new(window, cx).auto_grow(1, 100_000);
                    ta.set_context_menu_enabled(false);
                    // 浏览态按 Tab 走的是按钮（折叠 callout 等），不是一段段文字
                    ta.set_tab_stop(false);
                    ta
                })
            });
            let key = spec.key;
            let focus =
                cx.subscribe_in(&textarea, window, move |this, ta, event: &InputEvent, window, cx| {
                    if matches!(event, InputEvent::Focus) {
                        this.adopt_presenter(key, ta, window, cx);
                    }
                });
            let presenter = Presenter {
                textarea,
                range: spec.range.clone(),
                enter_newline: spec.enter_newline,
                links: Vec::new(),
                natural_width: None,
                text: String::new(),
                extra: Extra::default(),
                styled: None,
                _focus: focus,
            };
            self.presenters.insert(spec.key, presenter);
        }
        let is_focused =
            self.focused.as_ref().is_some_and(|f| f.textarea == self.presenters[&spec.key].textarea);
        let alignment = self
            .doc
            .blocks()
            .find(spec.key.id)
            .map(|block| {
                let attrs = self.preview_attrs(block.id, &block.attrs);
                let attrs = if attrs.pairs.iter().any(|(key, _)| key == "align") {
                    attrs
                } else {
                    aero_core::containers::ancestors(&self.doc, block.id)
                        .last()
                        .filter(|parent| matches!(parent.kind, BlockKind::ListItem { .. }))
                        .map_or(attrs, |p| self.preview_attrs(p.id, &p.attrs))
                };
                aero_core::properties::Alignment::of(&attrs)
            })
            .unwrap_or_default();
        let p = self.presenters.get_mut(&spec.key).expect("inserted above");
        p.textarea.update(cx, |ta, cx| ta.set_disabled(self.io.suspended, cx));
        if spec.key.part == Part::Unit && !spec.extra.code {
            p.textarea.update(cx, |ta, cx| {
                use aero_core::properties::Alignment;
                ta.set_text_align(
                    match alignment {
                        Alignment::Center => TextAlign::Center,
                        Alignment::Right => TextAlign::Right,
                        _ => TextAlign::Left,
                    },
                    cx,
                );
                ta.set_justify(alignment == Alignment::Justify, cx);
            });
        }
        p.range = spec.range;
        p.enter_newline = spec.enter_newline;
        if p.text != text || p.extra != spec.extra || p.styled.is_none() {
            let placeholder = spec.extra.placeholder.clone();
            let changed_text = !is_focused;
            p.textarea.update(cx, |ta, cx| {
                if changed_text && ta.value().as_ref() != text {
                    ta.set_value(text.clone(), window, cx);
                }
                ta.set_placeholder(placeholder.unwrap_or_default(), window, cx);
            });
            p.text = text;
            p.extra = spec.extra;
            p.natural_width = None;
        }
        if let Some(weight) = spec.measure
            && p.natural_width.is_none()
        {
            let current = p.textarea.read(cx).value().to_string();
            let visible = aero_core::markup::analyze(&current).visible_text(&current);
            let style = window.text_style();
            let mut font = style.font();
            font.weight = weight;
            let size = style.font_size.to_pixels(window.rem_size());
            let width = visible
                .split('\n')
                .map(|line| {
                    let run = TextRun {
                        len: line.len(),
                        font: font.clone(),
                        color: style.color,
                        ..Default::default()
                    };
                    window.text_system().shape_line(line.to_owned().into(), size, &[run], None).width
                })
                .fold(px(0.), Pixels::max);
            // 文字控件在右侧留了光标的余量，量出来的宽度要把它加上，否则会被折行
            p.natural_width = Some(width + px(12.));
        }
        self.restyle_presenter(spec.key, cx);
    }

    /// 标记样式跟着文字、主题和链接解析结果走；都没变就不重算。
    pub(super) fn restyle_presenter(&mut self, key: Key, cx: &mut Context<Self>) {
        let Some(p) = self.presenters.get(&key) else { return };
        let ta = p.textarea.read(cx);
        let text = ta.value().to_string();
        let editing = self.focused.as_ref().is_some_and(|f| f.textarea == p.textarea);
        let selection = editing.then(|| ta.selected_range());
        let dark = cx.theme().mode.is_dark();
        let link_styles = |links: &[(Range<usize>, String)]| -> Vec<LinkStyle> {
            links.iter().map(|(_, url)| self.preview_link_style(url, cx)).collect()
        };
        let preview = self.inline_color_preview(&p.range);
        if preview.is_none()
            && let Some(stamp) = &p.styled
            && stamp.text == text
            && stamp.extra == p.extra
            && stamp.dark == dark
            && stamp.selection == selection
            && stamp.links == link_styles(&p.links)
        {
            return;
        }
        let mut analysis = self.body_analysis(Some(key.id), &text, selection.clone());
        if p.extra.code {
            analysis = aero_core::markup::analyze("");
            analysis.conceal = aero_core::containers::prefix_ranges(
                &text,
                &aero_core::containers::continuation(&self.doc, key.id),
            );
        }
        let links = analysis.links.clone();
        let mut markup = self.presenter_markup(&text, &analysis, &p.extra, cx);
        let previewing = preview.is_some();
        if let Some((range, slot, color)) = preview {
            let color = crate::colors::token(slot, &color, dark);
            markup.styles.push((
                range,
                HighlightStyle {
                    color: (slot == aero_core::colors::Slot::Text).then_some(color).flatten(),
                    background_color: (slot == aero_core::colors::Slot::Background)
                        .then_some(color)
                        .flatten(),
                    ..Default::default()
                },
            ));
        }
        let stamp = StyleStamp { text, extra: p.extra.clone(), dark, selection, links: link_styles(&links) };
        let p = self.presenters.get_mut(&key).expect("checked above");
        p.links = links;
        p.styled = (!previewing).then_some(stamp);
        p.textarea.update(cx, |ta, cx| ta.set_markup(markup, cx));
    }

    /// 源码 → 界面样式。藏起全部成对标记，剩下的字按格式上色、换字体。
    pub(super) fn presenter_markup(
        &self,
        text: &str,
        analysis: &Analysis,
        extra: &Extra,
        cx: &App,
    ) -> Markup {
        use aero_core::colors::Slot;
        if extra.code {
            return Markup { conceal: analysis.conceal.clone(), ..Default::default() };
        }
        let theme = cx.theme();
        let dark = theme.mode.is_dark();
        let mut fonts = Vec::new();
        let mut dashed = Vec::new();
        let mut styles: Vec<(Range<usize>, HighlightStyle)> = Vec::new();
        if extra.checked && !text.is_empty() {
            styles.push((
                0..text.len(),
                HighlightStyle {
                    color: Some(theme.muted_foreground),
                    strikethrough: Some(gpui_kit::StrikethroughStyle { thickness: px(1.), color: None }),
                    ..Default::default()
                },
            ));
        }
        for (range, mark) in &analysis.styles {
            let style = match mark {
                Mark::Bold => HighlightStyle { font_weight: Some(FontWeight::BOLD), ..Default::default() },
                Mark::Italic => {
                    HighlightStyle { font_style: Some(gpui_kit::FontStyle::Italic), ..Default::default() }
                }
                Mark::Strike => HighlightStyle {
                    strikethrough: Some(gpui_kit::StrikethroughStyle { thickness: px(1.), color: None }),
                    ..Default::default()
                },
                Mark::Code => {
                    fonts.push((range.clone(), theme.mono_font_family.clone()));
                    HighlightStyle { background_color: Some(theme.muted), ..Default::default() }
                }
                Mark::Highlight => HighlightStyle {
                    background_color: Some(crate::colors::highlight(cx)),
                    ..Default::default()
                },
                Mark::Link => {
                    let url =
                        analysis.links.iter().find(|(label, _)| label == range).map(|(_, url)| url.as_str());
                    let style = url.map(|url| self.preview_link_style(url, cx));
                    let color = style.as_ref().and_then(|s| s.color).unwrap_or(theme.link);
                    // 找不到目标的链接画虚线，与预览一致
                    let underline = match style.map_or(LinkUnderline::Solid, |s| s.underline) {
                        LinkUnderline::Solid => Some(gpui_kit::UnderlineStyle {
                            thickness: px(1.),
                            color: Some(color),
                            wavy: false,
                        }),
                        LinkUnderline::Dashed => {
                            dashed.push((range.clone(), color));
                            None
                        }
                        LinkUnderline::None => None,
                    };
                    HighlightStyle { color: Some(color), underline, ..Default::default() }
                }
                Mark::Color { text, background } => HighlightStyle {
                    color: text.as_deref().and_then(|value| crate::colors::token(Slot::Text, value, dark)),
                    background_color: background
                        .as_deref()
                        .and_then(|value| crate::colors::token(Slot::Background, value, dark)),
                    ..Default::default()
                },
            };
            styles.push((range.clone(), style));
        }
        Markup {
            conceal: analysis.conceal.clone(),
            hard_breaks: analysis.hard_breaks.clone(),
            styles,
            fonts,
            dashed,
        }
    }

    pub(super) fn body_analysis(
        &self,
        id: Option<BlockId>,
        text: &str,
        selection: Option<Range<usize>>,
    ) -> Analysis {
        let mut analysis = aero_core::markup::analyze_editing(text, selection);
        if let Some(id) = id {
            let prefix = aero_core::containers::continuation(&self.doc, id);
            analysis.conceal.extend(aero_core::containers::prefix_ranges(text, &prefix));
            analysis.conceal.sort_by_key(|r| (r.start, r.end));
        }
        analysis
    }

    /// 正在编辑代码块里的代码：不配对括号、不弹链接和标签补全，控件里的文字已经没有围栏可供判断。
    pub(super) fn editing_code(&self) -> bool {
        let Some(f) = self.focused.as_ref() else { return false };
        let unit = f.id.and_then(|id| self.units.iter().find(|u| u.id == id));
        unit.is_some_and(|u| matches!(u.kind, BlockKind::CodeFence { .. }) && u.range != f.range)
    }

    pub(super) fn editable_fence_body(&self, id: BlockId, info: &str, text: &str) -> Option<Range<usize>> {
        if info.split_whitespace().next() == Some("mermaid")
            && let Some(local) = aero_core::containers::local_source(&self.doc, id)
            && aero_core::rich_content::mermaid_source(&local).is_some()
        {
            return None;
        }
        let prefix = aero_core::containers::continuation(&self.doc, id);
        fence_body(text, &prefix)
    }

    /// 这个区间是不是某个控件正在画的那一段。
    pub(super) fn presenter_for(&self, id: Option<BlockId>, range: &Range<usize>) -> Option<Key> {
        let id = id?;
        self.presenters.iter().find(|(key, p)| key.id == id && p.range == *range).map(|(key, _)| *key)
    }

    pub(super) fn presenter_key_of(&self, textarea: &Entity<TextareaState>) -> Option<Key> {
        self.presenters.iter().find(|(_, p)| p.textarea == *textarea).map(|(key, _)| *key)
    }

    /// 控件自己拿到了焦点（鼠标按下、Tab）：把它当成当前编辑的那一段。
    pub(super) fn adopt_presenter(
        &mut self,
        key: Key,
        textarea: &Entity<TextareaState>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.focused.as_ref().is_some_and(|f| f.textarea == *textarea) {
            return;
        }
        let Some(p) = self.presenters.get(&key).filter(|p| p.textarea == *textarea) else { return };
        let (range, enter_newline) = (p.range.clone(), p.enter_newline);
        self.exit_edit(cx);
        self.selection = None;
        self.selected_node = None;
        self.install_focus(range, Some(key.id), enter_newline, textarea.clone(), window, cx);
    }

    /// 鼠标按在一段文字上：控件已经放好光标并拿到焦点（同一次派发里、先于这里），这里只认领它。
    /// 焦点事件要等下一帧画完才发，而拖选从按下就开始，所以不等它。
    fn presenter_mouse_down(&mut self, key: Key, window: &mut Window, cx: &mut Context<Self>) {
        let Some(textarea) = self.presenters.get(&key).map(|p| p.textarea.clone()) else { return };
        if textarea.focus_handle(cx).is_focused(window) {
            self.adopt_presenter(key, &textarea, window, cx);
        }
    }

    /// 点在一段文字外面的留白（标题上方、单元格内边距）：光标放到离指针最近的位置。
    pub(super) fn focus_presenter_near(
        &mut self,
        key: Key,
        position: Point<Pixels>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(p) = self.presenters.get(&key) else { return };
        if self.focused.as_ref().is_some_and(|f| f.textarea == p.textarea) {
            return;
        }
        let ta = p.textarea.read(cx);
        let bounds = ta.input_bounds();
        let inside = point(
            position.x.clamp(bounds.left(), bounds.right() - px(1.)),
            position.y.clamp(bounds.top(), bounds.bottom() - px(1.)),
        );
        let cursor = ta.offset_for_point(inside).unwrap_or(p.text.len());
        let (range, nl) = (p.range.clone(), p.enter_newline);
        self.focus_range(range, Some(key.id), nl, cursor, window, cx);
    }

    /// 没在编辑的一段里，指针下的链接地址与它的矩形。
    fn link_at(&self, key: Key, position: Point<Pixels>, cx: &App) -> Option<(String, Bounds<Pixels>)> {
        let p = self.presenters.get(&key)?;
        if p.links.is_empty() {
            return None;
        }
        let ta = p.textarea.read(cx);
        let offset = ta.offset_for_point(position)?;
        let text = ta.value();
        let prev = text[..offset.min(text.len())].char_indices().next_back().map_or(0, |(i, _)| i);
        [offset, prev].into_iter().find_map(|at| {
            let (range, url) = p.links.iter().find(|(range, _)| range.contains(&at))?;
            let end = text[at..].chars().next().map_or(at, |c| at + c.len_utf8());
            ta.range_to_bounds(&(at..end)).filter(|b| b.contains(&position))?;
            Some((url.clone(), ta.range_to_bounds(range)?))
        })
    }

    /// 一段文字的控件，外面包着认领焦点与链接的处理。
    pub(super) fn render_presenter(&self, key: Key, cx: &mut Context<Self>) -> Option<AnyElement> {
        let p = self.presenters.get(&key)?;
        let focused = self.focused.as_ref().is_some_and(|f| f.textarea == p.textarea);
        let has_links = !p.links.is_empty();
        if !focused && has_links && self.has_navigation_link(&p.text) {
            let range = p.range.clone();
            let newline = p.enter_newline;
            let view = preview::markdown(
                format!("paragraph-link-{}-{:?}", key.id.0, key.part).into(),
                self.navigation_markdown(&p.text, None).into(),
                self.text_view_style(cx).with_paragraph_gap(rems(0.)),
                preview::PreviewContext {
                    editor: cx.entity(),
                    state: self.preview.clone(),
                    block: key.id,
                    reveal: 0,
                    origin: None,
                },
            );
            return Some(
                div()
                    .id(SharedString::from(format!("navigation-{}-{:?}", key.id.0, key.part)))
                    .w_full()
                    .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
                    .on_click(cx.listener(move |this, event: &ClickEvent, window, cx| {
                        if !event.is_right_click() {
                            cx.stop_propagation();
                            this.focus_range(range.clone(), Some(key.id), newline, range.len(), window, cx);
                        }
                    }))
                    .child(view)
                    .into_any_element(),
            );
        }
        let mut el = div()
            .id(SharedString::from(format!("text-{}-{:?}", key.id.0, key.part)))
            .relative()
            .w_full()
            .child(Textarea::new(&p.textarea))
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(move |this, _: &MouseDownEvent, window, cx| {
                    cx.stop_propagation();
                    this.presenter_mouse_down(key, window, cx);
                }),
            );
        if !focused && has_links {
            el = el
                .capture_any_mouse_down(cx.listener(move |this, event: &MouseDownEvent, window, cx| {
                    if event.button != MouseButton::Left || event.modifiers.shift || event.click_count > 1 {
                        return;
                    }
                    if let Some((url, _)) = this.link_at(key, event.position, cx) {
                        this.close_block_menu(cx);
                        cx.stop_propagation();
                        window.prevent_default();
                        this.pending_link = Some((key, url));
                    }
                }))
                .on_mouse_up(
                    MouseButton::Left,
                    cx.listener(move |this, event: &MouseUpEvent, window, cx| {
                        let Some((pending, url)) = this.pending_link.take() else { return };
                        if pending != key
                            || this.link_at(key, event.position, cx).is_none_or(|(u, _)| u != url)
                        {
                            return;
                        }
                        cx.stop_propagation();
                        this.follow_link(&url, window, cx);
                    }),
                );
        }
        if has_links {
            el = el
                .on_mouse_move(cx.listener(move |this, event: &MouseMoveEvent, window, cx| {
                    this.presenter_hover(key, Some(event.position), window, cx);
                }))
                .on_hover(cx.listener(move |this, hovered: &bool, window, cx| {
                    if !*hovered {
                        this.presenter_hover(key, None, window, cx);
                    }
                }));
        }
        if focused {
            if self.slash.is_some() {
                el = el.child(self.render_slash_menu(cx));
            }
            if self.wiki.is_some() {
                el = el.child(self.render_wiki_menu(cx));
            }
            if self.tag_menu.is_some() {
                el = el.child(self.render_tag_menu(cx));
            }
        }
        Some(el.into_any_element())
    }

    /// 指针在一段文字上移动（`None` 是移出去了）：进出链接时通知悬停预览。编辑中的那段不弹预览。
    fn presenter_hover(
        &mut self,
        key: Key,
        position: Option<Point<Pixels>>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let editing = self
            .presenters
            .get(&key)
            .is_some_and(|p| self.focused.as_ref().is_some_and(|f| f.textarea == p.textarea));
        let hit = position.filter(|_| !editing).and_then(|position| self.link_at(key, position, cx));
        let hit = hit.map(|(url, bounds)| (key, SharedString::from(url), bounds));
        if self.hovered_link == hit {
            return;
        }
        if let Some((_, url, bounds)) = self.hovered_link.take() {
            self.hover_link(&url, false, bounds, window, cx);
        }
        if let Some((_, url, bounds)) = &hit {
            self.hover_link(url, true, *bounds, window, cx);
        }
        self.hovered_link = hit;
    }

    /// 打开链接：库内指针和笔记链接交给工作区，单方括号地址交给浏览器。
    fn follow_link(&mut self, url: &str, window: &mut Window, cx: &mut Context<Self>) {
        if let Some((path, fragment)) = aero_core::links::decode_vault_url(url) {
            cx.emit(EditorEvent::VaultLink { path, fragment });
            return;
        }
        match aero_core::links::decode_note_url(url) {
            Some((target, fragment)) => self.follow_note(target, fragment, None, window, cx),
            None if url.starts_with(aero_core::links::NOTE_URL_PREFIX) => {}
            None => self.open_external_link(url, window, cx),
        }
    }

    pub(super) fn follow_note(
        &self,
        target: String,
        fragment: Option<String>,
        origin: Option<&VaultPath>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if origin.is_some() || fragment.as_deref().is_some_and(|f| f.starts_with('^')) {
            match self.resolve_reference(&target, fragment.as_deref(), origin) {
                Some(aero_core::links::Resolution::Resolved { path, fragment }) => {
                    cx.emit(EditorEvent::OpenNoteFragment { path, fragment });
                    return;
                }
                Some(aero_core::links::Resolution::Ambiguous { .. }) => {
                    window.push_notification(
                        gpui_kit::component::notification::Notification::warning("引用来源有歧义")
                            .title("Aero"),
                        cx,
                    );
                    return;
                }
                _ if origin.is_some() || self.embeds.resolver.is_some() => {
                    window.push_notification(
                        gpui_kit::component::notification::Notification::info("找不到引用来源").title("Aero"),
                        cx,
                    );
                    return;
                }
                _ => {}
            }
        }
        cx.emit(EditorEvent::LinkClicked { target, fragment });
    }
}

/// 围栏代码块里代码本身的区间（不含开头的 ```` ```lang ```` 行与结尾的围栏）。没有代码行时为 `None`：
/// 那时一打字就会写到结尾围栏的同一行上，仍用源码编辑。
fn fence_body(src: &str, prefix: &str) -> Option<Range<usize>> {
    let src = src.trim_end_matches(['\n', '\r']);
    let first = src.find('\n')?;
    let open = src[..first].trim_start();
    let fence = open.chars().next().filter(|c| *c == '`' || *c == '~')?;
    let fence_len = open.chars().take_while(|c| *c == fence).count();
    let start = first + 1;
    let rest = &src[start..];
    let closing = rest.rfind('\n').map_or(0, |i| i + 1);
    let last = super::text_ops::strip_continuation(&rest[closing..], prefix).trim();
    let closed = last.len() >= fence_len && last.chars().all(|c| c == fence);
    let end = if !closed {
        src.len()
    } else if closing == 0 {
        return None;
    } else {
        start + closing - 1
    };
    Some(start..end)
}

/// 这张表按单元格画、按单元格编辑。
pub(super) fn presents_table(text: &str) -> bool {
    table::parse(text).is_some()
}

/// 段落中的 HTML 换行仍走预览；表格单元格用 hard_breaks 保留源字节。
fn has_html_break(text: &str) -> bool {
    text.to_ascii_lowercase().contains("<br")
}

/// 引用里只有段落（没有列表、代码、嵌套引用）时按文字画；复杂的仍用源码编辑。
pub(super) fn simple_quote(text: &str) -> bool {
    let body = text
        .lines()
        .map(|line| {
            let body = line.trim_start().strip_prefix('>').unwrap_or(line);
            body.strip_prefix(' ').unwrap_or(body)
        })
        .collect::<Vec<_>>()
        .join("\n");
    body.lines().all(simple_body_line) && !has_rich_block(&body)
}

/// 按段落判断独立嵌入，避免把与文字混排的 PDF 误当成渲染块。
fn has_rich_block(text: &str) -> bool {
    aero_core::parse(text).blocks.iter().any(|block| {
        let source = &text[block.range.clone()];
        aero_core::rich_content::pdf_embed(source).is_some()
            || preview::is_note_embed(source)
            || aero_core::rich_content::mermaid_source(source).is_some()
    })
}

pub(super) fn simple_body_line(body: &str) -> bool {
    let t = body.trim_start();
    if t.starts_with('>')
        || t.starts_with('#')
        || t.starts_with("```")
        || t.starts_with("~~~")
        || t.starts_with('|')
    {
        return false;
    }
    if body.starts_with("    ") || body.starts_with('\t') {
        return false;
    }
    if ["- ", "* ", "+ "].iter().any(|m| t.starts_with(m)) || t == "-" || t == "*" {
        return false;
    }
    let digits = t.bytes().take_while(u8::is_ascii_digit).count();
    !(digits > 0 && matches!(t.as_bytes().get(digits), Some(b'.' | b')')))
}

#[cfg(test)]
mod tests {
    use super::fence_body;

    #[test]
    fn fence_body_is_the_code_between_the_fences() {
        let body = |src: &str| fence_body(src, "").map(|range| src[range].to_owned());
        assert_eq!(body("```rust\nfn main() {\n}\n```").as_deref(), Some("fn main() {\n}"));
        assert_eq!(body("~~~~\na\n~~~~\n").as_deref(), Some("a"));
        assert_eq!(body("```\nunclosed").as_deref(), Some("unclosed"));
        assert_eq!(body("```\n```"), None, "no code line to type into");

        let quoted = "```\n> first\n> second\n>```";
        let range = fence_body(quoted, "> ").unwrap();
        assert_eq!(&quoted[range], "> first\n> second");
    }
}
