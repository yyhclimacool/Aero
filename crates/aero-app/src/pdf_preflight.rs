//! 有界的 PDF 预检，不是完整验证器或解码沙箱。
//! 原始词法检查看不到 ObjStm、内容流中的嵌套；hayro 0.7 的公共加载 API
//! 没有可配置的对象深度 / filter 输出预算。图像声明也不能约束编码内部尺寸。
use hayro::hayro_syntax::{
    Pdf,
    object::{Dict, Name, ObjRef, String as PdfString},
    reader::{Reader, ReaderExt},
};

use super::{MAX_PIXELS, MAX_SIDE};

const MAX_SOURCE_BYTES: usize = 128 * 1024 * 1024;
const MAX_DEPTH: usize = 128;
const INVALID_SYNTAX: &str = "PDF 词法结构无效或不受支持";
const INVALID_STREAM: &str = "PDF 数据流边界无效或不受支持";

fn source_size(len: usize) -> Result<(), String> {
    if len > MAX_SOURCE_BYTES {
        return Err("PDF 源文件过大（最多 128 MiB）".into());
    }
    Ok(())
}

// 只使用 hayro 的非递归叶子接口；禁止在这里 read/skip Object、Array 或 Dict。
enum Token<'a> {
    Open(u8),
    Close(u8),
    Name(Name<'a>),
    Integer(u32),
    Word(&'a [u8]),
    Other,
}

fn regular(b: u8) -> bool {
    !matches!(
        b,
        0 | b'\t'
            | b'\n'
            | 12
            | b'\r'
            | b' '
            | b'('
            | b')'
            | b'<'
            | b'>'
            | b'['
            | b']'
            | b'{'
            | b'}'
            | b'/'
            | b'%'
    )
}

fn token<'a>(r: &mut Reader<'a>) -> Result<Option<Token<'a>>, String> {
    r.skip_white_spaces_and_comments();
    let Some(b) = r.peek_byte() else { return Ok(None) };
    let value = match b {
        b'<' if r.forward_tag(b"<<").is_some() => Token::Open(b'<'),
        b'>' if r.forward_tag(b">>").is_some() => Token::Close(b'<'),
        b'[' | b']' => {
            r.forward();
            if b == b'[' { Token::Open(b'[') } else { Token::Close(b'[') }
        }
        b'(' | b'<' => {
            // String::skip 的 literal 括号计数和 hex 扫描均为迭代，且不解码 / 分配。
            r.skip::<PdfString<'_>>(false).ok_or(INVALID_SYNTAX)?;
            Token::Other
        }
        b'/' => Token::Name(r.read_without_context::<Name<'_>>().ok_or(INVALID_SYNTAX)?),
        _ if regular(b) => {
            // 把间接引用当作一个值；ObjRef 只读取整数和 R，不解析被引用对象。
            if matches!(b, b'0'..=b'9' | b'+') && r.read_without_context::<ObjRef>().is_some() {
                Token::Other
            } else {
                let start = r.offset();
                r.forward_while(regular);
                let word = &r.data[start..r.offset()];
                match std::str::from_utf8(word).ok().and_then(|s| s.parse::<u32>().ok()) {
                    Some(n) => Token::Integer(n),
                    None => Token::Word(word),
                }
            }
        }
        _ => return Err(INVALID_SYNTAX.into()),
    };
    Ok(Some(value))
}

