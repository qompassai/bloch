# MCP derives v1

`ToolDefinitions` derives a JSON array from a fieldless enum. Each variant declares `name`, `schema` and optional `annotations` in `#[tool(...)]`. Its doc comments provide the description. One enum lets the compiler reject duplicate wire names across a surface and preserves tool order. Schema and annotation expressions return ordinary JSON values so callers can reuse exact existing schema builders.

Bloch will build its concrete SDK tool vector when the handler is constructed. Both discovery and listing use that handler and listing clones its already constructed surface. The derive generates declarations only and never instantiates a server or router.

`Refusal` derives `code()`, `message()` and `From<Enum> for String` for variants explicitly tagged `#[refusal(code = "...")]` with one String message field. Bloch's tool boundary already uses String for refusals and its SDK converts that String to a text block with `isError`. Call sites keep their current outer error conversions, especially the lane wall and startup error. Assay documents other fleet framing for Ache and Ardor. Those servers need their existing adapters to consume the code and message separately.

The installed tool entry was canonicalized with assay and contains:

```json
{"annotations":{"destructiveHint":false,"idempotentHint":true,"openWorldHint":false,"readOnlyHint":true},"description":"Read. Cross-repo search over indexed .ai/memory entries, lane-filtered server-side (nsfw hidden from general-only callers).","inputSchema":{"additionalProperties":false,"properties":{"caller":{"description":"Caller name.","type":"string"},"clearance":{"description":"Caller clearance (default general).","enum":["general","restricted","all","ntranch"],"type":"string"},"limit":{"maximum":50,"minimum":1,"type":"integer"},"query":{"description":"Search text (all tokens match).","type":"string"},"repo":{"description":"Repo filter","type":"string"},"state":{"description":"State filter.","enum":["inbox","promoted","archive"],"type":"string"}},"required":["query"],"type":"object"},"name":"memory_search"}
```

Installed refusal fixtures (startup stderr, then raw MCP replies):

```text
bloch: error: Git operation failed: private_memory_refused: Pax's private memory (~/MEMORY.md, ~/memory/) is never indexed or served
{"jsonrpc":"2.0","id":2,"result":{"resultType":"complete","content":[{"type":"text","text":"promote_failed: Git operation failed: lane_wall_refusal: nsfw-labeled content may not be promoted into a general-lane repo"}],"isError":true}}
{"jsonrpc":"2.0","id":3,"result":{"resultType":"complete","content":[{"type":"text","text":"promoter_not_gatekeeper: promotion is a Pax/Owner gate act, got 'worker'"}],"isError":true}}
```
