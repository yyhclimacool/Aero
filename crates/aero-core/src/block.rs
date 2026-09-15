//! Block 模型（方案第 7 节）。Block 只持有区间，不复制文本。

use std::ops::Range;
use std::sync::Arc;

use crate::anchors::Anchor;

/// 会话内稳定的内部 id（7.3）。不持久化。
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct BlockId(pub u64);

/// Callout 类型（6.3.7）。未知类型保留原文，按 note 渲染。
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CalloutKind {
    Note,
    Tip,
    Warning,
    Danger,
    Info,
    Question,
    Example,
    Quote,
    Other(String),
}

impl CalloutKind {
    pub fn parse(s: &str) -> Self {
        match s.to_ascii_lowercase().as_str() {
            "note" => Self::Note,
            "tip" => Self::Tip,
            "warning" => Self::Warning,
            "danger" => Self::Danger,
            "info" => Self::Info,
            "question" => Self::Question,
            "example" => Self::Example,
            "quote" => Self::Quote,
            _ => Self::Other(s.to_owned()),
        }
    }
}

/// Callout 的折叠标记：`[!tip]-` 默认折叠，`[!tip]+` 默认展开。
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum Fold {
    #[default]
    None,
    Collapsed,
    Expanded,
}

/// 属性列表 `{.class #id key=value}`（6.3.10）。未知内容原样保留。
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Attrs {
    pub id: Option<String>,
    pub classes: Vec<String>,
    pub pairs: Vec<(String, String)>,
}

impl Attrs {
    pub fn is_empty(&self) -> bool {
        self.id.is_none() && self.classes.is_empty() && self.pairs.is_empty()
    }

    /// 解析 `{...}` 的内部（不含花括号）。按 6.5：全部 token 必须是 `.class`、`#id` 或
    /// `key=value`，否则整体不是属性列表，返回 `None`。
    pub fn parse_inner(inner: &str) -> Option<Attrs> {
        let mut attrs = Attrs::default();
        let mut saw_any = false;
        for tok in tokenize_attrs(inner)? {
            saw_any = true;
            if let Some(class) = tok.strip_prefix('.') {
                if class.is_empty() || !class.chars().all(is_attr_name_char) {
                    return None;
                }
                attrs.classes.push(class.to_owned());
            } else if let Some(id) = tok.strip_prefix('#') {
                if id.is_empty() || !id.chars().all(is_attr_name_char) {
                    return None;
                }
                attrs.id = Some(id.to_owned());
            } else if let Some((k, v)) = tok.split_once('=') {
                if k.is_empty() || !k.chars().all(is_attr_name_char) {
                    return None;
                }
                let v = v.strip_prefix('"').and_then(|v| v.strip_suffix('"')).unwrap_or(v);
                attrs.pairs.push((k.to_owned(), v.to_owned()));
            } else {
                return None;
            }
        }
        saw_any.then_some(attrs)
    }

    /// 合并另一组属性（后者的 id 覆盖前者）。
    pub fn merge(&mut self, other: Attrs) {
        if other.id.is_some() {
            self.id = other.id;
        }
        self.classes.extend(other.classes);
        self.pairs.extend(other.pairs);
    }
}

fn is_attr_name_char(c: char) -> bool {
    c.is_alphanumeric() || matches!(c, '-' | '_' | ':')
}

/// 按空白切 token，`key="a b"` 的双引号内不切。引号不闭合返回 `None`。
fn tokenize_attrs(inner: &str) -> Option<Vec<&str>> {
    let mut out = Vec::new();
    let mut start: Option<usize> = None;
    let mut in_quote = false;
    for (i, c) in inner.char_indices() {
        match (c, in_quote) {
            ('"', _) => {
                in_quote = !in_quote;
                start.get_or_insert(i);
            }
            (c, false) if c.is_whitespace() => {
                if let Some(s) = start.take() {
                    out.push(&inner[s..i]);
                }
            }
            _ => {
                start.get_or_insert(i);
            }
        }
    }
    if in_quote {
        return None;
    }
    if let Some(s) = start {
        out.push(&inner[s..]);
    }
    Some(out)
}

