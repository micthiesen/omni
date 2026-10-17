//! Synthetic stylist ledger for `--preview`: a small invented wardrobe in the
//! preview's throwaway database, written through the same service the MCP
//! tools use, so the `/stylist` page has every section to show. Nothing here
//! describes a real person.

use jiff::ToSpan;
use jiff::civil::Date;
use omni_runtime::AppContext;
use omni_stylist::Stylist;
use omni_stylist::api::{AppendEventInput, ContextInput, SaveRecordInput};
use omni_stylist::archive::Reason;
use omni_stylist::types::Namespace;
use serde::de::DeserializeOwned;
use serde_json::{Value, json};

struct Seeder {
    stylist: Stylist,
    namespace: Namespace,
    today: Date,
    next: u32,
}

fn decode<T: DeserializeOwned>(value: Value) -> anyhow::Result<T> {
    Ok(serde_json::from_value(value)?)
}

impl Seeder {
    fn key(&mut self) -> String {
        self.next += 1;
        format!("preview-seed-{:08}", self.next)
    }

    /// `days` days before today, as YYYY-MM-DD.
    fn ago(&self, days: i64) -> String {
        self.today
            .checked_sub(days.days())
            .unwrap_or(self.today)
            .to_string()
    }

    async fn save(&mut self, record: Value) -> anyhow::Result<String> {
        let input: SaveRecordInput = decode(json!({
            "namespace": self.namespace, "idempotencyKey": self.key(), "record": record,
        }))?;
        let saved = self
            .stylist
            .save_record(input)
            .await
            .map_err(|e| anyhow::anyhow!(e.tool_text()))?;
        Ok(saved.record.id)
    }

    async fn revise(&mut self, id: &str, revision: u64, record: Value) -> anyhow::Result<()> {
        let input: SaveRecordInput = decode(json!({
            "namespace": self.namespace, "idempotencyKey": self.key(), "id": id,
            "expectedRevision": revision, "record": record,
        }))?;
        self.stylist
            .save_record(input)
            .await
            .map_err(|e| anyhow::anyhow!(e.tool_text()))?;
        Ok(())
    }

    async fn event(&mut self, days_ago: i64, event: Value) -> anyhow::Result<String> {
        let input: AppendEventInput = decode(json!({
            "namespace": self.namespace, "idempotencyKey": self.key(),
            "occurredOn": self.ago(days_ago), "event": event,
        }))?;
        let appended = self
            .stylist
            .append_event(input)
            .await
            .map_err(|e| anyhow::anyhow!(e.tool_text()))?;
        Ok(appended.event.id)
    }
}

fn provenance() -> Value {
    json!({"certainty": "explicit", "source": "guided_measurement"})
}

fn stated() -> Value {
    json!({"certainty": "explicit", "source": "user_statement"})
}

fn variant(brand: &str, model: &str, category: &str, size: &str, fiber: &str) -> Value {
    json!({
        "kind": "variant", "brand": brand, "model": model, "category": category,
        "sizeLabel": size, "sizeSystem": if category.starts_with("bottoms") { "waist_inseam" } else { "alpha" },
        "cut": "regular",
        "fabric": {"composition": [{"fiber": fiber, "percent": 100}], "construction": if category.starts_with("bottoms") { "woven" } else { "knit" }},
        "provenance": {"certainty": "explicit", "source": "label"},
    })
}

fn body(dimension: &str, method: &str, cm: f64, observed: &str) -> Value {
    json!({
        "kind": "measurement", "subject": {"type": "body"}, "dimension": dimension,
        "method": method, "endpoints": "suggested landmarks, soft tape", "value": cm, "unit": "cm",
        "tolerance": 0.5, "observedOn": observed, "provenance": provenance(),
    })
}

fn garment(
    piece: &str,
    dimension: &str,
    method: &str,
    cm: f64,
    wash: &str,
    observed: &str,
) -> Value {
    json!({
        "kind": "measurement", "subject": {"type": "piece", "id": piece}, "dimension": dimension,
        "method": method, "endpoints": "laid flat, seam to seam", "value": cm, "unit": "cm",
        "garmentState": {"stretch": "taut", "wash": wash}, "observedOn": observed,
        "provenance": provenance(),
    })
}

