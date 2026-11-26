//! 用户上色的颜色令牌（`ui_design.md` 13.3 的 `content.*`，调色板见 7.7）。
//!
//! 9 色 × 文字 / 背景 × 浅色 / 深色，源码里的类名（`{.red}` / `{.bg-blue}`）由
//! [`aero_core::colors`] 负责，这里只回答"这个类名在当前主题下是什么颜色"。
//!
//! 对比度按 WCAG AA（4.5:1）取值，两种主题下都满足两件事：文字色在编辑区底色上可读，
//! 正文色在背景色上可读。文字色与背景色叠加（红字 + 黄底）时不保证 AA——与 Notion 一样，
//! 那是用户自己的选择。

use aero_core::block::Attrs;
use aero_core::colors::{PALETTE, Slot};
use gpui_kit::component::ActiveTheme as _;
use gpui_kit::{App, Hsla, rgb};

/// 文字色（浅色主题）：在 `#ffffff` 上都 ≥ 4.5:1。
const TEXT_LIGHT: [u32; 9] =
    [0x747371, 0x996750, 0xb05d0b, 0x966b23, 0x427f5e, 0x317aa4, 0x8c62ab, 0xbb4a86, 0xc74743];
/// 文字色（深色主题）：提亮而不是反相，在 `#0a0a0a` 上都 ≥ 4.5:1。
const TEXT_DARK: [u32; 9] =
    [0x9b9b9b, 0xba856f, 0xc77d48, 0xca9849, 0x529e72, 0x5e87c9, 0x9d68d3, 0xd15796, 0xdf5452];
/// 背景色（浅色主题）：正文色 `#0a0a0a` 在上面都 ≥ 4.5:1。`yellow` 同时是 `==` 的高亮色。
const BG_LIGHT: [u32; 9] =
    [0xe5e5e5, 0xe7d3c0, 0xfed7aa, 0xfef08a, 0xbbf7d0, 0xbfdbfe, 0xe9d5ff, 0xfbcfe8, 0xfecaca];
/// 背景色（深色主题）：降饱和压暗而不是反相，正文色 `#fafafa` 在上面都 ≥ 4.5:1。
const BG_DARK: [u32; 9] =
    [0x3a3a3a, 0x55402f, 0x7a4a1b, 0x66551c, 0x2e5b40, 0x2c4a75, 0x4b3a6e, 0x6b2c4e, 0x6e2b2b];

fn value(slot: Slot, color: &str, dark: bool) -> Option<u32> {
    let ix = PALETTE.iter().position(|&c| c == color)?;
    Some(match (slot, dark) {
        (Slot::Text | Slot::Border, false) => TEXT_LIGHT[ix],
        (Slot::Text | Slot::Border, true) => TEXT_DARK[ix],
        (Slot::Background, false) => BG_LIGHT[ix],
        (Slot::Background, true) => BG_DARK[ix],
    })
}

/// 调色板里某个颜色在这一组、这套主题下的色值。类名不在调色板里时返回 `None`（不渲染）。
pub fn token(slot: Slot, color: &str, dark: bool) -> Option<Hsla> {
    if let Some(c) = aero_core::colors::CustomColor::parse(color) {
        return Some(
            gpui_kit::Rgba {
                r: c.red as f32 / 255.,
                g: c.green as f32 / 255.,
                b: c.blue as f32 / 255.,
                a: c.alpha,
            }
            .into(),
        );
    }
    value(slot, color, dark).map(|v| rgb(v).into())
}

/// CSS hex for the native HTML renderer, which accepts hex colors in spans.
fn hex(slot: Slot, color: &str, dark: bool) -> Option<String> {
    if let Some(color) = aero_core::colors::normalize_custom(color) {
        if color.starts_with('#') {
            return Some(color);
        }
        let c = aero_core::colors::CustomColor::parse(&color)?;
        // Only the HTML renderer needs an 8-bit alpha. The Markdown source and presenter retain the decimal value.
        let alpha = (c.alpha * 255.).round() as u8;
        return Some(format!("#{:02X}{:02X}{:02X}{alpha:02X}", c.red, c.green, c.blue));
    }
    value(slot, color, dark).map(|v| format!("#{v:06x}"))
}

