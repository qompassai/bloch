use mcp_derive::{Refusal, ToolDefinitions};
use serde_json::{json, Value};

#[allow(dead_code)]
#[derive(ToolDefinitions)]
enum Tools {
    /// Read. Cross-repo search over indexed .ai/memory entries, lane-filtered server-side (nsfw hidden from general-only callers).
    #[tool(
        name = "memory_search",
        schema = json!({
            "type": "object",
            "properties": {
                "caller": {"description": "Caller name.", "type": "string"},
                "clearance": {
                    "description": "Caller clearance (default general).",
                    "enum": ["general", "restricted", "all", "ntranch"],
                    "type": "string"
                },
                "limit": {"maximum": 50, "minimum": 1, "type": "integer"},
                "query": {"description": "Search text (all tokens match).", "type": "string"},
                "repo": {"description": "Repo filter", "type": "string"},
                "state": {
                    "description": "State filter.",
                    "enum": ["inbox", "promoted", "archive"],
                    "type": "string"
                }
            },
            "required": ["query"],
            "additionalProperties": false
        }),
        annotations = json!({
            "destructiveHint": false, "idempotentHint": true,
            "openWorldHint": false, "readOnlyHint": true
        })
    )]
    Search,
}

#[derive(Refusal)]
enum Denied {
    #[refusal(code = "private_memory_refused")]
    Private(String),
    #[refusal(code = "lane_wall_refusal")]
    LaneWall(String),
    #[refusal(code = "promoter_not_gatekeeper")]
    Promoter(String),
}

#[test]
fn tool_matches_installed_bloch() {
    let mut actual = Tools::definitions();
    actual.sort_all_objects();
    assert_eq!(
        serde_json::to_string(&actual).unwrap(),
        format!("[{}]", include_str!("fixtures/tool.json"))
    );
}

#[test]
fn refusals_match_installed_bloch() {
    let private = String::from(Denied::Private(
        "Pax's private memory (~/MEMORY.md, ~/memory/) is never indexed or served".into(),
    ));
    assert_eq!(
        format!("bloch: error: Git operation failed: {private}\n"),
        include_str!("fixtures/private_memory_refused.stderr"),
    );
    let lane = String::from(Denied::LaneWall(
        "nsfw-labeled content may not be promoted into a general-lane repo".into(),
    ));
    let promoter = String::from(Denied::Promoter(
        "promotion is a Pax/Owner gate act, got 'worker'".into(),
    ));
    for (id, text, fixture) in [
        (
            2,
            format!("promote_failed: Git operation failed: {lane}"),
            include_str!("fixtures/lane_wall_refusal.json"),
        ),
        (
            3,
            promoter,
            include_str!("fixtures/promoter_not_gatekeeper.json"),
        ),
    ] {
        let mut actual = json!({
            "jsonrpc": "2.0", "id": id,
            "result": {"resultType": "complete", "content": [{"type": "text", "text": text}], "isError": true}
        });
        let mut expected: Value = serde_json::from_str(fixture).unwrap();
        actual.sort_all_objects();
        expected.sort_all_objects();
        assert_eq!(
            serde_json::to_vec(&actual).unwrap(),
            serde_json::to_vec(&expected).unwrap()
        );
    }
}

#[test]
fn invalid_declarations_are_compile_errors() {
    trybuild::TestCases::new().compile_fail("tests/ui/*.rs");
}
