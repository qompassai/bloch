use mcp_derive::ToolDefinitions;

#[derive(ToolDefinitions)]
enum Tools {
    #[tool(name = "missing", schema = serde_json::json!({}))]
    Missing,
}

fn main() {}