/// 一个块或一段文字要用的颜色。两组各自独立，可以只有一个。
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Paint {
    pub text: Option<Hsla>,
    pub background: Option<Hsla>,
}

impl Paint {
    /// 块颜色向下传递（6.3.10）：祖先的文字色在本块没写文字色时生效；
    /// 背景不往下传——祖先的背景矩形已经把子块盖在里面了。
    pub fn inherit(self, text: Option<Hsla>) -> Self {
        Self { text: self.text.or(text), ..self }
    }
}

/// 属性列表中的有效预设色或自定义色。
pub fn of(attrs: &Attrs, cx: &App) -> Paint {
    let dark = cx.theme().mode.is_dark();
    let color = |slot: Slot| slot.color_of(attrs).and_then(|c| token(slot, c, dark));
    Paint { text: color(Slot::Text), background: color(Slot::Background) }
}

/// 「上次使用的颜色」（7.7）：`⌘⇧H` 应用它，颜色菜单里作为第一行显示。
/// 默认是黄色背景，也就是 `==` 那条路——没用过菜单时 `⌘⇧H` 与从前完全一样。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Last {
    pub slot: Slot,
    /// 调色板里的颜色名，始终是 [`PALETTE`] 中的一项
    pub color: String,
}

impl Default for Last {
    fn default() -> Self {
        Self { slot: Slot::Background, color: "yellow".into() }
    }
}

impl gpui_kit::Global for Last {}

pub fn last(cx: &App) -> Last {
    cx.try_global::<Last>().cloned().unwrap_or_default()
}

/// 记住这次用的颜色。颜色菜单与工具条上色后调用；调色板外的颜色不记。
pub fn remember(slot: Slot, color: &str, cx: &mut App) {
    if PALETTE.contains(&color) || aero_core::colors::CustomColor::parse(color).is_some() {
        cx.set_global(Last { slot, color: color.into() });
    }
}

/// `==` 高亮的底色，等于 `content.bg.yellow`（13.3）。
pub fn highlight(cx: &App) -> Hsla {
    token(Slot::Background, "yellow", cx.theme().mode.is_dark()).expect("yellow 在调色板里")
}

/// 富文本预览的上色：把 [`aero_core::highlight::preview`] 生成的类名换成当前主题的色值。
///
/// core 不认识主题，只写 `<span class="red bg-blue">` 与 `<mark>`；这里换成 gpui-base 认得的
/// `style` / `color` 属性。类名来自调色板，不需要转义。
pub fn paint_html(html: &str, dark: bool) -> String {
    let mut out = html.replace("<mark>", &format!("<mark color=\"{}\">", highlight_hex(dark)));
    while let Some(at) = out.find("<span class=\"") {
        let open = at + "<span class=\"".len();
        let Some(end) = out[open..].find('"').map(|i| open + i) else { break };
        let mut style = String::new();
        for class in out[open..end].split(' ') {
            let (slot, color) = match class.strip_prefix("bg-") {
                Some(color) => (Slot::Background, color),
                None => (Slot::Text, class),
            };
            if let Some(hex) = hex(slot, color, dark) {
                let property = match slot {
                    Slot::Text => "color",
                    Slot::Background => "background-color",
                    Slot::Border => "border-color",
                };
                style.push_str(&format!("{property}:{hex};"));
            }
        }
        out.replace_range(at..end + 1, &format!("<span style=\"{style}\""));
    }
    out
}

fn highlight_hex(dark: bool) -> String {
    hex(Slot::Background, "yellow", dark).expect("yellow 在调色板里")
}

/// 拖出的文字选区。主题会把选区压到 30% 透明度，这里改成不透明的紫色。
pub fn apply_text_selection(cx: &mut App) {
    let color = Hsla::from(rgb(0xa855f7)).alpha(1.);
    let theme = gpui_kit::component::Theme::global_mut(cx);
    theme.selection = color;
    gpui_kit::base::Theme::global_mut(cx).tokens.colors.selection = color;
}

#[cfg(test)]
mod tests {
    use super::*;

