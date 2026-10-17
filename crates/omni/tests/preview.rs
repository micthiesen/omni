//! The `--preview` fixture data decodes through every typed model and shows
//! up in the dashboard routes.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use axum::http::StatusCode;
use omni_testkit::TestApp;

#[tokio::test]
async fn seeds_typed_fixture_data_the_routes_serve() {
    let app = TestApp::new().await;
    let mut wired = omni::wiring::wire(&app.ctx, 0).await.unwrap();
    let now = app.ctx.clock.now_ms();
    omni::preview::seed(&app.ctx, now).await.unwrap();
    for task in omni::preview::fake_tasks().unwrap() {
        app.ctx.tasks.track(task).unwrap();
    }
    let (router, _) = omni::boot::app_router(
        &app.ctx,
        &mut wired.subsystems,
        std::path::Path::new("/nonexistent"),
    );
    let (status, snapshot) = app.get_json(&router, "/api/snapshot").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(snapshot["tasks"].as_array().unwrap().len(), 6);
    let runs = snapshot["runs"].as_array().unwrap();
    assert_eq!(runs.len(), 18);
    assert!(runs.iter().any(|run| run["status"] == "degraded"));
    assert_eq!(snapshot["onDeck"].as_array().unwrap().len(), 3);
    let (_, recs) = app.get_json(&router, "/api/recommendations").await;
    assert_eq!(recs["recommendations"].as_array().unwrap().len(), 4);
    let (_, pets) = app.get_json(&router, "/api/pets").await;
    assert_eq!(pets[0]["name"], "Mochi");
    assert_eq!(pets[0]["weightHistory"].as_array().unwrap().len(), 91);
    let (_, activity) = app.get_json(&router, "/api/email-activity").await;
    assert_eq!(activity["activities"].as_array().unwrap().len(), 1);
    omni::preview_stylist::seed(&app.ctx).await.unwrap();
    let (status, stylist) = app.get_json(&router, "/api/stylist").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(stylist["wardrobe"].as_array().unwrap().len(), 5);
    let forecast = &stylist["predictions"][0];
    assert_eq!(forecast["contextVerified"], true);
    assert_eq!(forecast["outcomes"].as_array().unwrap().len(), 2);
    assert!(
        stylist["due"]
            .as_array()
            .unwrap()
            .iter()
            .any(|d| d["kind"] == "return_decision")
    );
    assert!(
        stylist["contradictions"]
            .as_array()
            .unwrap()
            .iter()
            .any(|c| c["kind"] == "body_measurement")
    );
    assert_eq!(stylist["archives"].as_array().unwrap().len(), 1);
}

#[test]
fn preview_streamers_are_valid_and_one_is_background() {
    let rows = omni_live::config::rows_for(omni::preview::preview_streamers(), 0).unwrap();
    assert_eq!(rows.len(), 4);
    assert_eq!(rows[3].tier, omni_api::streamers::StreamerTier::Background);
}
