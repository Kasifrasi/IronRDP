#![expect(
    unused_crate_dependencies,
    reason = "the example calls only the viewer's public GPU validation entry point"
)]

fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt().with_env_filter("info").init();
    ironrdp_viewer::gpu::verify_renderer()
}