    /// sRGB 相对亮度（WCAG 2.1）。
    fn luminance(color: u32) -> f32 {
        let channel = |shift: u32| {
            let c = ((color >> shift) & 0xff) as f32 / 255.;
            if c <= 0.03928 { c / 12.92 } else { ((c + 0.055) / 1.055).powf(2.4) }
        };
        0.2126 * channel(16) + 0.7152 * channel(8) + 0.0722 * channel(0)
    }

    fn contrast(a: u32, b: u32) -> f32 {
        let (a, b) = (luminance(a), luminance(b));
        (a.max(b) + 0.05) / (a.min(b) + 0.05)
    }

    /// 编辑区底色与正文色（gpui-component 的 Default Light / Dark）：令牌按它们取的值。
    const LIGHT: (u32, u32) = (0xffffff, 0x0a0a0a);
    const DARK: (u32, u32) = (0x0a0a0a, 0xfafafa);

    /// 13.3 要求 `content.*` 在两种主题下都满足 AA：文字色对底色，正文色对背景色。
    #[test]
    fn content_tokens_meet_aa_contrast_in_both_themes() {
        for (mode, (background, foreground), text, bg) in
            [("浅色", LIGHT, TEXT_LIGHT, BG_LIGHT), ("深色", DARK, TEXT_DARK, BG_DARK)]
        {
            for (color, &value) in PALETTE.iter().zip(text.iter()) {
                let ratio = contrast(value, background);
                assert!(ratio >= 4.5, "{mode} content.text.{color} 对比度 {ratio:.2}");
            }
            for (color, &value) in PALETTE.iter().zip(bg.iter()) {
                let ratio = contrast(value, foreground);
                assert!(ratio >= 4.5, "{mode} content.bg.{color} 对比度 {ratio:.2}");
                // 背景色要看得出来，但不能盖掉正文：与编辑区底色明显不同
                assert!(contrast(value, background) >= 1.1, "{mode} content.bg.{color} 与底色太接近");
            }
        }
    }

    /// 令牌覆盖整个调色板，深浅两套各不相同；`bg.yellow` 就是 `==` 的高亮色。
    #[test]
    fn every_palette_color_has_a_token_in_both_slots() {
        for color in PALETTE {
            for dark in [false, true] {
                assert!(token(Slot::Text, color, dark).is_some(), "{color}");
                assert!(token(Slot::Background, color, dark).is_some(), "{color}");
            }
            assert_ne!(token(Slot::Text, color, false), token(Slot::Text, color, true), "{color}");
        }
        // 调色板外的类名不上色
        for class in ["custom", "bg-blue", "", "chartreuse"] {
            assert_eq!(token(Slot::Text, class, false), None, "{class}");
        }
        assert_eq!(highlight_hex(false), "#fef08a");
        assert_eq!(highlight_hex(true), "#66551c");
    }

    /// 预览 HTML 的上色：core 写类名，这里换成色值；`<mark>` 与 `bg-yellow` 同色。
    #[test]
    fn paint_html_resolves_palette_classes_to_theme_colors() {
        assert_eq!(
            paint_html("<p><span class=\"red\">文字</span></p>", false),
            "<p><span style=\"color:#c74743;\">文字</span></p>"
        );
        assert_eq!(
            paint_html("<p><span class=\"red bg-blue\">文字</span></p>", true),
            "<p><span style=\"color:#df5452;background-color:#2c4a75;\">文字</span></p>"
        );
        // 未知类名不上色，但 span 保留（不影响文字）
        assert_eq!(paint_html("<span class=\"custom\">x</span>", false), "<span style=\"\">x</span>");
        // `==` 的 `<mark>` 与 `{.bg-yellow}` 落到同一个令牌
        let marked = paint_html("<p><mark>a</mark><span class=\"bg-yellow\">b</span></p>", false);
        assert_eq!(
            marked,
            "<p><mark color=\"#fef08a\">a</mark><span style=\"background-color:#fef08a;\">b</span></p>"
        );
        assert_eq!(
            paint_html(r##"<span class="#123456 bg-rgba(12,34,56,0.3749)">字</span>"##, false),
            r#"<span style="color:#123456;background-color:#0C223860;">字</span>"#
        );
    }
}
