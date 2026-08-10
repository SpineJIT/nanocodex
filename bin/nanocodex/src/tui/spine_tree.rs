use nanocodex_spine_runtime::{
    SpineTreeNode, SpineTreeNodeKind, SpineTreeNodeStatus, SpineTreeSnapshot,
};
use ratatui::{
    style::{Color, Modifier, Style},
    text::{Line, Span, Text},
};
const MAX_SUMMARY_CHARS: usize = 120;

pub(super) fn text(snapshot: &SpineTreeSnapshot) -> Text<'static> {
    let mut lines = vec![Line::from(vec![
        Span::styled("• ", Style::default().fg(Color::DarkGray)),
        Span::styled(
            "Spine Tree",
            Style::default()
                .fg(Color::Magenta)
                .add_modifier(Modifier::BOLD),
        ),
    ])];
    let mut roots = Vec::new();
    for node in children(snapshot, None) {
        if node.kind == SpineTreeNodeKind::RootEpoch {
            roots.extend(children(snapshot, Some(node.id.as_str())));
        } else {
            roots.push(node);
        }
    }
    if roots.is_empty() {
        lines.push(Line::styled(
            "  └ (empty)",
            Style::default().fg(Color::DarkGray),
        ));
    } else {
        render_nodes(snapshot, &roots, "  ", &mut lines);
    }
    Text::from(lines)
}
fn render_nodes(
    snapshot: &SpineTreeSnapshot,
    nodes: &[&SpineTreeNode],
    prefix: &str,
    lines: &mut Vec<Line<'static>>,
) {
    for (index, node) in nodes.iter().enumerate() {
        let last = index + 1 == nodes.len();
        let branch = if last { "└ " } else { "├ " };
        let child_prefix = format!("{prefix}{}", if last { "  " } else { "│ " });
        let active = node.id == snapshot.active_node_id;
        lines.push(node_line(node, active, format!("{prefix}{branch}")));
        render_nodes(
            snapshot,
            &children(snapshot, Some(node.id.as_str())),
            &child_prefix,
            lines,
        );
    }
}
fn children<'a>(
    snapshot: &'a SpineTreeSnapshot,
    parent_id: Option<&str>,
) -> Vec<&'a SpineTreeNode> {
    snapshot
        .nodes
        .iter()
        .filter(|node| node.parent_id.as_deref() == parent_id)
        .collect()
}

fn node_line(node: &SpineTreeNode, active: bool, prefix: String) -> Line<'static> {
    let marker = marker(node.status, active);
    let style = marker_style(node.status, active);
    Line::from(vec![
        Span::styled(prefix, Style::default().fg(Color::DarkGray)),
        Span::styled(marker, style),
        Span::raw(" "),
        Span::styled(node_label(node, active), style),
    ])
}

const fn marker(status: SpineTreeNodeStatus, active: bool) -> &'static str {
    if active {
        return "◉";
    }
    match status {
        SpineTreeNodeStatus::Live => "◉",
        SpineTreeNodeStatus::Opened => "▾",
        SpineTreeNodeStatus::Closed => "✓",
        SpineTreeNodeStatus::Compacted => "◌",
    }
}

fn marker_style(status: SpineTreeNodeStatus, active: bool) -> Style {
    if active || status == SpineTreeNodeStatus::Live {
        return Style::default()
            .fg(Color::Cyan)
            .add_modifier(Modifier::BOLD);
    }
    match status {
        SpineTreeNodeStatus::Closed => Style::default()
            .fg(Color::Green)
            .add_modifier(Modifier::BOLD),
        SpineTreeNodeStatus::Opened | SpineTreeNodeStatus::Compacted => {
            Style::default().fg(Color::DarkGray)
        }
        SpineTreeNodeStatus::Live => Style::default()
            .fg(Color::Cyan)
            .add_modifier(Modifier::BOLD),
    }
}

fn node_label(node: &SpineTreeNode, active: bool) -> String {
    let summary = node
        .summary
        .as_deref()
        .map(str::trim)
        .filter(|summary| !summary.is_empty())
        .unwrap_or_else(|| default_node_label(node.status, active));
    truncate_summary(summary)
}

fn default_node_label(status: SpineTreeNodeStatus, active: bool) -> &'static str {
    if active || status == SpineTreeNodeStatus::Live {
        return "Current task";
    }
    match status {
        SpineTreeNodeStatus::Live => "Current task",
        SpineTreeNodeStatus::Opened => "Task",
        SpineTreeNodeStatus::Closed => "Completed task",
        SpineTreeNodeStatus::Compacted => "Previous task",
    }
}

fn truncate_summary(summary: &str) -> String {
    let mut characters = summary.chars();
    let prefix = characters
        .by_ref()
        .take(MAX_SUMMARY_CHARS)
        .collect::<String>();
    if characters.next().is_some() {
        format!("{prefix}…")
    } else {
        prefix
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use nanocodex_spine_runtime::{SpineTreeNodeKind, SpineTreeNodeStatus};

    #[test]
    fn tree_text_marks_the_active_and_closed_scopes() {
        let snapshot = SpineTreeSnapshot {
            active_node_id: "1.2".to_owned(),
            nodes: vec![
                SpineTreeNode {
                    id: "1".to_owned(),
                    parent_id: None,
                    kind: SpineTreeNodeKind::RootEpoch,
                    status: SpineTreeNodeStatus::Opened,
                    summary: None,
                },
                node(
                    "1.1",
                    Some("1"),
                    Some("inspect the parser"),
                    SpineTreeNodeStatus::Closed,
                ),
                node(
                    "1.2",
                    Some("1"),
                    Some("implement the fix"),
                    SpineTreeNodeStatus::Live,
                ),
            ],
        };

        let rendered = text(&snapshot)
            .lines
            .iter()
            .map(|line| {
                line.spans
                    .iter()
                    .map(|span| span.content.as_ref())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n");

        assert!(rendered.contains("Spine Tree"));
        assert!(rendered.contains("✓ inspect the parser"));
        assert!(rendered.contains("◉ implement the fix"));
        assert!(!rendered.contains("Root session"));
    }

    #[test]
    fn tree_text_uses_spine_defaults_for_missing_summaries() {
        let snapshot = SpineTreeSnapshot {
            active_node_id: "1.2".to_owned(),
            nodes: vec![
                node("1.1", None, None, SpineTreeNodeStatus::Closed),
                node("1.2", None, None, SpineTreeNodeStatus::Live),
            ],
        };

        let rendered = text(&snapshot)
            .lines
            .iter()
            .map(|line| {
                line.spans
                    .iter()
                    .map(|span| span.content.as_ref())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n");

        assert!(rendered.contains("✓ Completed task"));
        assert!(rendered.contains("◉ Current task"));
    }

    fn node(
        id: &str,
        parent_id: Option<&str>,
        summary: Option<&str>,
        status: SpineTreeNodeStatus,
    ) -> SpineTreeNode {
        SpineTreeNode {
            id: id.to_owned(),
            parent_id: parent_id.map(str::to_owned),
            kind: SpineTreeNodeKind::Task,
            status,
            summary: summary.map(str::to_owned),
        }
    }
}
