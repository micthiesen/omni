//! `amazon_search` and `amazon_product`.

pub mod defs;

use std::sync::Arc;

use omni_mcp_kit::{McpTool, ToolContext, ToolError, ToolMetaError, typed_tool};
use serde::Deserialize;

use crate::amazon::page::{Asin, MAX_RESULTS, SearchOptions, Sort, Store};
use crate::amazon::{AmazonError, AmazonService};

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct SearchInput {
    query: String,
    #[serde(default)]
    store: Store,
    #[serde(default)]
    sort: Sort,
    #[serde(default = "one")]
    page: u32,
    #[serde(default = "twenty")]
    limit: u32,
    #[serde(default)]
    include_sponsored: bool,
}

#[derive(Deserialize)]
struct ProductInput {
    asin: String,
    #[serde(default)]
    store: Store,
}

fn one() -> u32 {
    1
}

fn twenty() -> u32 {
    20
}

fn tool_error(error: &AmazonError) -> ToolError {
    ToolError::execute(error.to_string())
}

pub fn tools(amazon: Arc<AmazonService>) -> Result<Vec<McpTool>, ToolMetaError> {
    let for_search = amazon.clone();
    let search = typed_tool(
        &defs::AMAZON_SEARCH,
        move |input: SearchInput, _cx: ToolContext| {
            let amazon = for_search.clone();
            async move {
                let query = input.query.trim();
                if query.is_empty() {
                    return Err(ToolError::input("query: must not be blank"));
                }
                let options = SearchOptions {
                    store: input.store,
                    page: input.page,
                    limit: usize::try_from(input.limit)
                        .unwrap_or(MAX_RESULTS)
                        .min(MAX_RESULTS),
                    include_sponsored: input.include_sponsored,
                };
                amazon
                    .search(query, input.sort, options)
                    .await
                    .map_err(|e| tool_error(&e))
            }
        },
    )?;
    let product = typed_tool(
        &defs::AMAZON_PRODUCT,
        move |input: ProductInput, _cx: ToolContext| {
            let amazon = amazon.clone();
            async move {
                let asin = Asin::parse(&input.asin)
                    .ok_or_else(|| ToolError::input("asin: must be 10 letters or digits"))?;
                amazon
                    .product(&asin, input.store)
                    .await
                    .map_err(|e| tool_error(&e))
            }
        },
    )?;
    Ok(vec![search, product])
}
