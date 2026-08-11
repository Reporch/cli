use pulldown_cmark::{CodeBlockKind, Event, Options, Parser, Tag, TagEnd};

pub fn markdown_to_tex(markdown: &str) -> String {
    let parser = Parser::new_ext(markdown, Options::ENABLE_MATH | Options::ENABLE_TABLES);
    let mut output = String::new();
    let mut verbatim = false;
    for event in parser {
        match event {
            Event::Start(Tag::Heading { .. }) => output.push_str("\\section*{"),
            Event::End(TagEnd::Heading(_)) => output.push_str("}\n"),
            Event::Start(Tag::Paragraph) => {}
            Event::End(TagEnd::Paragraph) => output.push_str("\n\n"),
            Event::Start(Tag::Emphasis) => output.push_str("\\emph{"),
            Event::End(TagEnd::Emphasis) => output.push('}'),
            Event::Start(Tag::Strong) => output.push_str("\\textbf{"),
            Event::End(TagEnd::Strong) => output.push('}'),
            Event::Start(Tag::CodeBlock(CodeBlockKind::Indented | CodeBlockKind::Fenced(_))) => {
                verbatim = true;
                output.push_str("\\begin{verbatim}\n");
            }
            Event::End(TagEnd::CodeBlock) => {
                verbatim = false;
                output.push_str("\\end{verbatim}\n");
            }
            Event::Start(Tag::List(Some(_))) => output.push_str("\\begin{enumerate}\n"),
            Event::End(TagEnd::List(true)) => output.push_str("\\end{enumerate}\n"),
            Event::Start(Tag::List(None)) => output.push_str("\\begin{itemize}\n"),
            Event::End(TagEnd::List(false)) => output.push_str("\\end{itemize}\n"),
            Event::Start(Tag::Item) => output.push_str("\\item "),
            Event::End(TagEnd::Item) => output.push('\n'),
            Event::Start(Tag::Table(_)) => output.push_str("\\begin{tabular}{l}\n"),
            Event::End(TagEnd::Table) => output.push_str("\\end{tabular}\n"),
            Event::End(TagEnd::TableHead | TagEnd::TableRow) => output.push_str(" \\\\\n"),
            Event::End(TagEnd::TableCell) => output.push_str(" & "),
            Event::Text(text) if verbatim => output.push_str(&text),
            Event::Text(text) => output.push_str(&escape_latex(&text)),
            Event::Code(code) => {
                output.push_str("\\texttt{");
                output.push_str(&escape_latex(&code));
                output.push('}');
            }
            Event::InlineMath(math) => {
                output.push('$');
                output.push_str(&math);
                output.push('$');
            }
            Event::DisplayMath(math) => {
                output.push_str("\\[");
                output.push_str(&math);
                output.push_str("\\]\n");
            }
            Event::SoftBreak => output.push('\n'),
            Event::HardBreak => output.push_str("\\\\\n"),
            Event::Rule => output.push_str("\\hrule\n"),
            Event::Html(_) | Event::InlineHtml(_) => {}
            _ => {}
        }
    }
    output
}

pub fn escape_latex(value: &str) -> String {
    let mut escaped = String::with_capacity(value.len());
    for character in value.chars() {
        match character {
            '\\' => escaped.push_str("\\textbackslash{}"),
            '{' => escaped.push_str("\\{"),
            '}' => escaped.push_str("\\}"),
            '#' => escaped.push_str("\\#"),
            '$' => escaped.push_str("\\$"),
            '%' => escaped.push_str("\\%"),
            '&' => escaped.push_str("\\&"),
            '_' => escaped.push_str("\\_"),
            '^' => escaped.push_str("\\textasciicircum{}"),
            '~' => escaped.push_str("\\textasciitilde{}"),
            other => escaped.push(other),
        }
    }
    escaped
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn transforms_markdown_deterministically_and_escapes_latex() {
        let markdown = "# A & B\n\nUse `x_y` and $n+1$.\n";
        let expected = "\\section*{A \\& B}\nUse \\texttt{x\\_y} and $n+1$.\n\n";
        assert_eq!(markdown_to_tex(markdown), expected);
        assert_eq!(markdown_to_tex(markdown), expected);
    }
}
