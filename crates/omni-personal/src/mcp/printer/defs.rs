//! The printer tools' contracts: metadata, policy and the schema types
//! their input and output schemas derive from (`omni_mcp_kit::schema`). These
//! types describe the wire format only; the handlers decode and encode with
//! their own types. After changing anything here run `cargo xtask mcp-golden`
//! and review the snapshot diff.

use omni_mcp_kit::schema::{Lit, Literal, positive};
use omni_mcp_kit::{Annotations, ApprovalPolicy, Policy, ToolDef, ToolDefinition, ToolInfo};
use schemars::JsonSchema;

pub static GET_PRINTER_STATUS: ToolDef<GetPrinterStatusInput, GetPrinterStatusOutput> =
    ToolDef::new(ToolInfo {
        name: "get_printer_status",
        title: "Get Monochrome Printer Status",
        description: "Read readiness, queue depth, toner level, and capabilities from Michael's fixed Brother HL-L2370DW monochrome laser printer. This does not print anything.",
        annotations: Annotations {
            read_only_hint: true,
            destructive_hint: false,
            idempotent_hint: true,
            open_world_hint: true,
        },
        policy: Policy {
            side_effects: &["Reads status from the fixed LAN printer"],
            cost: "none",
            recommended: ApprovalPolicy::Allow,
        },
    });

pub static PRINT_DOCUMENT: ToolDef<PrintDocumentInput, PrintDocumentOutput> = ToolDef::new(
    ToolInfo {
        name: "print_document",
        title: "Print PDF in Black and White",
        description: "Print a public HTTPS PDF on Michael's Brother HL-L2370DW monochrome laser printer. Black-and-white only. Uses two-sided long-edge printing and Letter paper by default. Every call requires approval because it consumes paper and toner and physically exposes the document.",
        annotations: Annotations {
            read_only_hint: false,
            destructive_hint: false,
            idempotent_hint: false,
            open_world_hint: true,
        },
        policy: Policy {
            side_effects: &[
                "Downloads a public PDF",
                "Sends a physical print job to the fixed Brother printer",
                "Physically exposes the printed document",
            ],
            cost: "Consumes paper, toner, and electricity; no paid API",
            recommended: ApprovalPolicy::RequireApproval,
        },
    },
);

/// Every printer tool, in serving order.
pub static TOOLS: [&dyn ToolDefinition; 2] = [&GET_PRINTER_STATUS, &PRINT_DOCUMENT];

#[derive(JsonSchema)]
#[schemars(deny_unknown_fields)]
pub struct GetPrinterStatusInput {}

#[derive(JsonSchema)]
#[schemars(rename_all = "camelCase", deny_unknown_fields)]
pub struct GetPrinterStatusOutput {
    #[schemars(transform = Literal(true))]
    pub configured: Lit,
    pub name: Option<String>,
    pub uri: String,
    pub state: String,
    pub state_reasons: Vec<String>,
    pub ready: bool,
    pub accepting_jobs: Option<bool>,
    pub queued_job_count: Option<u64>,
    #[schemars(range(min = 0, max = 100))]
    pub toner_percent: Option<f64>,
    #[schemars(transform = Literal(true))]
    pub monochrome_only: Lit,
    #[schemars(transform = Literal("two-sided-long-edge"))]
    pub default_sides: Lit,
    pub supported_formats: Vec<String>,
    pub supported_media: Vec<String>,
}

#[derive(JsonSchema)]
#[schemars(rename_all = "lowercase")]
pub enum Paper {
    Letter,
    A4,
    Legal,
}

#[derive(JsonSchema)]
#[schemars(rename_all = "kebab-case")]
pub enum Sides {
    OneSided,
    TwoSidedLongEdge,
    TwoSidedShortEdge,
}

#[derive(JsonSchema)]
#[schemars(rename_all = "camelCase", deny_unknown_fields)]
pub struct PrintDocumentInput {
    #[schemars(description = "Public HTTPS PDF URL", length(max = 2048), url)]
    pub url: String,
    #[schemars(length(min = 1, max = 80))]
    pub job_name: Option<String>,
    #[schemars(range(min = 1, max = 3), extend("default" = 1))]
    pub copies: Option<u64>,
    #[schemars(extend("default" = "letter"))]
    pub paper: Option<Paper>,
    #[schemars(extend("default" = "two-sided-long-edge"))]
    pub sides: Option<Sides>,
    #[schemars(description = "Allow the same PDF and settings to print again within 5 minutes", extend("default" = false))]
    pub allow_duplicate: Option<bool>,
}

#[derive(JsonSchema)]
#[schemars(rename_all = "camelCase", deny_unknown_fields)]
pub struct PrintDocumentOutput {
    #[schemars(transform = Literal(true))]
    pub accepted: Lit,
    pub completed: bool,
    pub job_id: Option<i64>,
    pub job_uri: String,
    pub job_state: String,
    pub job_name: String,
    #[schemars(transform = positive)]
    pub pages: u64,
    #[schemars(range(min = 1, max = 3))]
    pub copies: u64,
    pub paper: Paper,
    pub sides: Sides,
    pub impressions_completed: Option<u64>,
    pub message: String,
}
