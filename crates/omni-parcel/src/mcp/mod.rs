//! `parcels_add`: submit one delivery to Parcel (`crate::add`).

pub mod defs;

use std::sync::Arc;

use omni_mcp_kit::{McpTool, ToolContext, ToolError, ToolMetaError, typed_tool};
use serde::Deserialize;

use crate::add::{AddError, AddRequest, ParcelAdder};

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct AddInput {
    tracking_number: String,
    carrier: String,
    #[serde(default)]
    description: Option<String>,
    #[serde(default)]
    send_push_confirmation: bool,
}

/// Builds `parcels_add`; it answers `not_configured` without `PARCEL_API_KEY`.
pub fn tools(adder: Arc<ParcelAdder>) -> Result<Vec<McpTool>, ToolMetaError> {
    let add_tool = typed_tool(
        &defs::PARCELS_ADD,
        move |input: AddInput, _cx: ToolContext| {
            let adder = adder.clone();
            async move {
                adder
                    .add(AddRequest {
                        tracking_number: input.tracking_number,
                        carrier: input.carrier,
                        description: input.description,
                        send_push_confirmation: input.send_push_confirmation,
                    })
                    .await
                    .map_err(|error| match error {
                        AddError::Input(error) => ToolError::input(error.to_string()),
                        AddError::Store(error) => ToolError::execute_from(&error),
                    })
            }
        },
    )?;
    Ok(vec![add_tool])
}
