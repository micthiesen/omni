//! Recipe MCP tools: thin adapters over [`Recipes`] under the gateway's
//! bearer authentication. Contracts are in [`defs`].

pub mod defs;

use std::future::Future;
use std::sync::Arc;

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;
use futures::future::BoxFuture;
use omni_mcp_kit::{
    Content, McpTool, SchemaValidator, ToolContext, ToolDef, ToolError, ToolHandler, ToolMetaError,
    ToolOutput, raw_tool, typed_tool,
};
use schemars::JsonSchema;
use serde::Serialize;
use serde::de::DeserializeOwned;
use serde_json::{Map, Value, json};

use crate::db::Actor;
use crate::error::RecipesError;
use crate::service::Recipes;

fn execute(error: RecipesError) -> ToolError {
    ToolError::execute(error.tool_text())
}

fn tool<I, O, F, Fut>(
    def: &'static ToolDef<I, O>,
    recipes: &Arc<Recipes>,
    f: F,
) -> Result<McpTool, ToolMetaError>
where
    I: JsonSchema + DeserializeOwned + Send + 'static,
    O: JsonSchema + Serialize + Send + 'static,
    F: Fn(Arc<Recipes>, I, Actor) -> Fut + Send + Sync + 'static,
    Fut: Future<Output = Result<O, RecipesError>> + Send + 'static,
{
    use omni_mcp_kit::ToolDefinition as _;
    let recipes = recipes.clone();
    let name = def.info().name;
    def.meta()?;
    typed_tool(def, move |input: I, cx: ToolContext| {
        let fut = f(recipes.clone(), input, Actor::new(name, cx.call_id));
        async move { fut.await.map_err(execute) }
    })
}

/// A tool whose result carries JPEG image blocks after its text.
struct ImageTool<I, F> {
    input: SchemaValidator,
    output: SchemaValidator,
    run: F,
    _input: std::marker::PhantomData<fn(I)>,
}

impl<I, O, F, Fut> ToolHandler for ImageTool<I, F>
where
    I: DeserializeOwned + Send + 'static,
    O: Serialize + Send + 'static,
    F: Fn(I) -> Fut + Send + Sync + 'static,
    Fut: Future<Output = Result<(O, Vec<Vec<u8>>), RecipesError>> + Send + 'static,
{
    fn call<'a>(
        &'a self,
        input: Value,
        _cx: ToolContext,
    ) -> BoxFuture<'a, Result<ToolOutput, ToolError>> {
        Box::pin(async move {
            let input = omni_core::js::normalize_numbers(input);
            self.input.check(&input).map_err(ToolError::input)?;
            let decoded: I =
                serde_json::from_value(input).map_err(|e| ToolError::input(e.to_string()))?;
            let (output, images) = (self.run)(decoded).await.map_err(execute)?;
            let value =
                serde_json::to_value(output).map_err(|e| ToolError::output(e.to_string()))?;
            let Value::Object(structured) = omni_core::js::normalize_numbers(value) else {
                return Err(ToolError::output("tool output must be a JSON object"));
            };
            self.output
                .check(&Value::Object(structured.clone()))
                .map_err(ToolError::output)?;
            let mut text: Content = Map::new();
            text.insert("type".into(), json!("text"));
            text.insert(
                "text".into(),
                json!(omni_core::js::json_stringify(&Value::Object(
                    structured.clone()
                ))),
            );
            let mut content = vec![text];
            for jpeg in images {
                let mut image: Content = Map::new();
                image.insert("type".into(), json!("image"));
                image.insert("data".into(), json!(STANDARD.encode(jpeg)));
                image.insert("mimeType".into(), json!("image/jpeg"));
                content.push(image);
            }
            Ok(ToolOutput::Custom {
                structured,
                content,
            })
        })
    }
}

