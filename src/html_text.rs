//! Best-effort HTML to readable text conversion for format="text".
//!
//! A single forward scan over the document: the input is a web page the server
//! doesn't control, so nothing here may rescan the document per tag.

use url::Url;

const LATIN1_ENTITIES: [&str; 96] = [
    "nbsp", "iexcl", "cent", "pound", "curren", "yen", "brvbar", "sect", "uml", "copy", "ordf", "laquo", "not", "shy",
    "reg", "macr", "deg", "plusmn", "sup2", "sup3", "acute", "micro", "para", "middot", "cedil", "sup1", "ordm",
    "raquo", "frac14", "frac12", "frac34", "iquest", "Agrave", "Aacute", "Acirc", "Atilde", "Auml", "Aring", "AElig",
    "Ccedil", "Egrave", "Eacute", "Ecirc", "Euml", "Igrave", "Iacute", "Icirc", "Iuml", "ETH", "Ntilde", "Ograve",
    "Oacute", "Ocirc", "Otilde", "Ouml", "times", "Oslash", "Ugrave", "Uacute", "Ucirc", "Uuml", "Yacute", "THORN",
    "szlig", "agrave", "aacute", "acirc", "atilde", "auml", "aring", "aelig", "ccedil", "egrave", "eacute", "ecirc",
    "euml", "igrave", "iacute", "icirc", "iuml", "eth", "ntilde", "ograve", "oacute", "ocirc", "otilde", "ouml",
    "divide", "oslash", "ugrave", "uacute", "ucirc", "uuml", "yacute", "thorn", "yuml",
];

fn named_entity(name: &str) -> Option<char> {
    if let Some(i) = LATIN1_ENTITIES.iter().position(|e| *e == name) {
        return char::from_u32(160 + i as u32);
    }
    Some(match name {
        "amp" => '&',
        "lt" => '<',
        "gt" => '>',
        "quot" => '"',
        "apos" => '\'',
        "OElig" => 'Œ',
        "oelig" => 'œ',
        "Scaron" => 'Š',
        "scaron" => 'š',
        "Yuml" => 'Ÿ',
        "fnof" => 'ƒ',
        "circ" => 'ˆ',
        "tilde" => '˜',
        "ensp" => '\u{2002}',
        "emsp" => '\u{2003}',
        "thinsp" => '\u{2009}',
        "zwnj" => '\u{200c}',
        "zwj" => '\u{200d}',
        "lrm" => '\u{200e}',
        "rlm" => '\u{200f}',
        "ndash" => '–',
        "mdash" => '—',
        "lsquo" => '‘',
        "rsquo" => '’',
        "sbquo" => '‚',
        "ldquo" => '“',
        "rdquo" => '”',
        "bdquo" => '„',
        "dagger" => '†',
        "Dagger" => '‡',
        "bull" => '•',
        "hellip" => '…',
        "permil" => '‰',
        "prime" => '′',
        "Prime" => '″',
        "lsaquo" => '‹',
        "rsaquo" => '›',
        "euro" => '€',
        "trade" => '™',
        "minus" => '−',
        "larr" => '←',
        "uarr" => '↑',
        "rarr" => '→',
        "darr" => '↓',
        "harr" => '↔',
        _ => return None,
    })
}

/// Parses an entity body (between "&" and ";"), returning the character.
fn entity_char(body: &str) -> Option<char> {
    if let Some(num) = body.strip_prefix('#') {
        let code = match num.strip_prefix(['x', 'X']) {
            Some(hex) if (1..=6).contains(&hex.len()) => u32::from_str_radix(hex, 16).ok()?,
            None if (1..=7).contains(&num.len()) && num.bytes().all(|b| b.is_ascii_digit()) => num.parse().ok()?,
            _ => return None,
        };
        return Some(if code == 0 { '\u{fffd}' } else { char::from_u32(code).unwrap_or('\u{fffd}') });
    }
    let mut bytes = body.bytes();
    let valid = bytes.next().is_some_and(|b| b.is_ascii_alphabetic()) && bytes.all(|b| b.is_ascii_alphanumeric());
    if valid { named_entity(body) } else { None }
}

pub fn decode_entities(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(amp) = rest.find('&') {
        out.push_str(&rest[..amp]);
        rest = &rest[amp..];
        // Entity bodies are at most 32 bytes; only look that far for the ";".
        let window = &rest.as_bytes()[1..rest.len().min(34)];
        let decoded = window
            .iter()
            .position(|&b| b == b';')
            .and_then(|semi| entity_char(&rest[1..1 + semi]).map(|ch| (ch, semi + 2)));
        match decoded {
            Some((ch, len)) => {
                out.push(ch);
                rest = &rest[len..];
            }
            None => {
                out.push('&');
                rest = &rest[1..];
            }
        }
    }
    out.push_str(rest);
    out
}

