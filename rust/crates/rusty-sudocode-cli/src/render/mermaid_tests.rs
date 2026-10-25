use super::super::{ColorSupport, MarkdownStreamState, TerminalRenderer};
use unicode_width::UnicodeWidthStr;

fn render(source: &str, width: usize) -> String {
    TerminalRenderer::new()
        .with_color_support(ColorSupport::NoColor)
        .render_markdown_with_width(source, width)
}

#[test]
fn completed_fences_display_all_advertised_families_and_unicode() {
    for (source, labels) in [
        ("flowchart TD\nA[请求] --> B[Reply]", ["请求", "Reply"]),
        ("sequenceDiagram\nA->>B: request", ["A", "request"]),
        ("stateDiagram-v2\n[*] --> Ready", ["Ready", "initial"]),
        (
            "classDiagram\nclass Order\nOrder --> Item",
            ["Order", "Item"],
        ),
        ("erDiagram\nORDER ||--o{ ITEM : contains", ["ORDER", "ITEM"]),
    ] {
        let output = render(&format!("```mermaid title=example\n{source}\n```\n"), 100);
        for label in labels {
            assert!(output.contains(label), "{output}");
        }
        assert!(output.contains('─'), "{output}");
        assert!(!output.contains("source shown"), "{output}");
        assert!(!output.contains('\x1b'), "NoColor leaked ANSI: {output:?}");
        assert!(output.lines().all(|line| line.width() <= 100), "{output}");
    }
}

#[test]
fn unfinished_unsupported_and_narrow_fences_keep_readable_source() {
    let source = "flowchart LR\nA[请求] --> B[Reply]\n";
    let unfinished = render(&format!("```mermaid\n{source}"), 100);
    assert!(unfinished.contains(source));
    assert!(!unfinished.contains("source shown"));
    for (source, width) in [("pie\n\"Cats\": 2\n", 80), (source, 8)] {
        let output = render(&format!("```mermaid\n{source}```\n"), width);
        assert!(output.contains("source shown"), "{output}");
        assert!(output.contains(source), "{output}");
    }
    let unsafe_source = render("```mermaid\nflowchart TD\nA[\x1b[2J]\n```", 80);
    assert!(!unsafe_source.contains('\x1b'));
    assert!(unsafe_source.contains("\\u{1b}[2J"), "{unsafe_source}");
}

#[test]
fn nested_fences_reserve_prefix_width_and_require_the_real_closer() {
    let output = render(
        "> ~~~~mermaid\n> flowchart TD\n> A[请求] --> B[Reply]\n> ~~~~~\n",
        40,
    );
    assert!(output.contains("请求"), "{output}");
    assert!(!output.contains("flowchart"), "{output}");
    assert!(output.lines().all(|line| line.width() <= 40), "{output}");
    assert!(output
        .lines()
        .filter(|line| !line.is_empty())
        .all(|line| line.starts_with("> ")));
    let incomplete = render("````mermaid\nflowchart TD\nA --> B\n```\n", 80);
    assert!(incomplete.contains("flowchart TD"));
    assert!(!incomplete.contains("source shown"));
}

#[test]
fn streaming_waits_for_fences_and_retains_source_across_every_split() {
    let source = "```mermaid\nflowchart TD\nA[请求] --> B[Reply]\n```\n\n";
    let renderer = TerminalRenderer::new().with_color_support(ColorSupport::NoColor);
    let expected = renderer.render_markdown(source);
    for split in source.char_indices().map(|(offset, _)| offset) {
        let mut stream = MarkdownStreamState::default();
        let first = stream.push(&renderer, &source[..split]);
        if !source[..split].contains("\n```\n") {
            assert!(first.is_none(), "premature diagram at byte {split}");
            assert_eq!(stream.pending_source(), &source[..split]);
        }
        let mut output = first.unwrap_or_default();
        output.push_str(&stream.push(&renderer, &source[split..]).unwrap_or_default());
        output.push_str(&stream.flush(&renderer).unwrap_or_default());
        assert_eq!(output.trim(), expected.trim(), "split at {split}");
    }
}

#[test]
fn streaming_keeps_nested_mermaid_fences_with_blank_lines_together() {
    let renderer = TerminalRenderer::new().with_color_support(ColorSupport::NoColor);
    let prefix = "- Diagram:\n\n  ```mermaid\n  flowchart TD\n\n  A[请求] --> B[Reply]\n";
    let tail = "  ```\n\n";
    let mut stream = MarkdownStreamState::default();
    assert!(stream.push(&renderer, prefix).is_none());
    assert_eq!(stream.pending_source(), prefix);
    let mut output = stream.push(&renderer, tail).unwrap_or_default();
    output.push_str(&stream.flush(&renderer).unwrap_or_default());
    assert!(
        output.contains("请求") && !output.contains("flowchart"),
        "{output}"
    );
    assert_eq!(output.trim(), render(&format!("{prefix}{tail}"), 78).trim());
}
