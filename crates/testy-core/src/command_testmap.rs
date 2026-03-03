use anyhow::Result;

#[derive(Debug, Clone)]
pub struct TestmapBuildArgs {
    pub manifest: Vec<String>,
    pub output: String,
    pub timings_output: String,
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct TestmapBuildSummary {
    pub manifest_files: usize,
    pub records: usize,
    pub tests: usize,
    pub files: usize,
    pub output_testmap_path: String,
    pub output_timings_path: String,
}

#[derive(Debug, Clone)]
pub struct TestmapBuildOutput {
    pub warnings: Vec<String>,
    pub summary: TestmapBuildSummary,
}

pub fn run_testmap_build(
    build: TestmapBuildArgs,
    adapters: &crate::pipeline_testmap::TestMapAdapters,
) -> Result<TestmapBuildOutput> {
    let response = crate::pipeline_testmap::run_testmap(
        crate::pipeline_testmap::TestMapRequest {
            manifest_globs: build.manifest,
            output_testmap_path: build.output,
            output_timings_path: build.timings_output,
        },
        adapters,
    )?;

    Ok(TestmapBuildOutput {
        warnings: response.warnings,
        summary: TestmapBuildSummary {
            manifest_files: response.stats.manifest_files,
            records: response.stats.records,
            tests: response.stats.tests,
            files: response.stats.files,
            output_testmap_path: response.output_testmap_path,
            output_timings_path: response.output_timings_path,
        },
    })
}
