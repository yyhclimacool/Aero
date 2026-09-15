use std::collections::HashMap;

use aero_core::{CalloutKind, Fold};
use gpui_kit::assets::IconName;
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::text::{MarkdownNode, TextView, TextViewStyle, markdown_ast};
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
) -> TextView {
    TextView::markdown(id.clone(), text)
        .style(style.clone())
        .selectable(false)
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
            render_callout(id, callout, style.clone(), reveal, &state, cx)
        })
        .w_full()
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
                    callout.body.clone().into(),
                    style,
                    reveal,
                    state.clone(),
                ),
            ))
        })
        .into_any_element()
}

#[cfg(test)]
mod tests {
    use super::parse_callout;
    use aero_core::{BlockKind, CalloutKind, Fold};

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
}