fn image_tool<I, O, F, Fut>(def: &'static ToolDef<I, O>, run: F) -> Result<McpTool, ToolMetaError>
where
    I: JsonSchema + DeserializeOwned + Send + 'static,
    O: JsonSchema + Serialize + Send + 'static,
    F: Fn(I) -> Fut + Send + Sync + 'static,
    Fut: Future<Output = Result<(O, Vec<Vec<u8>>), RecipesError>> + Send + 'static,
{
    use omni_mcp_kit::ToolDefinition as _;
    let meta = def.meta()?;
    raw_tool(
        def,
        Arc::new(ImageTool {
            input: SchemaValidator::new(&meta.name, &meta.input_schema)?,
            output: SchemaValidator::new(&meta.name, &meta.output_schema)?,
            run,
            _input: std::marker::PhantomData,
        }),
    )
}

/// Every recipe tool over `recipes`.
pub fn tools(recipes: Arc<Recipes>) -> Result<Vec<McpTool>, ToolMetaError> {
    let (image_recipes, inbox_recipes) = (recipes.clone(), recipes.clone());
    Ok(vec![
        tool(&defs::RECIPES_GUIDE, &recipes, |r, _input, _| async move {
            r.guide().await
        })?,
        tool(&defs::RECIPES_SEARCH, &recipes, |r, input, _| async move {
            r.search(input).await
        })?,
        tool(&defs::RECIPES_GET, &recipes, |r, input, _| async move {
            r.get(input).await
        })?,
        tool(&defs::RECIPES_HISTORY, &recipes, |r, input, _| async move {
            r.history(input).await
        })?,
        image_tool(&defs::RECIPES_IMAGE_GET, move |input| {
            let r = image_recipes.clone();
            async move {
                let (output, jpeg) = r.image_get(input).await?;
                Ok((output, vec![jpeg]))
            }
        })?,
        tool(
            &defs::RECIPES_CREATE,
            &recipes,
            |r, input, actor| async move { r.create(input, actor).await },
        )?,
        tool(
            &defs::RECIPES_EDIT,
            &recipes,
            |r, input, actor| async move { r.edit(input, actor).await },
        )?,
        tool(
            &defs::RECIPES_REPLACE,
            &recipes,
            |r, input, actor| async move { r.replace(input, actor).await },
        )?,
        tool(
            &defs::RECIPES_REVERT,
            &recipes,
            |r, input, actor| async move { r.revert(input, actor).await },
        )?,
        tool(
            &defs::RECIPES_ORGANIZE,
            &recipes,
            |r, input, actor| async move { r.organize(input, actor).await },
        )?,
        tool(
            &defs::RECIPES_TAGS_EDIT,
            &recipes,
            |r, input, actor| async move { r.tags_edit(input, actor).await },
        )?,
        tool(
            &defs::RECIPES_MERGE,
            &recipes,
            |r, input, actor| async move { r.merge(input, actor).await },
        )?,
        tool(
            &defs::RECIPES_UNDO,
            &recipes,
            |r, input, actor| async move { r.undo(input, actor).await },
        )?,
        tool(
            &defs::RECIPES_IMAGES_EDIT,
            &recipes,
            |r, input, actor| async move { r.images_edit(input, actor).await },
        )?,
        tool(
            &defs::RECIPES_IMPORT_URL,
            &recipes,
            |r, input, _| async move { r.import_url(input).await },
        )?,
        tool(
            &defs::RECIPES_INBOX_LIST,
            &recipes,
            |r, input, _| async move { r.inbox_list(input).await },
        )?,
        image_tool(&defs::RECIPES_INBOX_GET, move |input| {
            let r = inbox_recipes.clone();
            async move { r.inbox_get(input).await }
        })?,
        tool(
            &defs::RECIPES_INBOX_RESOLVE,
            &recipes,
            |r, input, _| async move { r.inbox_resolve(input).await },
        )?,
        tool(
            &defs::RECIPES_COOK_START,
            &recipes,
            |r, input, _| async move { r.cook_start(input).await },
        )?,
        tool(
            &defs::RECIPES_COOK_GET,
            &recipes,
            |r, input, _| async move { r.cook_get(input).await },
        )?,
        tool(
            &defs::RECIPES_COOK_UPDATE,
            &recipes,
            |r, input, actor| async move { r.cook_update(input, actor).await },
        )?,
        tool(
            &defs::RECIPES_REQUESTS_LIST,
            &recipes,
            |r, input, _| async move { r.requests_list(input).await },
        )?,
        tool(
            &defs::RECIPES_REQUEST_UPDATE,
            &recipes,
            |r, input, _| async move { r.request_update(input).await },
        )?,
        tool(
            &defs::RECIPES_PINS,
            &recipes,
            |r, input, actor| async move { r.pins(input, actor).await },
        )?,
    ])
}