const SKIPPED: [&str; 11] =
    ["script", "style", "noscript", "template", "svg", "math", "head", "iframe", "object", "canvas", "select"];

const BLOCKS: [&str; 29] = [
    "p",
    "div",
    "section",
    "article",
    "header",
    "footer",
    "nav",
    "main",
    "aside",
    "ul",
    "ol",
    "dl",
    "dt",
    "dd",
    "table",
    "thead",
    "tbody",
    "tfoot",
    "tr",
    "caption",
    "blockquote",
    "pre",
    "form",
    "fieldset",
    "legend",
    "figure",
    "figcaption",
    "address",
    "details",
];

fn find_href(attrs: &str, base: &Url) -> Option<String> {
    let lower = attrs.to_ascii_lowercase();
    let mut from = 0;
    while let Some(i) = lower[from..].find("href") {
        let start = from + i;
        from = start + 4;
        if start > 0 && !lower.as_bytes()[start - 1].is_ascii_whitespace() {
            continue;
        }
        let after = attrs[from..].trim_start();
        let Some(value) = after.strip_prefix('=') else { continue };
        let value = value.trim_start();
        let raw = match value.chars().next() {
            Some(q @ ('"' | '\'')) => value[1..].split(q).next().unwrap_or(""),
            _ => value.split(|c: char| c.is_ascii_whitespace() || c == '>').next().unwrap_or(""),
        };
        let raw = decode_entities(raw);
        let raw = raw.trim();
        if raw.is_empty() || raw.starts_with('#') {
            return None;
        }
        let url = base.join(raw).ok()?;
        return matches!(url.scheme(), "http" | "https" | "mailto").then(|| url.to_string());
    }
    None
}

fn title(html: &str, lower: &str) -> Option<String> {
    let start = lower.find("<title")?;
    let open_end = start + lower[start..].find('>')? + 1;
    let len = lower[open_end..].find('<').unwrap_or(lower.len() - open_end);
    let text = decode_entities(&html[open_end..open_end + len]);
    let text = text.split_whitespace().collect::<Vec<_>>().join(" ");
    (!text.is_empty()).then_some(text)
}

/// Collapses horizontal whitespace, trims lines and allows at most one blank line in a row.
fn tidy(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut blank_lines = 0;
    for line in text.split('\n') {
        let words: Vec<&str> = line.split(|c: char| c.is_whitespace()).filter(|w| !w.is_empty()).collect();
        if words.is_empty() {
            blank_lines += 1;
            continue;
        }
        if !out.is_empty() {
            out.push_str(if blank_lines > 0 { "\n\n" } else { "\n" });
        }
        blank_lines = 0;
        out.push_str(&words.join(" "));
    }
    out
}

