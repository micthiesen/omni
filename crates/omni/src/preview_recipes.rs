//! Synthetic recipes for `--preview`, so the recipe app has a library to
//! show when `RECIPES_APP_URL` is set: the four sample recipes of the format
//! and a small three-step one,
//! created through the same service the tools use. The first also gets what
//! the app's other screens need: notes, a stored source text, one finished
//! cook with a photo and an adjustment, and there is one photo waiting in the
//! inbox. Nothing here is real data.

use omni_api::fond::{CookActor, CookEventKind};
use omni_recipes::api::{CreateInput, TagGroup, TagOp, TagsEditInput};
use omni_recipes::cook::{Append, Start};
use omni_recipes::db::Actor;
use omni_runtime::AppContext;

const SAMPLES: [(&str, &[&str]); 5] = [
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
    (
        include_str!("preview_recipes/mustard-vinaigrette.json"),
        &["vegetarian"],
    ),
];

/// A drawn placeholder dish, not a photograph.
const DISH: &[u8] = include_bytes!("preview_recipes/dish.jpg");

const SOURCE_TEXT: &str = "Chicken Piccata\n\nServes 4\n\n\
    500 g chicken breasts, halved into cutlets\nsalt and pepper\n60 g flour\n\
    2 tbsp olive oil\n4 tbsp cold butter\n120 ml white wine\n180 ml chicken stock\n\
    2 lemons\n2 tbsp capers\nparsley\n\n\
    Season and dredge the cutlets. Sear in oil and half the butter until golden.\n\
    Deglaze with wine, add stock, lemon juice and capers, and reduce by half.\n\
    Swirl in the rest of the butter, return the chicken, and finish with parsley.";

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
    let mut first = None;
    for (index, (json, tags)) in SAMPLES.into_iter().enumerate() {
        let created = recipes
            .create(
                CreateInput {
                    document: serde_json::from_str(json)?,
                    tags: Some(tags.iter().map(|tag| (*tag).to_owned()).collect()),
                    create_tags: None,
                    notes: (index == 0).then(|| "Double the sauce for guests.".to_owned()),
                    from_inbox: None,
                    source_text: (index == 0).then(|| SOURCE_TEXT.to_owned()),
                    derived_from: None,
                    allow_duplicate: None,
                    review: Some(false),
                    dry_run: None,
                },
                actor(),
            )
            .await
            .map_err(failed)?;
        if index == 0 {
            first = created.id;
        }
    }
    if let Some(recipe_id) = first {
        let session = recipes
            .start_session(Start {
                id: None,
                recipe_id,
                revision: None,
                scale: None,
                servings: None,
            })
            .await
            .map_err(failed)?;
        recipes
            .cook_photo(&session.id, Some("preview".to_owned()), DISH.to_vec())
            .await
            .map_err(failed)?;
        let at = ctx.clock.now_ms();
        let event = |id: &str, kind| Append {
            actor: CookActor::App,
            client_event_id: id.to_owned(),
            at,
            kind,
        };
        recipes
            .append_events(
                &session.id,
                vec![
                    event(
                        "preview-adjust",
                        CookEventKind::Adjust {
                            target: "sear".to_owned(),
                            text: "Sear 5 minutes per side; the new pan runs cool.".to_owned(),
                        },
                    ),
                    event("preview-finish", CookEventKind::Finish { note: None }),
                ],
                true,
                Actor::app(),
            )
            .await
            .map_err(failed)?;
    }
    recipes
        .upload_photo(
            "preview-card",
            0,
            1,
            Some("Recipe card".to_owned()),
            DISH.to_vec(),
        )
        .await
        .map_err(failed)?;
    Ok(())
}
