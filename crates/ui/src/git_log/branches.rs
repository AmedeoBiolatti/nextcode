//! The Log's branches tree: HEAD, Local, Remote (per remote) and Tags.
//! Double-clicking a row filters the log to that branch.

use git::{Ref, RefKind};
use gpui::{ClickEvent, Context, div, prelude::*, px};

use super::GitLogPanel;
use crate::{pinpoint::Pin, selectable::selectable, theme};

pub(super) const LOCAL: &str = "local";
pub(super) const REMOTE: &str = "remote";
pub(super) const TAGS: &str = "tags";

const ROW_HEIGHT: f32 = 22.;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum BranchRow {
    /// The checked-out branch (or detached HEAD).
    Head { label: String },
    /// A collapsible group: `local`, `remote`, `remote/<name>`, `tags`.
    Group {
        key: String,
        label: String,
        depth: usize,
        collapsed: bool,
    },
    Ref {
        /// The full short name (`origin/main`): what the filter uses.
        name: String,
        /// Shown name (`main` under its remote).
        label: String,
        kind: RefKind,
        current: bool,
        depth: usize,
    },
}

impl BranchRow {
    /// The revision the log is filtered to when the row is activated.
    pub(super) fn revision(&self) -> Option<&str> {
        match self {
            Self::Head { .. } => Some("HEAD"),
            Self::Ref { name, .. } => Some(name),
            Self::Group { .. } => None,
        }
    }
}

/// Local branches (current first), remote branches, then tags, each by name.
pub(super) fn sorted_refs(refs: &[Ref]) -> Vec<&Ref> {
    let mut sorted: Vec<&Ref> = refs.iter().collect();
    sorted.sort_by(|a, b| (a.kind as u8, !a.head, &a.name).cmp(&(b.kind as u8, !b.head, &b.name)));
    sorted
}

/// The tree's rows for `refs`, leaving out children of collapsed groups.
pub(super) fn rows(
    refs: &[Ref],
    head_branch: Option<&str>,
    collapsed: &std::collections::HashSet<String>,
) -> Vec<BranchRow> {
    let mut rows = vec![BranchRow::Head {
        label: match head_branch {
            Some(branch) => format!("HEAD (Current Branch: {branch})"),
            None => "HEAD (detached)".into(),
        },
    }];
    let sorted = sorted_refs(refs);
    let group = |rows: &mut Vec<BranchRow>, key: &str, label: &str, depth: usize| {
        let is_collapsed = collapsed.contains(key);
        rows.push(BranchRow::Group {
            key: key.to_owned(),
            label: label.to_owned(),
            depth,
            collapsed: is_collapsed,
        });
        is_collapsed
    };
    let reference = |r: &Ref, label: &str, depth: usize| BranchRow::Ref {
        name: r.name.clone(),
        label: label.to_owned(),
        kind: r.kind,
        current: r.head,
        depth,
    };

    let local: Vec<&&Ref> = sorted
        .iter()
        .filter(|r| r.kind == RefKind::Branch)
        .collect();
    if !local.is_empty() && !group(&mut rows, LOCAL, "Local", 0) {
        rows.extend(local.iter().map(|r| reference(r, &r.name, 1)));
    }

    let remote: Vec<&&Ref> = sorted
        .iter()
        .filter(|r| r.kind == RefKind::Remote)
        .collect();
    if !remote.is_empty() && !group(&mut rows, REMOTE, "Remote", 0) {
        let mut remotes: Vec<&str> = remote
            .iter()
            .map(|r| {
                r.name
                    .split_once('/')
                    .map_or(&*r.name, |(remote, _)| remote)
            })
            .collect();
        remotes.dedup();
        for name in remotes {
            let key = format!("{REMOTE}/{name}");
            if group(&mut rows, &key, name, 1) {
                continue;
            }
            for r in &remote {
                if let Some(branch) = r
                    .name
                    .strip_prefix(name)
                    .and_then(|rest| rest.strip_prefix('/'))
                {
                    rows.push(reference(r, branch, 2));
                }
            }
        }
    }

    let tags: Vec<&&Ref> = sorted.iter().filter(|r| r.kind == RefKind::Tag).collect();
    if !tags.is_empty() && !group(&mut rows, TAGS, "Tags", 0) {
        rows.extend(tags.iter().map(|r| reference(r, &r.name, 1)));
    }
    rows
}

impl GitLogPanel {
    pub(super) fn rebuild_branches(&mut self) {
        let selected = self
            .selected_branch
            .and_then(|index| self.branch_rows.get(index))
            .cloned();
        let head_branch = self.head.as_ref().and_then(|head| head.branch.as_deref());
        self.branch_rows = rows(&self.refs, head_branch, &self.collapsed_groups);
        self.selected_branch = selected.and_then(|row| {
            self.branch_rows
                .iter()
                .position(|candidate| match (&row, candidate) {
                    (BranchRow::Head { .. }, BranchRow::Head { .. }) => true,
                    _ => *candidate == row,
                })
        });
    }

    fn click_branch(&mut self, index: usize, event: &ClickEvent, cx: &mut Context<Self>) {
        self.selected_branch = Some(index);
        match self.branch_rows.get(index).cloned() {
            Some(BranchRow::Group { key, .. }) => {
                if !self.collapsed_groups.remove(&key) {
                    self.collapsed_groups.insert(key);
                }
                self.rebuild_branches();
            }
            Some(row) if event.click_count() >= 2 => {
                let revision = row.revision().map(str::to_owned);
                if let Some(revision) = revision {
                    self.set_filter(|filter| filter.revisions = vec![revision], cx);
                }
            }
            _ => {}
        }
        cx.notify();
    }