fn stream_data(r: &mut Reader<'_>, length: Option<u32>) -> Result<(), String> {
    r.forward_tag(b"\r\n")
        .or_else(|| r.forward_tag(b"\n"))
        .or_else(|| r.forward_tag(b"\r"))
        .ok_or(INVALID_STREAM)?;
    if let Some(length) = length {
        // 直接 Length 优先，二进制中的伪 endstream / 括号不是语法。
        r.read_bytes(length as usize).ok_or(INVALID_STREAM)?;
        r.skip_white_spaces();
        r.forward_tag(b"endstream").ok_or(INVALID_STREAM)?;
    } else {
        // 间接 / 缺失 Length：和 hayro 的恢复路径一样定位第一个 endstream。
        // 后面必须是 endobj；有歧义就拒绝，不把剩余二进制当作结构括号。
        let offset = r
            .tail()
            .ok_or(INVALID_STREAM)?
            .windows(9)
            .position(|w| w == b"endstream")
            .ok_or(INVALID_STREAM)?;
        r.read_bytes(offset + 9).ok_or(INVALID_STREAM)?;
    }
    r.skip_white_spaces_and_comments();
    if !matches!(token(r)?, Some(Token::Word(b"endobj"))) {
        return Err(INVALID_STREAM.into());
    }
    Ok(())
}

/// O(源字节数) 扫描，固定 128 槽结构栈，无递归；必须在 Pdf::new 之前调用。
pub(super) fn source(bytes: &[u8]) -> Result<(), String> {
    source_size(bytes.len())?;
    let mut r = Reader::new(bytes);
    let mut stack = [0; MAX_DEPTH];
    let mut depth = 0;
    let mut length = None;
    let mut expect_key = true;
    let mut length_key = false;
    let mut closed_dict = false;
    while let Some(t) = token(&mut r)? {
        if closed_dict && matches!(t, Token::Word(b"stream")) {
            stream_data(&mut r, length)?;
            closed_dict = false;
            continue;
        }
        closed_dict = false;
        // 只记录最外层字典的 Length；复合值在打开时就算作一个值。
        if depth == 1 && stack[0] == b'<' && !matches!(t, Token::Close(_)) {
            if expect_key {
                length_key = matches!(&t, Token::Name(n) if n.as_ref() == b"Length");
                expect_key = false;
            } else {
                if length_key {
                    length = if let Token::Integer(n) = t { Some(n) } else { None };
                }
                expect_key = true;
            }
        }
        match t {
            Token::Open(kind) => {
                if depth == MAX_DEPTH {
                    return Err("PDF 词法嵌套过深（最多 128 层）".into());
                }
                if depth == 0 {
                    length = None;
                    expect_key = true;
                }
                stack[depth] = kind;
                depth += 1;
            }
            Token::Close(kind) => {
                if depth == 0 || stack[depth - 1] != kind {
                    return Err(INVALID_SYNTAX.into());
                }
                depth -= 1;
                closed_dict = depth == 0 && kind == b'<';
            }
            _ => {}
        }
    }
    if depth != 0 {
        return Err(INVALID_SYNTAX.into());
    }
    Ok(())
}

fn image_side(dict: &Dict<'_>, key: &[u8], short: &[u8]) -> Result<u32, String> {
    let read = |key| -> Result<u32, String> {
        let n = dict.get::<f64>(key).ok_or("PDF 图像尺寸缺失或无效")?;
        if !n.is_finite() || n.fract() != 0. || n <= 0. || n > f64::from(MAX_SIDE) {
            return Err("PDF 图像尺寸过大或无效（单边最多 16384 像素）".into());
        }
        Ok(n as u32)
    };
    // hayro 在 XObject 中也接受 W/H 简写，并优先于完整键；两者都存在时都校验。
    let long = dict.contains_key(key).then(|| read(key)).transpose()?;
    let short = dict.contains_key(short).then(|| read(short)).transpose()?;
    short.or(long).ok_or_else(|| "PDF 图像尺寸缺失或无效".into())
}

