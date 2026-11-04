//! Shared by the `community` providers (reddit, hackernews): one comment
//! record, the readable thread text an extract returns, and HTML-to-text for
//! the comment bodies both sites send as HTML.

use crate::types::SearchResult;
use dom_query::Document;
use std::collections::HashMap;

/// One comment of a thread, in thread order once [`order`] has run.
#[derive(Debug, Clone, PartialEq)]
pub struct Comment {
    pub id: String,
    /// The parent comment's id, or `None` for a top-level comment.
    pub parent: Option<String>,
    pub author: String,
    /// Reddit's score or HN's points; HN hides comment points (`None`).
    pub score: Option<i64>,
    /// 0 for a top-level comment.
    pub depth: usize,
    /// ISO 8601 timestamp.
    pub created: String,
    pub body: String,
}

/// The opening post of a thread.
#[derive(Debug, Clone, Default)]
pub struct Post {
    pub title: String,
    pub url: String,
    pub author: String,
    pub score: Option<i64>,
    pub num_comments: Option<u64>,
    /// `r/<name>` on Reddit; empty on HN.
    pub community: String,
    pub created: String,
    pub body: String,
    /// The link the post points to, when it is a link post.
    pub link: Option<String>,
}

/// Put `comments` in thread order: every comment after its parent, siblings
/// best-scored first (then oldest), depth recomputed from the tree. A
/// comment whose parent is not in the set becomes top-level, so nothing is
/// dropped.
pub fn order(comments: Vec<Comment>) -> Vec<Comment> {
    let ids: std::collections::HashSet<String> = comments.iter().map(|c| c.id.clone()).collect();
    let mut children: HashMap<Option<String>, Vec<Comment>> = HashMap::new();
    for c in comments {
        let parent = c.parent.clone().filter(|p| ids.contains(p) && *p != c.id);
        children.entry(parent).or_default().push(c);
    }
    for list in children.values_mut() {
        list.sort_by(|a, b| {
            b.score
                .unwrap_or(i64::MIN)
                .cmp(&a.score.unwrap_or(i64::MIN))
                .then_with(|| a.created.cmp(&b.created))
        });
    }
    let mut out = Vec::new();
    let mut stack: Vec<(Comment, usize)> = children
        .remove(&None)
        .unwrap_or_default()
        .into_iter()
        .rev()
        .map(|c| (c, 0))
        .collect();
    while let Some((mut c, depth)) = stack.pop() {
        c.depth = depth;
        if let Some(kids) = children.remove(&Some(c.id.clone())) {
            stack.extend(kids.into_iter().rev().map(|k| (k, depth + 1)));
        }
        out.push(c);
    }
    out
}

/// The thread as readable text: the post, then each comment as
/// `[score] author (depth N): body`, indented two spaces per depth.
pub fn render(post: &Post, comments: &[Comment]) -> String {
    let mut s = String::new();
    s.push_str("# ");
    s.push_str(&post.title);
    s.push_str("\n\n");
    let mut meta: Vec<String> = Vec::new();
    if !post.community.is_empty() {
        meta.push(post.community.clone());
    }
    if !post.author.is_empty() {
        meta.push(format!("by {}", post.author));
    }
    if let Some(score) = post.score {
        meta.push(format!("{score} points"));
    }
    if let Some(n) = post.num_comments {
        meta.push(format!("{n} comments"));
    }
    if !post.created.is_empty() {
        meta.push(post.created.clone());
    }
    if !meta.is_empty() {
        s.push_str(&meta.join(" · "));
        s.push_str("\n\n");
    }
    if let Some(link) = &post.link {
        s.push_str(&format!("Link: {link}\n\n"));
    }
    if !post.body.is_empty() {
        s.push_str(&post.body);
        s.push_str("\n\n");
    }
    s.push_str(&format!("## Comments ({} shown)\n\n", comments.len()));
    for c in comments {
        let indent = "  ".repeat(c.depth);
        let score = c.score.map_or("-".to_string(), |v| v.to_string());
        let mut lines = c.body.lines();
        let first = lines.next().unwrap_or("");
        s.push_str(&format!(
            "{indent}[{score}] {} (depth {}): {first}\n",
            c.author, c.depth
        ));
        for line in lines {
            s.push_str(&indent);
            s.push_str("    ");
            s.push_str(line);
            s.push('\n');
        }
    }
    s
}

/// One extract result for a thread. `extra` carries the post's numbers and
/// every comment as a record, plus `route` (which source served it).
pub fn result(
    provider: &str,
    post: &Post,
    comments: &[Comment],
    route: &str,
    notes: &[String],
) -> SearchResult {
    let records: Vec<serde_json::Value> = comments
        .iter()
        .map(|c| {
            serde_json::json!({
                "id": c.id,
                "parent": c.parent,
                "author": c.author,
                "score": c.score,
                "depth": c.depth,
                "created": c.created,
                "body": c.body,
            })
        })
        .collect();
    let mut extra = serde_json::json!({
        "content_origin": "untrusted_web_content",
        "author": post.author,
        "score": post.score,
        "num_comments": post.num_comments,
        "comments_returned": comments.len(),
        "route": route,
        "comments": records,
    });
    if let Some(obj) = extra.as_object_mut() {
        if let Some(sub) = post.community.strip_prefix("r/") {
            obj.insert("subreddit".into(), sub.into());
        }
        if let Some(link) = &post.link {
            obj.insert("link".into(), link.clone().into());
        }
        if !notes.is_empty() {
            obj.insert("notes".into(), notes.to_vec().into());
        }
    }
    SearchResult {
        title: post.title.clone(),
        url: post.url.clone(),
        snippet: render(post, comments),
        source: provider.to_string(),
        published: (!post.created.is_empty()).then(|| post.created.clone()),
        image_url: None,
        extra: Some(extra),
    }
}