    pub(super) fn render_branches(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let filtered: Option<&str> = match self.filter.revisions.as_slice() {
            [revision] => Some(revision),
            _ => None,
        };
        let rows = self.branch_rows.iter().enumerate().map(|(index, row)| {
            let selected = self.selected_branch == Some(index);
            let filtering = filtered.is_some() && row.revision() == filtered;
            let base = div()
                .id(("branch-row", index))
                .h(px(ROW_HEIGHT))
                .pr_2()
                .flex()
                .items_center()
                .gap_1()
                .text_sm()
                .cursor_pointer()
                .when(selected, |row| row.bg(theme::active_row()))
                .when(!selected, |row| {
                    row.hover(|style| style.bg(theme::hover_row()))
                })
                .on_click(cx.listener(move |this, event: &ClickEvent, _, cx| {
                    this.click_branch(index, event, cx)
                }));
            let (key, text) = match row {
                BranchRow::Head { label } => ("head".to_owned(), label),
                BranchRow::Group { key, label, .. } => (format!("group:{key}"), label),
                BranchRow::Ref {
                    name, label, kind, ..
                } => {
                    let kind = if *kind == RefKind::Tag {
                        "tag"
                    } else {
                        "branch"
                    };
                    (format!("{kind}:{name}"), label)
                }
            };
            let text = text.clone();
            match row {
                BranchRow::Head { label } => base
                    .pl(px(8.))
                    .child(div().text_color(theme::accent_text()).child("◉"))
                    .child(
                        div()
                            .min_w_0()
                            .truncate()
                            .when(filtering, |text| text.font_weight(gpui::FontWeight::BOLD))
                            .child(selectable("label", label.clone()).double_click_through()),
                    ),
                BranchRow::Group {
                    label,
                    depth,
                    collapsed,
                    ..
                } => base
                    .pl(px(8. + 14. * *depth as f32))
                    .text_color(theme::muted())
                    .child(if *collapsed { "▸" } else { "▾" })
                    .child(selectable("label", label.clone()).double_click_through()),
                BranchRow::Ref {
                    label,
                    kind,
                    current,
                    depth,
                    ..
                } => {
                    let (icon, color) = match kind {
                        RefKind::Tag => ("◆", theme::tag()),
                        _ if *current => ("★", theme::accent_text()),
                        _ => ("⎇", theme::muted()),
                    };
                    base.pl(px(8. + 14. * *depth as f32))
                        .child(div().w(px(14.)).flex_none().text_color(color).child(icon))
                        .child(
                            div()
                                .min_w_0()
                                .truncate()
                                .text_color(theme::text())
                                .when(filtering, |text| text.font_weight(gpui::FontWeight::BOLD))
                                .child(selectable("label", label.clone()).double_click_through()),
                        )
                }
            }
            .pin(key)
            .label(text)
        });
        div()
            .size_full()
            .flex()
            .flex_col()
            .child(
                div()
                    .flex_none()
                    .h(px(32.))
                    .px_2()
                    .flex()
                    .items_center()
                    .border_b_1()
                    .border_color(theme::border())
                    .text_xs()
                    .text_color(theme::muted())
                    .child(selectable("branches-title", "Branches")),
            )
            .child(
                div()
                    .id("log-branches")
                    .flex_1()
                    .min_h_0()
                    .overflow_y_scroll()
                    .py_1()
                    .children(rows)
                    .pin("branches"),
            )
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashSet;

    use super::*;

    fn r(name: &str, kind: RefKind, head: bool) -> Ref {
        Ref {
            name: name.into(),
            kind,
            target: "abc".into(),
            head,
        }
    }

    fn labels(rows: &[BranchRow]) -> Vec<String> {
        rows.iter()
            .map(|row| match row {
                BranchRow::Head { label } => label.clone(),
                BranchRow::Group { label, depth, .. } => format!("{}{label}/", "  ".repeat(*depth)),
                BranchRow::Ref { label, depth, .. } => format!("{}{label}", "  ".repeat(*depth)),
            })
            .collect()
    }

    #[test]
    fn refs_are_grouped_by_kind_and_remote() {
        let refs = [
            r("v1", RefKind::Tag, false),
            r("upstream/main", RefKind::Remote, false),
            r("zeta", RefKind::Branch, false),
            r("origin/main", RefKind::Remote, false),
            r("main", RefKind::Branch, true),
            r("origin/fix/x", RefKind::Remote, false),
        ];
        let rows = rows(&refs, Some("main"), &HashSet::new());
        assert_eq!(
            labels(&rows),
            [
                "HEAD (Current Branch: main)",
                "Local/",
                "  main",
                "  zeta",
                "Remote/",
                "  origin/",
                "    fix/x",
                "    main",
                "  upstream/",
                "    main",
                "Tags/",
                "  v1",
            ]
        );
        assert_eq!(rows[7].revision(), Some("origin/main"));
        assert_eq!(rows[0].revision(), Some("HEAD"));

        let collapsed = HashSet::from(["remote/origin".to_owned(), TAGS.to_owned()]);
        let rows = super::rows(&refs, None, &collapsed);
        assert_eq!(
            labels(&rows),
            [
                "HEAD (detached)",
                "Local/",
                "  main",
                "  zeta",
                "Remote/",
                "  origin/",
                "  upstream/",
                "    main",
                "Tags/",
            ]
        );
    }
}
