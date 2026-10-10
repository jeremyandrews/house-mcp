//! Escaping text for HTML and SVG, which share the same five characters.

pub(crate) fn escape(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for c in text.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&#39;"),
            c => out.push(c),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::escape;

    #[test]
    fn markup_characters_are_escaped() {
        assert_eq!(
            escape(r#"<img src=x onerror="a">'&"#),
            "&lt;img src=x onerror=&quot;a&quot;&gt;&#39;&amp;"
        );
    }
}
