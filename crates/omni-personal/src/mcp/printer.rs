//! `get_printer_status` and `print_document`.

pub mod defs;

use std::sync::Arc;

use omni_mcp_kit::{McpTool, ToolContext, ToolError, ToolMetaError, typed_tool};
use serde::Deserialize;

use crate::printer::{PrintPaper, PrintPdfInput, PrintSides, PrinterService};

#[derive(Deserialize)]
struct EmptyInput {}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct PrintDocumentInput {
    url: Option<String>,
    pdf_base64: Option<String>,
    job_name: Option<String>,
    #[serde(default = "one")]
    copies: f64,
    #[serde(default = "letter")]
    paper: PrintPaper,
    #[serde(default = "long_edge")]
    sides: PrintSides,
    #[serde(default)]
    allow_duplicate: bool,
}

fn one() -> f64 {
    1.0
}

fn letter() -> PrintPaper {
    PrintPaper::Letter
}

fn long_edge() -> PrintSides {
    PrintSides::TwoSidedLongEdge
}

fn require(printer: Option<&Arc<PrinterService>>) -> Result<&Arc<PrinterService>, ToolError> {
    printer.ok_or_else(|| ToolError::execute("Printer is not configured"))
}

/// Builds the printer tools; calls fail when no printer is configured.
pub fn tools(printer: Option<Arc<PrinterService>>) -> Result<Vec<McpTool>, ToolMetaError> {
    let for_status = printer.clone();
    let status = typed_tool(
        &defs::GET_PRINTER_STATUS,
        move |_input: EmptyInput, _cx: ToolContext| {
            let printer = for_status.clone();
            async move {
                let printer = require(printer.as_ref())?;
                let status = printer
                    .status()
                    .await
                    .map_err(|e| ToolError::execute(e.message))?;
                Ok::<_, ToolError>(status)
            }
        },
    )?;
    let print = typed_tool(
        &defs::PRINT_DOCUMENT,
        move |input: PrintDocumentInput, _cx: ToolContext| {
            let printer = printer.clone();
            async move {
                // `jobName` is trimmed and must stay non-empty.
                let job_name = match input.job_name {
                    Some(name) => {
                        let trimmed = omni_core::js::trim(&name);
                        if trimmed.is_empty() {
                            return Err(ToolError::input(
                                "jobName: String must contain at least 1 character(s)",
                            ));
                        }
                        Some(trimmed.to_owned())
                    }
                    None => None,
                };
                let printer = require(printer.as_ref())?;
                let request = PrintPdfInput {
                    url: input.url,
                    pdf_base64: input.pdf_base64,
                    paper: Some(input.paper),
                    sides: Some(input.sides),
                    copies: Some(input.copies),
                    job_name,
                    allow_duplicate: input.allow_duplicate,
                };
                let job = printer
                    .print_pdf(&request)
                    .await
                    .map_err(|e| ToolError::execute(e.message))?;
                Ok::<_, ToolError>(job)
            }
        },
    )?;
    Ok(vec![status, print])
}
