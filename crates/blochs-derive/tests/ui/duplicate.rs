use mcp_derive::ToolDefinitions;

#[derive(ToolDefinitions)]
enum Tools {
    /// First definition.
    #[tool(name = "same", schema = serde_json::json!({}))]
    First,
    /// Second definition.
    #[tool(name = "same", schema = serde_json::json!({}))]
    Second,
}

fn main() {}
