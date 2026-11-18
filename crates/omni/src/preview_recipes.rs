//! Synthetic recipes for `--preview`, so the recipe app has a library to
//! show when `RECIPES_APP_URL` is set: the four sample recipes of the format,
//! created through the same service the tools use. Nothing here is real data.

use omni_recipes::api::{CreateInput, TagGroup, TagOp, TagsEditInput};
use omni_recipes::db::Actor;
use omni_runtime::AppContext;

const SAMPLES: [(&str, &[&str]); 4] = [
    (
        omni_recipes::guide::EXAMPLE,
        &["main", "italian", "chicken", "weeknight"],
    ),
    (
        include_str!("preview_recipes/chocolate-chip-cookies.json"),
        &["dessert", "baking"],
    ),
    (
        include_str!("preview_recipes/lasagne-bolognese.json"),
        &["main", "italian", "beef", "weekend"],
    ),
    (
        include_str!("preview_recipes/enchiladas.json"),
        &["main", "mexican", "chicken"],
    ),
];

const TAGS: [(&str, TagGroup); 10] = [
    ("main", TagGroup::Course),
    ("dessert", TagGroup::Course),
    ("italian", TagGroup::Cuisine),
    ("mexican", TagGroup::Cuisine),
    ("chicken", TagGroup::Main),
    ("beef", TagGroup::Main),
    ("baking", TagGroup::Method),
    ("weeknight", TagGroup::Occasion),
    ("weekend", TagGroup::Occasion),
    ("vegetarian", TagGroup::Diet),
];

pub async fn seed(ctx: &AppContext) -> anyhow::Result<()> {
    let recipes = omni_recipes::service(ctx);
    let actor = || Actor::new("preview", "");
    let failed = |error: omni_recipes::RecipesError| anyhow::anyhow!(error.tool_text());
    recipes
        .tags_edit(
            TagsEditInput {
                ops: TAGS
                    .iter()
                    .map(|(name, group)| TagOp::Create {
                        name: (*name).to_owned(),
                        group: *group,
                        description: None,
                    })
                    .collect(),
                dry_run: None,
            },
            actor(),
        )
        .await
        .map_err(failed)?;
    for (json, tags) in SAMPLES {
        recipes
            .create(
                CreateInput {
                    document: serde_json::from_str(json)?,
                    tags: Some(tags.iter().map(|tag| (*tag).to_owned()).collect()),
                    create_tags: None,
                    notes: None,
                    from_inbox: None,
                    source_text: None,
                    derived_from: None,
                    allow_duplicate: None,
                    review: Some(false),
                    dry_run: None,
                },
                actor(),
            )
            .await
            .map_err(failed)?;
    }
    Ok(())
}
