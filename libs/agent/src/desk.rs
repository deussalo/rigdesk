use serde_json::Value;

/// One tool the desktop offers the agent.
#[derive(Clone, Debug)]
pub struct ToolSpec {
    pub name: &'static str,
    pub description: &'static str,
    pub input_schema: Value,
    /// Reads work while the human holds input; actions do not.
    pub read_only: bool,
}

#[derive(Clone, Debug, PartialEq)]
pub enum Content {
    Text(String),
    Png(Vec<u8>),
}

/// What the host gives the agent: capture, the UI tree, input, a shell. Calls block; the server runs them off its reactor.
pub trait Desk: Send + Sync + 'static {
    fn tools(&self) -> Vec<ToolSpec>;
    fn call(&self, tool: &str, args: &Value) -> Result<Vec<Content>, String>;
}