pub(super) fn images(pdf: &Pdf) -> Result<(), String> {
    let mut total = 0_u64;
    for object in pdf.objects() {
        let Some(stream) = object.into_stream() else { continue };
        let dict = stream.dict();
        if !dict.get::<Name<'_>>(b"Subtype").is_some_and(|n| n.as_ref() == b"Image") {
            continue;
        }
        let width = image_side(dict, b"Width", b"W")?;
        let height = image_side(dict, b"Height", b"H")?;
        let pixels = u64::from(width) * u64::from(height);
        if pixels > MAX_PIXELS {
            return Err("PDF 图像尺寸过大（单图最多 16 Mi 像素）".into());
        }
        total = total.checked_add(pixels).ok_or("PDF 图像总像素超出范围")?;
        if total > MAX_PIXELS {
            return Err("PDF 图像声明总像素过大（整份文档最多 16 Mi 像素）".into());
        }
        let mask = dict.get::<bool>(b"IM").or_else(|| dict.get::<bool>(b"ImageMask")).unwrap_or(false);
        for key in [b"BitsPerComponent".as_slice(), b"BPC".as_slice()] {
            if dict.contains_key(key) {
                let bpc = dict.get::<f64>(key).ok_or("PDF 图像位深无效")?;
                if !matches!(bpc, 1. | 2. | 4. | 8. | 16.) || (mask && bpc != 1.) {
                    return Err("PDF 图像位深无效（仅支持 1、2、4、8、16 位，蒙版为 1 位）".into());
                }
            }
        }
        // 缺省位深沿用 hayro：stencil mask 为 1，其余为 8；JPX 也允许省略此键。
        // 不调用 decoded / decoded_image，既包含引用与 mask，也包含未使用的 Image。
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{MAX_DEPTH, MAX_SOURCE_BYTES, source, source_size};

    #[test]
    fn pdf_source_size_limit_without_large_allocation() {
        assert!(source_size(MAX_SOURCE_BYTES).is_ok());
        assert!(source_size(MAX_SOURCE_BYTES + 1).unwrap_err().contains("源文件过大"));
        assert!(source_size(usize::MAX).is_err());
    }

    #[test]
    fn pdf_source_depth_boundary_counts_dicts_and_arrays() {
        for (open, close) in [("[", "]"), ("<< /A ", ">>")] {
            let bounded = format!("{}0{}", open.repeat(MAX_DEPTH), close.repeat(MAX_DEPTH));
            assert!(source(bounded.as_bytes()).is_ok());
            let deep = format!("{open}{bounded}{close}");
            assert!(source(deep.as_bytes()).unwrap_err().contains("嵌套"));
        }
        let mixed = format!("{}0{}", "[<< /A ".repeat(65), ">>]".repeat(65));
        assert!(source(mixed.as_bytes()).unwrap_err().contains("嵌套"));
        let literal = format!("({}{} )", "(".repeat(160), ")".repeat(160));
        assert!(source(literal.as_bytes()).is_ok(), "literal 自身嵌套不是结构嵌套");
    }

    #[test]
    fn pdf_source_honors_escaped_length_and_stream_boundaries() {
        let payload = format!("endstream\nendobj\n{}(\0", "[".repeat(160));
        for eol in ["\n", "\r", "\r\n"] {
            let stream = format!(
                "<< /A /Length /Nested << /Length 999 >> /Len#67th {} >>\nstream{eol}{payload}\nendstream\nendobj\n",
                payload.len()
            );
            assert!(source(stream.as_bytes()).is_ok());
            let after = format!("{stream}{}0{}", "[".repeat(160), "]".repeat(160));
            assert!(source(after.as_bytes()).unwrap_err().contains("嵌套"));
        }
    }

    #[test]
    fn pdf_source_rejects_malformed_framing_without_recovery() {
        for bytes in [
            b"[>>".as_slice(),
            b"<< /A [0]",
            b"(unterminated",
            b"<0123",
            b"<< /Length 99 >> stream\nabc\nendstream\nendobj",
            b"<< /Length 0 >> stream\nendstreamNO\nendobj",
            b"<< /Length 7 0 R >> stream\nendstream\n[[[\nendstream\nendobj",
        ] {
            assert!(source(bytes).is_err(), "{bytes:?}");
        }
        // 名称或裸 stream 关键字不得隐藏后面的 Catalog 数组。
        for prefix in ["/stream", "stream", "<< /Name /stream >> /stream"] {
            let deep = format!("{prefix}\n{}0{}", "[".repeat(160), "]".repeat(160));
            assert!(source(deep.as_bytes()).unwrap_err().contains("嵌套"));
        }
    }
}
