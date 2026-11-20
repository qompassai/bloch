use mcp_derive::Refusal;

#[derive(Refusal)]
enum Denied {
    Missing(String),
}

fn main() {}
