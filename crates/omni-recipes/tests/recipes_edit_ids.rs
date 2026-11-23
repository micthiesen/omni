//! Fewer round trips before an edit: the text and chart views can carry the
//! ids edit operations address, and an edit's own result always does.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use crate::common;

use common::{Harness, LASAGNE, PICCATA};
use serde_json::json;

#[tokio::test(start_paused = true)]
async fn the_text_view_marks_every_id_only_when_asked() {
    let h = Harness::new().await;
    let id = h.create(PICCATA).await;
    let plain = h
        .ok("recipes_get", json!({ "id": id, "view": "text" }))
        .await;
    let plain = plain["text"].as_str().unwrap();
    assert!(!plain.contains("[salt]"), "{plain}");

    let marked = h
        .ok(
            "recipes_get",
            json!({ "id": id, "view": "text", "ids": true }),
        )
        .await;
    let marked = marked["text"].as_str().unwrap();
    assert!(marked.contains("## Ingredients [piccata]"), "{marked}");
    assert!(marked.contains(" [salt]\n"), "{marked}");
    assert!(marked.contains("1. [dredge] "), "{marked}");
    assert!(marked.contains("[sear; by-products fond] "), "{marked}");
    // Without the markers the two read the same.
    let stripped: String = marked
        .replace(" [piccata]", "")
        .lines()
        .map(|line| {
            let line = match line.find(" [") {
                Some(at) if line.starts_with("- ") && line.ends_with(']') => &line[..at],
                _ => line,
            };
            match (line.find(". ["), line.find("] ")) {
                (Some(dot), Some(close)) if dot < close => {
                    format!("{}{}", &line[..dot + 2], &line[close + 2..])
                }
                _ => line.to_owned(),
            }
        })
        .collect::<Vec<_>>()
        .join("\n");
    assert_eq!(stripped.trim_end(), plain.trim_end());
}

#[tokio::test(start_paused = true)]
async fn components_and_chart_cells_carry_their_ids() {
    let h = Harness::new().await;
    let id = h.create(LASAGNE).await;
    let text = h
        .ok(
            "recipes_get",
            json!({ "id": id, "view": "text", "ids": true }),
        )
        .await;
    let text = text["text"].as_str().unwrap();
    assert!(text.contains("**Ragù** [ragu]"), "{text}");
    let chart = h
        .ok(
            "recipes_get",
            json!({ "id": id, "view": "chart", "ids": true }),
        )
        .await;
    let chart = chart["chart"].as_str().unwrap();
    assert!(chart.contains("[onion]"), "{chart}");
    let plain = h
        .ok("recipes_get", json!({ "id": id, "view": "chart" }))
        .await;
    assert!(!plain["chart"].as_str().unwrap().contains("[onion]"));
}

#[tokio::test(start_paused = true)]
async fn an_edit_result_reads_with_ids() {
    let h = Harness::new().await;
    let id = h.create(PICCATA).await;
    let edited = h
        .ok(
            "recipes_edit",
            json!({
                "id": id, "expectedRevision": 1, "summary": "More capers",
                "ops": [{ "op": "set", "target": "capers", "fields": { "qty": 3 } }],
            }),
        )
        .await;
    let text = edited["text"].as_str().unwrap();
    assert!(
        text.contains("capers") && text.contains("[capers]"),
        "{text}"
    );
}
