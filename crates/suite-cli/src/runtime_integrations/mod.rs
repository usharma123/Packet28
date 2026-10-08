use std::path::{Path, PathBuf};

use anyhow::Result;

use crate::agent_surface::AgentPromptFormat;
use crate::cmd_setup::McpConfigStatus;

pub(crate) mod antigravity;
pub(crate) mod claude;
pub(crate) mod cline;
pub(crate) mod codex;
pub(crate) mod copilot;
pub(crate) mod cursor;
pub(crate) mod gemini;
pub(crate) mod hermes;
pub(crate) mod kilocode;
pub(crate) mod opencode;
pub(crate) mod roo;
pub(crate) mod windsurf;

pub(crate) struct RuntimeEnvironment<'a> {
    root: PathBuf,
    home: PathBuf,
    command_exists: &'a dyn Fn(&str) -> bool,
    run_command: &'a dyn Fn(&str, &[String]) -> Result<bool>,
}

impl<'a> RuntimeEnvironment<'a> {
    #[cfg(test)]
    pub(crate) fn new(
        root: &'a Path,
        home: &'a Path,
        command_exists: &'a dyn Fn(&str) -> bool,
        run_command: &'a dyn Fn(&str, &[String]) -> Result<bool>,
    ) -> Self {
        Self {
            root: root.to_path_buf(),
            home: home.to_path_buf(),
            command_exists,
            run_command,
        }
    }

    pub(crate) fn root(&self) -> &Path {
        &self.root
    }

    pub(crate) fn home(&self) -> &Path {
        &self.home
    }

    pub(crate) fn command_exists(&self, name: &str) -> bool {
        (self.command_exists)(name)
    }

    pub(crate) fn run_command(&self, name: &str, args: &[String]) -> Result<bool> {
        (self.run_command)(name, args)
    }
}

impl RuntimeEnvironment<'static> {
    pub(crate) fn from_process(root: &Path) -> Self {
        Self {
            root: root.to_path_buf(),
            home: dirs_home(),
            command_exists: &which_exists,
            run_command: &run_command,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct PromptTarget {
    pub(crate) path: PathBuf,
    pub(crate) format: AgentPromptFormat,
}

type PromptTargetsFn = for<'a> fn(&RuntimeEnvironment<'a>) -> Vec<PromptTarget>;
type DetectFn = for<'a> fn(&RuntimeEnvironment<'a>) -> bool;
type ConfigureFn = for<'a> fn(&RuntimeEnvironment<'a>, bool) -> Result<McpConfigStatus>;
type ArtifactsFn = for<'a> fn(&RuntimeEnvironment<'a>) -> Vec<PathBuf>;
type StatusFn = for<'a> fn(&RuntimeEnvironment<'a>) -> String;

#[derive(Clone, Copy)]
pub(crate) struct IntegrationAction {
    configure: ConfigureFn,
    artifacts: ArtifactsFn,
    status: StatusFn,
}

impl IntegrationAction {
    pub(crate) const fn new(
        configure: ConfigureFn,
        artifacts: ArtifactsFn,
        status: StatusFn,
    ) -> Self {
        Self {
            configure,
            artifacts,
            status,
        }
    }

    pub(crate) fn configure(
        self,
        environment: &RuntimeEnvironment<'_>,
        auto_yes: bool,
    ) -> Result<McpConfigStatus> {
        (self.configure)(environment, auto_yes)
    }

    pub(crate) fn artifacts(self, environment: &RuntimeEnvironment<'_>) -> Vec<PathBuf> {
        (self.artifacts)(environment)
    }

    pub(crate) fn status(self, environment: &RuntimeEnvironment<'_>) -> String {
        debug_assert!(!self.artifacts(environment).is_empty());
        (self.status)(environment)
    }
}

pub(crate) struct RuntimeAdapter {
    pub(crate) name: &'static str,
    pub(crate) slug: &'static str,
    prompt_targets: PromptTargetsFn,
    detect: DetectFn,
    pub(crate) mcp: Option<IntegrationAction>,
    pub(crate) hooks: Option<IntegrationAction>,
    pub(crate) writes_hook_runtime_config: bool,
}

impl RuntimeAdapter {
    pub(crate) fn inspect(&'static self, environment: &RuntimeEnvironment<'_>) -> RuntimeInfo {
        RuntimeInfo {
            adapter: self,
            name: self.name,
            slug: self.slug,
            prompt_targets: (self.prompt_targets)(environment),
            detected: (self.detect)(environment),
        }
    }

    pub(crate) fn prompt_targets(&self, environment: &RuntimeEnvironment<'_>) -> Vec<PromptTarget> {
        (self.prompt_targets)(environment)
    }

    #[cfg(test)]
    pub(crate) fn detected(&self, environment: &RuntimeEnvironment<'_>) -> bool {
        (self.detect)(environment)
    }
}

pub(crate) struct RuntimeInfo {
    pub(crate) adapter: &'static RuntimeAdapter,
    pub(crate) name: &'static str,
    pub(crate) slug: &'static str,
    pub(crate) prompt_targets: Vec<PromptTarget>,
    pub(crate) detected: bool,
}

pub(crate) static RUNTIME_ADAPTERS: [&RuntimeAdapter; 12] = [
    &claude::ADAPTER,
    &cursor::ADAPTER,
    &codex::ADAPTER,
    &windsurf::ADAPTER,
    &copilot::ADAPTER,
    &gemini::ADAPTER,
    &opencode::ADAPTER,
    &hermes::ADAPTER,
    &cline::ADAPTER,
    &roo::ADAPTER,
    &kilocode::ADAPTER,
    &antigravity::ADAPTER,
];

pub(crate) fn adapters() -> &'static [&'static RuntimeAdapter] {
    &RUNTIME_ADAPTERS
}

#[cfg(test)]
pub(crate) fn adapter_for_slug(slug: &str) -> Option<&'static RuntimeAdapter> {
    adapters()
        .iter()
        .copied()
        .find(|adapter| adapter.slug == slug)
}

pub(crate) fn detect_runtimes(environment: &RuntimeEnvironment<'_>) -> Vec<RuntimeInfo> {
    adapters()
        .iter()
        .map(|adapter| adapter.inspect(environment))
        .collect()
}

pub(crate) fn dirs_home() -> PathBuf {
    std::env::var("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from("/tmp"))
}

pub(crate) fn which_exists(name: &str) -> bool {
    std::process::Command::new("which")
        .arg(name)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .map(|status| status.success())
        .unwrap_or(false)
}

pub(crate) fn run_command(name: &str, args: &[String]) -> Result<bool> {
    let status = std::process::Command::new(name)
        .args(args)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()?;
    Ok(status.success())
}

#[cfg(test)]
mod tests;