/// Seeds the `real` namespace of the preview database.
pub async fn seed(ctx: &AppContext) -> anyhow::Result<()> {
    let stylist = omni_stylist::service(ctx);
    let today = stylist.today();
    let mut s = Seeder {
        stylist,
        namespace: Namespace::real(),
        today,
        next: 0,
    };
    let measured = s.ago(20);
    s.save(json!({
        "kind": "profile_snapshot", "observedOn": measured,
        "attributes": [
            {"name": "height", "number": 180, "unit": "cm", "certainty": "explicit", "source": "user_measurement"},
            {"name": "build", "text": "long torso, average shoulders", "certainty": "explicit", "source": "user_statement"},
        ],
        "fitSensitivities": [{"region": "armhole", "note": "dislikes high, tight armholes", "certainty": "explicit"}],
        "provenance": stated(),
    }))
    .await?;
    for (dimension, method, cm) in [
        ("chest", "circumference", 101.5),
        ("waist_trouser", "circumference", 86.0),
        ("hip", "circumference", 99.0),
        ("shoulder_width", "linear", 46.5),
        ("arm_length", "linear", 63.0),
        ("inseam", "linear", 81.0),
    ] {
        s.save(body(dimension, method, cm, &measured)).await?;
    }
    s.save(body("chest", "circumference", 104.5, &s.ago(10)))
        .await?;

    let tee = s
        .save(variant(
            "Northline",
            "Heavy Crew Tee",
            "tops/tshirt",
            "M",
            "cotton",
        ))
        .await?;
    let tee_piece = s
        .save(json!({
            "kind": "piece", "label": "white heavy tee", "category": "tops/tshirt", "variantId": tee,
            "reference": {"categories": ["tops/tshirt"], "role": "baseline", "regions": ["chest", "shoulders", "body_length"]},
            "provenance": stated(),
        }))
        .await?;
    let tee_wash = s
        .event(
            60,
            json!({"type": "wash", "pieceId": tee_piece, "water": "cold", "drying": "line_dry"}),
        )
        .await?;
    s.event(40, json!({"type": "wear", "pieceId": tee_piece,
        "regionFit": [{"region": "chest", "verdict": "good"}, {"region": "body_length", "verdict": "good"}]})).await?;
    let mut chest = garment(
        &tee_piece,
        "chest",
        "flat_width",
        54.0,
        "washed",
        &s.ago(30),
    );
    chest["garmentState"]["afterEventId"] = json!(tee_wash);
    s.save(chest).await?;
    s.save(garment(
        &tee_piece,
        "body_length",
        "linear",
        72.0,
        "washed",
        &s.ago(30),
    ))
    .await?;
    s.save(garment(
        &tee_piece,
        "shoulder_width",
        "linear",
        47.0,
        "washed",
        &s.ago(30),
    ))
    .await?;

    let jeans = s
        .save(variant(
            "Coastline Denim",
            "Straight 502",
            "bottoms/jeans",
            "32x32",
            "cotton",
        ))
        .await?;
    let jeans_piece = s
        .save(json!({
            "kind": "piece", "label": "indigo straight jeans", "category": "bottoms/jeans", "variantId": jeans,
            "reference": {"categories": ["bottoms/jeans"], "role": "baseline", "regions": ["waist", "thigh"]},
            "provenance": stated(),
        }))
        .await?;
    s.event(
        90,
        json!({"type": "wash", "pieceId": jeans_piece, "water": "cold", "drying": "line_dry"}),
    )
    .await?;
    s.event(45, json!({"type": "feedback", "targetId": jeans_piece, "timing": "after_wear",
        "regionFit": [{"region": "waist", "verdict": "good"}, {"region": "inseam", "verdict": "slightly_long"}],
        "value": {"rating": "positive", "wearCount": 60}})).await?;
    s.save(garment(
        &jeans_piece,
        "waist",
        "flat_width",
        43.0,
        "washed",
        &s.ago(30),
    ))
    .await?;
    s.save(garment(
        &jeans_piece,
        "inseam",
        "linear",
        82.0,
        "washed",
        &s.ago(30),
    ))
    .await?;

    let shirt = s
        .save(variant(
            "Fenwick",
            "Oxford Shirt Slim",
            "tops/shirt",
            "M",
            "cotton",
        ))
        .await?;
    let shirt_piece = s
        .save(
            json!({"kind": "piece", "label": "blue oxford (returned)", "category": "tops/shirt",
            "variantId": shirt, "initialStatus": "undecided", "provenance": stated()}),
        )
        .await?;
    s.event(120, json!({"type": "try_on", "pieceId": shirt_piece, "overall": "wrong_size",
        "regionFit": [{"region": "shoulders", "verdict": "too_tight"}, {"region": "sleeve", "verdict": "good"}]})).await?;
    s.event(
        119,
        json!({"type": "decision", "pieceId": shirt_piece, "decision": "return",
        "reasons": [{"aspect": "fit", "region": "shoulders", "note": "pulls across the back"}]}),
    )
    .await?;
    s.event(115, json!({"type": "returned", "pieceId": shirt_piece}))
        .await?;

    let context = s
        .stylist
        .context(decode::<ContextInput>(
            json!({"namespace": "real", "includeRunbook": false}),
        )?)
        .await
        .map_err(|e| anyhow::anyhow!(e.tool_text()))?;
    let relaxed = s
        .save(variant(
            "Northline",
            "Relaxed Pocket Tee",
            "tops/tshirt",
            "M",
            "cotton",
        ))
        .await?;
    let forecast = s
        .save(json!({
            "kind": "prediction", "variantId": relaxed, "recommendedSize": "M",
            "decision": "recommend_with_caution", "basis": "known_brand_new_cut",
            "regionExpectations": [
                {"region": "chest", "expected": "slightly_loose", "rationale": "published chest 2 cm over the washed reference"},
                {"region": "body_length", "expected": "good", "rationale": "same length as the reference before washing"},
            ],
            "likelyFailureMode": {"region": "body_length", "description": "cotton jersey may shorten after hot washing"},
            "evidenceGaps": ["no post-wash measurements for this model"],
            "returnRisk": {"level": "medium", "rationale": "free returns within 30 days, unwashed only"},
            "contextRevision": context.snapshot.ledger_revision,
            "contextToken": context.snapshot.context_token,
        }))
        .await?;
    let pocket = s
        .save(
            json!({"kind": "piece", "label": "grey pocket tee", "category": "tops/tshirt",
            "variantId": relaxed, "initialStatus": "undecided", "provenance": stated()}),
        )
        .await?;
    s.event(0, json!({"type": "acquired", "pieceId": pocket, "predictionId": forecast, "statusAfter": "undecided",
        "order": {"retailer": "Northline", "orderRef": "NL-PREVIEW-1", "returnBy": s.ago(-18)}})).await?;
    let try_on = s.event(0, json!({"type": "try_on", "pieceId": pocket, "predictionId": forecast, "overall": "fits",
        "regionFit": [{"region": "chest", "verdict": "slightly_loose"}, {"region": "body_length", "verdict": "good"}]})).await?;
    s.event(0, json!({"type": "decision", "pieceId": pocket, "decision": "keep", "reasons": [{"aspect": "fit"}]})).await?;
    s.event(0, json!({"type": "outcome", "predictionId": forecast, "pieceId": pocket, "stage": "immediate",
        "result": "matched", "basedOnEventIds": [try_on],
        "regionResults": [{"region": "chest", "verdict": "slightly_loose"}, {"region": "body_length", "verdict": "good"}]})).await?;
    let wash = s
        .event(
            0,
            json!({"type": "wash", "pieceId": pocket, "water": "hot", "drying": "tumble_high",
        "observedChanges": [{"region": "body_length", "change": "shrank"}]}),
        )
        .await?;
    s.event(0, json!({"type": "outcome", "predictionId": forecast, "pieceId": pocket, "stage": "after_wash",
        "result": "missed", "basedOnEventIds": [wash],
        "regionResults": [{"region": "chest", "verdict": "good"}, {"region": "body_length", "verdict": "too_short"}]})).await?;
    s.save(json!({
        "kind": "rule", "statement": "Northline relaxed tees lose about 3 cm of length after a hot wash and tumble dry",
        "scope": {"brand": "Northline", "model": "Relaxed Pocket Tee", "region": "body_length"},
        "adjustment": {"region": "body_length", "direction": "shrinks", "magnitude": 3, "unit": "cm"},
        "basis": "known_brand_new_cut", "status": "proposed", "evidence": [wash],
    }))
    .await?;

    let overshirt = s
        .save(variant(
            "Fenwick",
            "Wool Overshirt",
            "outerwear/overshirt",
            "L",
            "wool",
        ))
        .await?;
    let pending = s
        .save(json!({"kind": "piece", "label": "charcoal overshirt", "category": "outerwear/overshirt",
            "variantId": overshirt, "initialStatus": "undecided", "provenance": stated()}))
        .await?;
    s.event(
        2,
        json!({"type": "acquired", "pieceId": pending, "statusAfter": "undecided",
        "order": {"retailer": "Fenwick", "orderRef": "FW-PREVIEW-2", "returnBy": s.ago(-5)}}),
    )
    .await?;

    let length = s
        .save(json!({"kind": "preference", "category": "tops", "aspect": "length", "region": "body_length",
            "statement": "hits at the hip bone", "strength": "prefer", "validFrom": s.ago(400), "provenance": stated()}))
        .await?;
    s.revise(&length, 1, json!({"kind": "preference", "category": "tops", "aspect": "length", "region": "body_length",
        "statement": "slightly cropped, just above the hip bone", "strength": "prefer", "validFrom": s.ago(30),
        "provenance": stated()}))
        .await?;
    s.save(json!({"kind": "preference", "category": "any", "aspect": "tactile", "statement": "no scratchy wool next to skin",
        "strength": "avoid", "validFrom": s.ago(200), "provenance": stated()}))
        .await?;
    s.save(json!({"kind": "need", "category": "tops/tshirt", "reason": "replacement", "priority": "high",
        "description": "replace the shrunken grey pocket tee", "relatedPieceIds": [pocket], "status": "open",
        "reviewAfter": s.ago(1), "provenance": stated()}))
        .await?;
    s.save(json!({"kind": "need", "category": "outerwear/jacket", "reason": "gap", "priority": "medium",
        "description": "a light rain jacket for spring", "status": "open",
        "constraints": {"budget": {"amount": 250, "currency": "CAD"}, "mustHave": ["hood"]}, "provenance": stated()}))
        .await?;
    s.stylist
        .export(&Namespace::real(), false, Reason::Backup)
        .await
        .map_err(|e| anyhow::anyhow!(e.tool_text()))?;
    Ok(())
}