/// Block 类型（7.1）。
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum BlockKind {
    // ---- Leaf ----
    FrontMatter,
    Paragraph,
    Heading {
        level: u8,
    },
    /// 围栏代码块，`info` 是围栏后的 info string（不含属性列表）
    CodeFence {
        info: String,
    },
    /// 缩进代码块
    CodeBlock,
    /// 只含一个 `$$…$$` 的段落
    MathBlock,
    Table,
    ThematicBreak,
    HtmlBlock,
    /// 解析器无法解释、按字节透传的内容（链接引用定义等）
    Raw,
    /// 独占一行的 `^id` / `{属性列表}`，语义属于前一个兄弟块；UI 不显示
    Marker,

    // ---- Container ----
    List {
        ordered: bool,
        start: u32,
    },
    /// `task = Some(checked)` 表示任务列表项
    ListItem {
        task: Option<bool>,
    },
    BlockQuote,
    Callout {
        kind: CalloutKind,
        title: Option<String>,
        fold: Fold,
    },
    FootnoteDefinition {
        name: String,
    },
    /// `:::name` 容器（6.4.1）。v1 内容按普通 Markdown 渲染，围栏行不可见
    Directive {
        name: String,
    },
}

impl BlockKind {
    pub fn is_container(&self) -> bool {
        matches!(
            self,
            Self::List { .. }
                | Self::ListItem { .. }
                | Self::BlockQuote
                | Self::Callout { .. }
                | Self::FootnoteDefinition { .. }
                | Self::Directive { .. }
        )
    }

    /// 用于 BlockId 匹配与索引的类型标签，不含参数。
    pub fn tag(&self) -> &'static str {
        match self {
            Self::FrontMatter => "front_matter",
            Self::Paragraph => "paragraph",
            Self::Heading { .. } => "heading",
            Self::CodeFence { .. } => "code_fence",
            Self::CodeBlock => "code_block",
            Self::MathBlock => "math_block",
            Self::Table => "table",
            Self::ThematicBreak => "thematic_break",
            Self::HtmlBlock => "html_block",
            Self::Raw => "raw",
            Self::Marker => "marker",
            Self::List { .. } => "list",
            Self::ListItem { .. } => "list_item",
            Self::BlockQuote => "block_quote",
            Self::Callout { .. } => "callout",
            Self::FootnoteDefinition { .. } => "footnote_definition",
            Self::Directive { .. } => "directive",
        }
    }
}

/// 一个块。`range` 覆盖块的全部源文本（含 `^id`、属性列表等尾部标记）；
/// `suffix_len` 是尾部标记占用的字节数，编辑器显示正文时去掉。
#[derive(Clone, Debug)]
pub struct Block {
    pub id: BlockId,
    pub kind: BlockKind,
    pub range: Range<Anchor>,
    pub suffix_len: u32,
    /// 文件中的 `^id`（6.3.3），如有
    pub persisted_id: Option<Arc<str>>,
    pub attrs: Attrs,
    /// 块源文本的 xxh3 哈希，用于重解析后的身份匹配（7.3）与索引增量
    pub hash: u64,
    pub children: Vec<Block>,
}

/// 整篇文档的块树。
#[derive(Clone, Debug, Default)]
pub struct BlockTree {
    pub roots: Vec<Block>,
}

impl BlockTree {
    /// 先序遍历，附带深度（顶层为 0）。
    pub fn iter(&self) -> impl Iterator<Item = (usize, &Block)> {
        let mut stack: Vec<(usize, &Block)> = self.roots.iter().rev().map(|b| (0, b)).collect();
        std::iter::from_fn(move || {
            let (depth, b) = stack.pop()?;
            stack.extend(b.children.iter().rev().map(|c| (depth + 1, c)));
            Some((depth, b))
        })
    }

    pub fn len(&self) -> usize {
        self.iter().count()
    }

    pub fn is_empty(&self) -> bool {
        self.roots.is_empty()
    }

    pub fn find(&self, id: BlockId) -> Option<&Block> {
        self.iter().map(|(_, b)| b).find(|b| b.id == id)
    }

    /// 只遍历叶子块（编辑单位）。
    pub fn leaves(&self) -> impl Iterator<Item = &Block> {
        self.iter().map(|(_, b)| b).filter(|b| !b.kind.is_container())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn attrs_parse() {
        let a = Attrs::parse_inner(".red .bg-yellow #x k=v q=\"a b\"").unwrap();
        assert_eq!(a.classes, ["red", "bg-yellow"]);
        assert_eq!(a.id.as_deref(), Some("x"));
        assert_eq!(a.pairs, [("k".into(), "v".into()), ("q".into(), "a b".into())]);
        // `f(x) = {1, 2}` 之类不是属性列表
        assert_eq!(Attrs::parse_inner("1, 2"), None);
        assert_eq!(Attrs::parse_inner(""), None);
        assert_eq!(Attrs::parse_inner(".red word"), None);
    }

    #[test]
    fn callout_kind() {
        assert_eq!(CalloutKind::parse("WARNING"), CalloutKind::Warning);
        assert_eq!(CalloutKind::parse("todo"), CalloutKind::Other("todo".into()));
    }
}
