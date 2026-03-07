use anyhow::Result;
use clap::Args;

#[derive(Args, Clone)]
pub struct AgentPromptArgs {
    /// Output fragment family
    #[arg(long, value_enum)]
    pub format: crate::agent_surface::AgentPromptFormat,

    /// Optional repo root hint to include in generated command examples
    #[arg(long, default_value = ".")]
    pub root: String,
}

pub fn run(args: AgentPromptArgs) -> Result<i32> {
    let root = match args.root.trim() {
        "" | "." => None,
        other => Some(other),
    };
    print!(
        "{}",
        crate::agent_surface::render_prompt_fragment(args.format, root)
    );
    Ok(0)
}
