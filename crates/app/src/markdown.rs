use rxcord_api::id::Id;

#[derive(Debug, Clone, PartialEq)]
pub enum Block {
    Para(Vec<Span>),
    Code { lang: Option<String>, text: String },
}

#[derive(Debug, Clone, PartialEq, Default)]
pub struct Span {
    pub text: String,
    pub bold: bool,
    pub italic: bool,
    pub strike: bool,
    pub code: bool,
    pub link: Option<String>,
    pub mention: Option<Mention>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mention {
    User(Id),
    Channel(Id),
    Role(Id),
    Everyone,
}

/// Fences first, then each line inline. Delimiters never match across lines.
/// Note: `__x__` (Discord underline) is not supported; each `_` is an italic delimiter, so it renders italic.
pub fn parse(src: &str) -> Vec<Block> {
    let mut out = Vec::new();
    let mut rest = src;
    while let Some(open) = rest.find("```") {
        let after = &rest[open + 3..];
        let Some(close) = after.find("```") else { break };
        push_para(&mut out, &rest[..open]);
        let body = &after[..close];
        let (lang, text) = match body.split_once('\n') {
            Some((l, t)) if !l.trim().is_empty() && !l.trim().contains(char::is_whitespace) => {
                (Some(l.trim().to_string()), t)
            }
            Some((l, t)) if l.trim().is_empty() => (None, t),
            _ => (None, body),
        };
        let text = text.strip_suffix('\n').unwrap_or(text);
        out.push(Block::Code { lang, text: text.to_string() });
        rest = &after[close + 3..];
    }
    push_para(&mut out, rest);
    out
}

fn push_para(out: &mut Vec<Block>, text: &str) {
    let text = text.trim_matches('\n');
    if text.trim().is_empty() {
        return;
    }
    let mut spans = Vec::new();
    for (i, line) in text.split('\n').enumerate() {
        if i > 0 {
            push_span(&mut spans, Span { text: "\n".into(), ..Default::default() });
        }
        inline(line, &mut spans);
    }
    out.push(Block::Para(spans));
}

fn push_span(spans: &mut Vec<Span>, s: Span) {
    if s.text.is_empty() {
        return;
    }
    if let Some(l) = spans.last_mut() {
        let same = |a: &Span, b: &Span| {
            (a.bold, a.italic, a.strike, a.code, &a.link) == (b.bold, b.italic, b.strike, b.code, &b.link)
        };
        if same(l, &s) && l.mention.is_none() && s.mention.is_none() {
            l.text.push_str(&s.text);
            return;
        }
    }
    spans.push(s);
}

#[derive(Clone, Copy, PartialEq)]
enum Kind {
    Bold,
    Star,
    Under,
    Strike,
}

enum Tok {
    Text(String),
    /// code / link / mention: already a full span, only emphasis is added later
    Atom(Span),
    Delim { kind: Kind, lit: &'static str, matched: bool },
}

fn inline(line: &str, spans: &mut Vec<Span>) {
    let c: Vec<char> = line.chars().collect();
    let mut toks: Vec<Tok> = Vec::new();
    let mut stack: Vec<usize> = Vec::new(); // indices of open Delims; only the top may close
    let mut buf = String::new();
    let mut i = 0;
    let mut no_more_ticks = false;
    macro_rules! flush {
        () => {
            if !buf.is_empty() {
                toks.push(Tok::Text(std::mem::take(&mut buf)));
            }
        };
    }
    while i < c.len() {
        let ch = c[i];
        let prev = if i > 0 { Some(c[i - 1]) } else { None };
        match ch {
            '`' if !no_more_ticks => {
                let n = c[i..].iter().take_while(|&&x| x == '`').count();
                // look only at the next backtick run; equal length closes
                let mut j = i + n;
                while j < c.len() && c[j] != '`' {
                    j += 1;
                }
                let m = c[j.min(c.len())..].iter().take_while(|&&x| x == '`').count();
                if j >= c.len() {
                    no_more_ticks = true;
                }
                if m == n && j > i + n {
                    flush!();
                    let text: String = c[i + n..j].iter().collect();
                    toks.push(Tok::Atom(Span { text, code: true, ..Default::default() }));
                    i = j + n;
                } else {
                    buf.extend(&c[i..i + n]);
                    i += n;
                }
            }
            '`' => {
                buf.push(ch);
                i += 1;
            }
            '*' | '_' | '~' => {
                let n = c[i..].iter().take_while(|&&x| x == ch).count();
                let next = c.get(i + n).copied();
                let mut left = n;
                let mut prev = prev;
                while left > 0 {
                    let (can_open, can_close) = match ch {
                        '_' => (
                            prev.is_none_or(|p| !p.is_alphanumeric()) && next.is_some_and(|x| !x.is_whitespace()),
                            prev.is_some_and(|p| !p.is_whitespace()) && next.is_none_or(|x| !x.is_alphanumeric()),
                        ),
                        _ => (
                            true,
                            prev.is_some_and(|p| !p.is_whitespace()),
                        ),
                    };
                    let can_open = can_open && (left > 1 || next.is_some()) && next.is_none_or(|x| !x.is_whitespace());
                    let top = stack.last().copied();
                    let top_kind = top.map(|t| match &toks[t] {
                        Tok::Delim { kind, .. } => *kind,
                        _ => unreachable!(),
                    });
                    // content must be non-empty: the opener may not be the last token
                    let nonempty = top.is_some_and(|t| t + 1 < toks.len() || !buf.is_empty());
                    let want = match (ch, top_kind) {
                        ('*', Some(Kind::Star)) | ('_', Some(Kind::Under)) => Some(1),
                        ('*', Some(Kind::Bold)) if left >= 2 => Some(2),
                        ('~', Some(Kind::Strike)) if left >= 2 => Some(2),
                        _ => None,
                    };
                    if let (true, true, Some(take)) = (can_close, nonempty, want) {
                        flush!();
                        let t = stack.pop().unwrap();
                        let Tok::Delim { matched, .. } = &mut toks[t] else { unreachable!() };
                        *matched = true;
                        toks.push(Tok::Delim {
                            kind: top_kind.unwrap(),
                            lit: "",
                            matched: true,
                        });
                        left -= take;
                    } else if can_open && (ch != '~' || left >= 2) {
                        flush!();
                        let (take, kind, lit) = match ch {
                            '*' if left >= 2 => (2, Kind::Bold, "**"),
                            '*' => (1, Kind::Star, "*"),
                            '_' => (1, Kind::Under, "_"),
                            _ => (2, Kind::Strike, "~~"),
                        };
                        stack.push(toks.len());
                        toks.push(Tok::Delim { kind, lit, matched: false });
                        left -= take;
                    } else {
                        buf.push(ch);
                        left -= 1;
                    }
                    prev = Some(ch);
                }
                i += n;
            }
            '<' => {
                if let Some((m, len)) = mention(&c[i..]) {
                    flush!();
                    let text: String = c[i..i + len].iter().collect();
                    toks.push(Tok::Atom(Span { text, mention: Some(m), ..Default::default() }));
                    i += len;
                } else {
                    buf.push(ch);
                    i += 1;
                }
            }
            '@' if prev.is_none_or(|p| !p.is_alphanumeric()) => {
                let len = ["@everyone", "@here"]
                    .iter()
                    .find(|w| {
                        c[i..].iter().take(w.len()).copied().eq(w.chars())
                            && c.get(i + w.len()).is_none_or(|x| !x.is_alphanumeric())
                    })
                    .map(|w| w.len());
                match len {
                    Some(len) => {
                        flush!();
                        let text: String = c[i..i + len].iter().collect();
                        toks.push(Tok::Atom(Span { text, mention: Some(Mention::Everyone), ..Default::default() }));
                        i += len;
                    }
                    None => {
                        buf.push(ch);
                        i += 1;
                    }
                }
            }
            'h' if prev.is_none_or(|p| !p.is_alphanumeric()) && is_scheme(&c[i..]) => {
                let mut end = i;
                while end < c.len() && !c[end].is_whitespace() {
                    end += 1;
                }
                if matches!(c[end - 1], ')' | '.' | ',') {
                    end -= 1;
                }
                flush!();
                let text: String = c[i..end].iter().collect();
                toks.push(Tok::Atom(Span { link: Some(text.clone()), text, ..Default::default() }));
                i = end;
            }
            _ => {
                buf.push(ch);
                i += 1;
            }
        }
    }
    flush!();
    // an unmatched opener was never closed: it is literal text
    let (mut bold, mut italic, mut strike) = (0u32, 0u32, 0u32);
    for t in toks {
        match t {
            Tok::Delim { kind, lit, matched } => {
                if !matched {
                    push_style(spans, lit.to_string(), bold > 0, italic > 0, strike > 0);
                    continue;
                }
                let on = if lit.is_empty() { -1 } else { 1 }; // closers carry no literal
                let cnt = match kind {
                    Kind::Bold => &mut bold,
                    Kind::Strike => &mut strike,
                    _ => &mut italic,
                };
                *cnt = cnt.wrapping_add_signed(on);
            }
            Tok::Text(text) => push_style(spans, text, bold > 0, italic > 0, strike > 0),
            Tok::Atom(mut s) => {
                s.bold = bold > 0;
                s.italic = italic > 0;
                s.strike = strike > 0;
                push_span(spans, s);
            }
        }
    }
}

fn push_style(spans: &mut Vec<Span>, text: String, bold: bool, italic: bool, strike: bool) {
    push_span(spans, Span { text, bold, italic, strike, ..Default::default() });
}

fn is_scheme(c: &[char]) -> bool {
    let s = |p: &str| c.len() > p.len() && c.iter().take(p.len()).copied().eq(p.chars());
    s("http://") || s("https://")
}

/// `c` starts at `<`. Returns the mention and its length in chars.
fn mention(c: &[char]) -> Option<(Mention, usize)> {
    if !matches!(c.get(1), Some('@' | '#')) {
        return None;
    }
    let end = c.iter().take(30).position(|&x| x == '>')?;
    let body: String = c[1..end].iter().collect();
    let (ctor, digits): (fn(Id) -> Mention, &str) = if let Some(d) = body.strip_prefix("@&") {
        (Mention::Role, d)
    } else if let Some(d) = body.strip_prefix("@!") {
        (Mention::User, d)
    } else if let Some(d) = body.strip_prefix('@') {
        (Mention::User, d)
    } else if let Some(d) = body.strip_prefix('#') {
        (Mention::Channel, d)
    } else {
        return None;
    };
    if digits.is_empty() || !digits.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    Some((ctor(Id(digits.parse().ok()?)), end + 1))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn plain(t: &str) -> Span {
        Span { text: t.into(), ..Default::default() }
    }
    fn para(src: &str) -> Vec<Span> {
        match parse(src).into_iter().next() {
            Some(Block::Para(s)) => s,
            b => panic!("not a para: {b:?}"),
        }
    }

    #[test]
    fn plain_text() {
        assert_eq!(parse("hello world"), vec![Block::Para(vec![plain("hello world")])]);
    }

    #[test]
    fn bold_italic_nested() {
        let s = para("***x***");
        assert_eq!(s, vec![Span { text: "x".into(), bold: true, italic: true, ..Default::default() }]);
        let s = para("a *b* _c_ ~~d~~ **e**");
        assert!(s[1].italic && s[3].italic && s[5].strike && s[7].bold);
    }

    #[test]
    fn inline_code_no_format_inside() {
        let s = para("`**a**`");
        assert_eq!(s, vec![Span { text: "**a**".into(), code: true, ..Default::default() }]);
    }

    #[test]
    fn fence_with_lang() {
        assert_eq!(
            parse("hi\n```rust\nfn main(){}\n```"),
            vec![
                Block::Para(vec![plain("hi")]),
                Block::Code { lang: Some("rust".into()), text: "fn main(){}".into() }
            ]
        );
        assert_eq!(
            parse("```\nx\n```"),
            vec![Block::Code { lang: None, text: "x".into() }]
        );
    }

    #[test]
    fn unterminated_fence_is_text() {
        let b = parse("```rust\nfn");
        assert_eq!(b.len(), 1);
        let Block::Para(s) = &b[0] else { panic!() };
        let all: String = s.iter().map(|s| s.text.as_str()).collect();
        assert_eq!(all, "```rust\nfn");
        assert!(s.iter().all(|s| !s.code));
    }

    #[test]
    fn lone_stars_are_text() {
        assert_eq!(para("a ** b"), vec![plain("a ** b")]);
        assert_eq!(para("**x"), vec![plain("**x")]);
        assert_eq!(para("* a *"), vec![plain("* a *")]);
        assert_eq!(para("snake_case_name"), vec![plain("snake_case_name")]);
    }

    #[test]
    fn mentions_parse() {
        let s = para("<@1> <@!2> <#3> <@&4> @everyone @here <@x>");
        let m: Vec<_> = s.iter().filter_map(|s| s.mention).collect();
        assert_eq!(
            m,
            vec![
                Mention::User(Id(1)),
                Mention::User(Id(2)),
                Mention::Channel(Id(3)),
                Mention::Role(Id(4)),
                Mention::Everyone,
                Mention::Everyone
            ]
        );
        assert_eq!(s.last().unwrap().text, " <@x>");
    }

    #[test]
    fn link_parse() {
        let s = para("see https://a.b/c?d=1) ok");
        assert_eq!(s[0], plain("see "));
        assert_eq!(
            s[1],
            Span { text: "https://a.b/c?d=1".into(), link: Some("https://a.b/c?d=1".into()), ..Default::default() }
        );
        assert_eq!(s[2], plain(") ok"));
    }

    #[test]
    fn everyone_needs_word_boundary() {
        assert_eq!(para("@everyoneelse"), vec![plain("@everyoneelse")]);
    }

    #[test]
    fn emoji_next_to_star() {
        let s = para("*😀*");
        assert_eq!(s, vec![Span { text: "😀".into(), italic: true, ..Default::default() }]);
        assert_eq!(para("😀**"), vec![plain("😀**")]);
    }

    #[test]
    fn double_underscore_is_italic() {
        let s = para("__x__");
        assert_eq!(s, vec![Span { text: "x".into(), italic: true, ..Default::default() }]);
    }

    #[test]
    fn adversarial_inputs_linear_and_plain() {
        let mixed: String = "abcXYZ019".chars().cycle().take(4000).collect();
        let cases = [
            mixed,
            format!("```{}", "x".repeat(100_000)),
            "*_".repeat(50_000),
            "`a``".repeat(25_000),
            "<@".repeat(50_000),
        ];
        for src in cases {
            let t = std::time::Instant::now();
            let b = parse(&src);
            assert!(t.elapsed().as_millis() < 50, "took {:?}", t.elapsed());
            if src.contains("```") && !src.starts_with("```") {
                continue; // repeated "```" legitimately forms fences; only timing matters
            }
            assert_eq!(b.len(), 1);
            let Block::Para(s) = &b[0] else { panic!("not a para") };
            assert!(s.iter().all(|s| !s.bold && !s.italic && !s.code && s.mention.is_none()));
            let all: String = s.iter().map(|s| s.text.as_str()).collect();
            assert_eq!(all, src);
        }
    }

    #[test]
    fn long_no_space_linear() {
        for c in ["*", "_", "~", "`", "<"] {
            let src = c.repeat(200_000);
            let t = std::time::Instant::now();
            let _ = parse(&src);
            assert!(t.elapsed().as_millis() < 50, "{c} took {:?}", t.elapsed());
        }
    }
}