pub fn html_to_text(html: &str, base: &Url) -> String {
    // ASCII lowercasing keeps byte offsets identical, so indices into `lower` work on `html`.
    let lower = html.to_ascii_lowercase();
    let bytes = html.as_bytes();
    let mut out = String::with_capacity(html.len() / 2);
    let mut link: Option<(String, usize)> = None; // (href, position in `out` after "[")
    let mut text_start = 0;
    let mut pos = 0;

    while let Some(rel) = lower[pos..].find('<') {
        let lt = pos + rel;
        let next = bytes.get(lt + 1).copied().unwrap_or(0);
        let closing = next == b'/';
        let is_markup = next.is_ascii_alphabetic()
            || next == b'!'
            || next == b'?'
            || (closing && bytes.get(lt + 2).is_some_and(u8::is_ascii_alphabetic));
        if !is_markup {
            pos = lt + 1;
            continue;
        }

        if lower[lt..].starts_with("<!--") {
            out.push_str(&decode_entities(&html[text_start..lt]));
            pos = lower[lt + 4..].find("-->").map_or(html.len(), |i| lt + 4 + i + 3);
            text_start = pos;
            continue;
        }

        // The tag ends at the next ">"; a "<" before it means this wasn't a tag.
        let Some(len) = bytes[lt + 1..].iter().position(|&b| b == b'>' || b == b'<') else {
            // A tag cut off by the end of the document is dropped.
            out.push_str(&decode_entities(&html[text_start..lt]));
            text_start = html.len();
            break;
        };
        let gt = lt + 1 + len;
        if bytes[gt] == b'<' {
            pos = gt;
            continue;
        }

        out.push_str(&decode_entities(&html[text_start..lt]));
        pos = gt + 1;
        text_start = pos;

        let name_start = if closing { lt + 2 } else { lt + 1 };
        let name_len = bytes[name_start..gt].iter().take_while(|b| b.is_ascii_alphanumeric()).count();
        let name = &lower[name_start..name_start + name_len];
        let attrs = &html[name_start + name_len..gt];

        if !closing && SKIPPED.contains(&name) {
            let needle = format!("</{name}");
            pos = match lower[pos..].find(&needle) {
                Some(i) => {
                    let close = pos + i + needle.len();
                    lower[close..].find('>').map_or(html.len(), |j| close + j + 1)
                }
                None => html.len(),
            };
            text_start = pos;
            continue;
        }

        match (name, closing) {
            ("a", false) => {
                link = find_href(attrs, base).map(|href| {
                    out.push('[');
                    (href, out.len())
                });
            }
            ("a", true) => {
                if let Some((href, start)) = link.take() {
                    if out[start..].trim().is_empty() {
                        out.truncate(start - 1); // drop links without text
                    } else {
                        out.push_str(&format!("]({href})"));
                    }
                }
            }
            ("h1" | "h2" | "h3" | "h4" | "h5" | "h6", false) => {
                out.push_str("\n\n");
                out.push_str(&"#".repeat(usize::from(name.as_bytes()[1] - b'0')));
                out.push(' ');
            }
            ("h1" | "h2" | "h3" | "h4" | "h5" | "h6", true) => out.push_str("\n\n"),
            ("li", false) => out.push_str("\n- "),
            ("br" | "hr", _) => out.push('\n'),
            ("td" | "th", false) => out.push_str(" | "),
            (name, _) if BLOCKS.contains(&name) => out.push('\n'),
            _ => {}
        }
    }
    out.push_str(&decode_entities(&html[text_start..]));

    let body = tidy(&out);
    match title(html, &lower) {
        Some(title) => format!("Title: {title}\n\n{body}"),
        None => body,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn base() -> Url {
        Url::parse("https://example.com/base/page").unwrap()
    }

    #[test]
    fn decodes_entities() {
        assert_eq!(decode_entities("&auml;&ouml;&aring; &Ouml; &yuml; &nbsp;&amp;&lt;&gt;"), "äöå Ö ÿ \u{a0}&<>");
        assert_eq!(decode_entities("&#228;&#xE4;&#X1F600;&#0;&#xD800;"), "ää😀\u{fffd}\u{fffd}");
        assert_eq!(decode_entities("&unknown; &AMP; & alone &;"), "&unknown; &AMP; & alone &;");
        assert_eq!(decode_entities("Ä&amp"), "Ä&amp");
    }

    #[test]
    fn converts_html_to_text() {
        let html = r##"<!doctype html><html><head><title>My &amp; Page</title><style>p{}</style></head>
            <body><nav><a href="/home">Home</a></nav>
            <script>alert("<b>x</b>")</script><!-- hidden -->
            <h1>Hello</h1><p>First   para with <b>bold</b> and a <a href="docs/x?a=1&amp;b=2">link</a>.</p>
            <ul><li>one</li><li>two</li></ul>
            <a href="#top">Top</a> <a href="javascript:void(0)">js</a> <a href="/img"><img src=x></a>
            <p>1 < 2 and 3 > 2</p>
            <table><tr><td>a</td><td>b</td></tr></table></body></html>"##;
        let expected = [
            "Title: My & Page",
            "",
            "[Home](https://example.com/home)",
            "",
            "# Hello",
            "",
            "First para with bold and a [link](https://example.com/base/docs/x?a=1&b=2).",
            "",
            "- one",
            "- two",
            "",
            "Top js",
            "",
            "1 < 2 and 3 > 2",
            "",
            "| a | b",
        ]
        .join("\n");
        assert_eq!(html_to_text(html, &base()), expected);
    }

    #[test]
    fn unclosed_elements_and_comments() {
        assert_eq!(html_to_text("before<script>never closed", &base()), "before");
        assert_eq!(html_to_text("before<!-- never closed", &base()), "before");
        assert_eq!(html_to_text("a <b unterminated", &base()), "a");
        assert_eq!(html_to_text("Ünïcödé <p>tëxt</p>", &base()), "Ünïcödé\ntëxt");
    }

    #[test]
    fn hostile_input_is_linear() {
        let hostile =
            "<script".repeat(200_000) + &"<".repeat(200_000) + &"<!--".repeat(100_000) + &"<a href=x>".repeat(100_000);
        let started = std::time::Instant::now();
        html_to_text(&hostile, &base());
        assert!(started.elapsed().as_secs() < 2, "conversion should be linear");
    }
}
