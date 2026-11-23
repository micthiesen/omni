//! Variants: named versions of one recipe, each a list of field changes,
//! checked and rendered as the document they make. The shortbread sample
//! carries every step field added with them (counts, sizes, heat stages,
//! batches) and renders to its committed output.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use omni_recipe_format::{
    Code, Marks, Op, RecipeDoc, Variant, VariantError, VariantOp, apply_ops, chart_ascii,
    chart_blocks, chart_html, diff_summary, render_list, render_timeline, validate, variant_doc,
};
use serde_json::{Value, json};

const JSON: &str = include_str!("golden/hardtack-shortbread.json");

fn shortbread() -> RecipeDoc {
    serde_json::from_str(JSON).unwrap()
}

fn set(target: &str, fields: Value) -> VariantOp {
    VariantOp::Set {
        target: target.to_owned(),
        fields: fields.as_object().unwrap().clone(),
        unset: Vec::new(),
    }
}

fn variant(id: &str, ops: Vec<VariantOp>) -> Variant {
    Variant {
        id: id.to_owned(),
        name: "Test".to_owned(),
        note: None,
        ops,
    }
}

#[test]
fn the_shortbread_sample_renders_byte_for_byte() {
    let doc = shortbread();
    let report = validate(&doc);
    assert!(
        report.errors.is_empty() && report.warnings.is_empty(),
        "{report:?}"
    );
    let stored: Value = serde_json::from_str(JSON).unwrap();
    assert_eq!(
        serde_json::to_value(&doc).unwrap(),
        stored,
        "the document round-trips"
    );
    assert_eq!(
        render_list(&doc, 1.0),
        include_str!("golden/hardtack-shortbread.list.md")
    );
    let chart: Vec<String> = chart_blocks(&doc, 1.0)
        .iter()
        .map(|block| chart_ascii(block, &Marks::new()))
        .collect();
    assert_eq!(
        format!("{}\n", chart.join("\n\n")),
        include_str!("golden/hardtack-shortbread.chart.txt")
    );
    let html: Vec<String> = chart_blocks(&doc, 1.0).iter().map(chart_html).collect();
    assert_eq!(
        format!("{}\n", html.join("\n")),
        include_str!("golden/hardtack-shortbread.chart.html")
    );
    assert_eq!(
        format!("{}\n", render_timeline(&doc)),
        include_str!("golden/hardtack-shortbread.timeline.txt")
    );
    let hardtack = variant_doc(&doc, "hardtack").unwrap();
    assert!(hardtack.variants.is_empty());
    assert_eq!(
        render_list(&hardtack, 2.0),
        include_str!("golden/hardtack-shortbread.hardtack.list.md")
    );
}

#[test]
fn a_variant_cannot_change_more_than_fields_yield_and_time() {
    let mut doc = shortbread();
    doc.variants = vec![variant(
        "titled",
        vec![VariantOp::SetRecipe {
            fields: json!({ "title": "Other" }).as_object().unwrap().clone(),
            unset: Vec::new(),
        }],
    )];
    let report = validate(&doc);
    assert_eq!(report.errors.len(), 1, "{:?}", report.errors);
    assert_eq!(report.errors[0].path, "variants.titled.ops");
    assert!(report.errors[0].message.contains("not \"title\""));
    assert!(matches!(
        variant_doc(&doc, "titled"),
        Err(VariantError::Refused { .. })
    ));
    assert!(matches!(
        variant_doc(&doc, "nope"),
        Err(VariantError::Unknown { .. })
    ));
}

#[test]
fn a_variants_own_problems_are_reported_under_its_path() {
    let mut doc = shortbread();
    doc.variants = vec![
        variant("more", vec![set("flour", json!({ "qty": 113 }))]),
        variant("lost", vec![set("nothere", json!({ "qty": 1 }))]),
        variant("bad id!", vec![set("flour", json!({ "qty": 300 }))]),
    ];
    let report = validate(&doc);
    let errors: Vec<String> = report
        .errors
        .iter()
        .map(|p| format!("{} [{}]", p.path, p.code))
        .collect();
    assert_eq!(
        errors,
        ["variants.lost.ops [bad_value]", "variants.bad id! [bad_id]",]
    );
    // 113 g is off the measuring grid only in the variant.
    let warnings: Vec<String> = report
        .warnings
        .iter()
        .map(|p| format!("{} [{}]", p.path, p.code))
        .collect();
    assert!(
        warnings.contains(&"variants.more.ingredients.flour [awkward_amount]".to_owned()),
        "{warnings:?}"
    );
    assert!(
        !warnings.iter().any(|w| w.starts_with("ingredients.")),
        "{warnings:?}"
    );
}

#[test]
fn set_variant_remove_variant_rename_and_rescale_keep_variants_in_step() {
    let doc = shortbread();
    let added = apply_ops(
        &doc,
        &[Op::SetVariant {
            variant: Box::new(variant(
                "rich",
                vec![
                    set("butter", json!({ "qty": 250 })),
                    VariantOp::ReplaceText {
                        target: "cream".to_owned(),
                        find: "{sugar}".to_owned(),
                        replace: "{sugar} and a pinch more {butter|butter}".to_owned(),
                    },
                ],
            )),
        }],
    )
    .unwrap();
    assert_eq!(added.variants.len(), 2);
    assert_eq!(diff_summary(&doc, &added), ["+ variant rich"]);

    let renamed = apply_ops(
        &added,
        &[Op::Rename {
            target: "butter".to_owned(),
            to: "fat".to_owned(),
        }],
    )
    .unwrap();
    let rich = &renamed.variants[1];
    assert_eq!(rich.ops[0], set("fat", json!({ "qty": 250 })));
    assert!(
        matches!(&rich.ops[1], VariantOp::ReplaceText { replace, .. }
        if replace == "{sugar} and a pinch more {fat|butter}")
    );
    assert!(
        validate(&renamed).is_valid(),
        "{:?}",
        validate(&renamed).errors
    );

    let doubled = apply_ops(&added, &[Op::Rescale { factor: 2.0 }]).unwrap();
    assert_eq!(
        doubled.variants[1].ops[0],
        set("butter", json!({ "qty": 500 }))
    );
    let VariantOp::SetRecipe { fields, .. } = &doubled.variants[0].ops[0] else {
        panic!("the hardtack yield");
    };
    assert_eq!(fields["yield"]["qty"], 72);

    let removed = apply_ops(
        &added,
        &[Op::RemoveVariant {
            id: "hardtack".to_owned(),
        }],
    )
    .unwrap();
    assert_eq!(removed.variants.len(), 1);
    assert_eq!(diff_summary(&added, &removed), ["− variant hardtack"]);
    let missing = apply_ops(
        &removed,
        &[Op::RemoveVariant {
            id: "hardtack".to_owned(),
        }],
    );
    assert!(missing.is_err());
}

#[test]
fn removing_a_step_a_variant_changes_is_refused_by_the_validator() {
    let doc = shortbread();
    let mut cut = apply_ops(
        &doc,
        &[Op::Remove {
            target: "cool".to_owned(),
        }],
    )
    .unwrap();
    cut.variants = vec![variant(
        "cooler",
        vec![set("cool", json!({ "until": "cold" }))],
    )];
    let report = validate(&cut);
    assert!(
        report
            .errors
            .iter()
            .any(|p| p.path == "variants.cooler.ops" && p.code == Code::BadValue),
        "{:?}",
        report.errors
    );
}