/// Readable text from an HTML fragment: paragraphs, list items and line
/// breaks become lines, entities are decoded, runs of blank lines collapse.
pub fn html_to_text(html: &str) -> String {
    if !html.contains('<') && !html.contains('&') {
        return tidy(html);
    }
    let mut marked = String::with_capacity(html.len() + 64);
    let lower = html.to_ascii_lowercase();
    let mut last = 0;
    for (i, _) in lower.match_indices('<') {
        let tag = &lower[i..];
        let breaks = [
            "<p>",
            "<p ",
            "</p>",
            "<br",
            "</li>",
            "<li",
            "</pre>",
            "<pre",
            "</blockquote>",
            "</h",
        ]
        .iter()
        .any(|t| tag.starts_with(t));
        if breaks {
            marked.push_str(&html[last..i]);
            marked.push('\u{2029}');
            last = i;
        }
    }
    marked.push_str(&html[last..]);
    let doc = Document::fragment(marked);
    let text = doc.root().text().to_string();
    tidy(&text.replace('\u{2029}', "\n"))
}

/// Collapse spaces inside each line and blank-line runs to one.
fn tidy(text: &str) -> String {
    let mut out: Vec<String> = Vec::new();
    for line in text.lines() {
        let line = line.split_whitespace().collect::<Vec<_>>().join(" ");
        if line.is_empty() && out.last().is_none_or(|l| l.is_empty()) {
            continue;
        }
        out.push(line);
    }
    while out.last().is_some_and(|l| l.is_empty()) {
        out.pop();
    }
    out.join("\n")
}

/// Seconds since the epoch as an ISO 8601 UTC timestamp.
pub fn iso_from_unix(secs: i64) -> String {
    let days = secs.div_euclid(86_400);
    let rem = secs.rem_euclid(86_400);
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = yoe + era * 400 + i64::from(m <= 2);
    format!(
        "{y:04}-{m:02}-{d:02}T{:02}:{:02}:{:02}Z",
        rem / 3600,
        rem % 3600 / 60,
        rem % 60
    )
}

/// Clip to `max` chars on a char boundary, with an ellipsis when cut.
pub fn clip(text: &str, max: usize) -> String {
    if text.chars().count() <= max {
        return text.to_string();
    }
    let mut s: String = text.chars().take(max).collect();
    s.push('…');
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    fn c(id: &str, parent: Option<&str>, score: i64) -> Comment {
        Comment {
            id: id.into(),
            parent: parent.map(str::to_string),
            author: format!("u_{id}"),
            score: Some(score),
            depth: 99,
            created: "2024-01-01T00:00:00Z".into(),
            body: format!("body {id}"),
        }
    }

    #[test]
    fn order_puts_children_after_parents_best_first_and_keeps_orphans() {
        let out = order(vec![
            c("b1", Some("a"), 1),
            c("a", None, 5),
            c("z", None, 9),
            c("b2", Some("a"), 7),
            c("orphan", Some("gone"), 2),
        ]);
        let ids: Vec<(&str, usize)> = out.iter().map(|c| (c.id.as_str(), c.depth)).collect();
        assert_eq!(
            ids,
            [("z", 0), ("a", 0), ("b2", 1), ("b1", 1), ("orphan", 0)]
        );
    }

    #[test]
    fn render_indents_by_depth_and_shows_score_author_depth() {
        let post = Post {
            title: "T".into(),
            ..Post::default()
        };
        let mut reply = c("r", Some("a"), 3);
        reply.body = "line one\nline two".into();
        let text = render(&post, &order(vec![c("a", None, 5), reply]));
        assert!(text.contains("[5] u_a (depth 0): body a\n"), "{text}");
        assert!(
            text.contains("  [3] u_r (depth 1): line one\n      line two\n"),
            "{text}"
        );
    }

    #[test]
    fn html_to_text_keeps_paragraphs_and_decodes_entities() {
        assert_eq!(
            html_to_text("If Rust had I&#x2F;O<p>second &amp; <i>last</i>"),
            "If Rust had I/O\nsecond & last"
        );
        assert_eq!(html_to_text("plain   text"), "plain text");
    }

    #[test]
    fn iso_from_unix_matches_known_dates() {
        assert_eq!(iso_from_unix(1702320692), "2023-12-11T18:51:32Z");
        assert_eq!(iso_from_unix(0), "1970-01-01T00:00:00Z");
    }
}
