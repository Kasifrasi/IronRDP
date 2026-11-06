use anyhow::Context as _;
use winit::application::ApplicationHandler;
use winit::event::WindowEvent;
use winit::event_loop::{ActiveEventLoop, EventLoop};
use winit::monitor::MonitorHandle;
use winit::window::WindowId;

use crate::monitor::MonitorLayout;

/// Local output selectors and optional preferred window dimensions.
#[derive(Clone)]
pub struct MonitorSelection {
    pub selectors: Vec<String>,
    pub primary: Option<String>,
    pub preferred_size: (Option<u16>, Option<u16>),
}

impl MonitorSelection {
    pub(crate) fn resolve(&self, event_loop: &ActiveEventLoop) -> anyhow::Result<(MonitorLayout, Vec<String>)> {
        let available: Vec<_> = event_loop.available_monitors().collect();
        let mut selected = Vec::new();
        for selector in &self.selectors {
            let index = resolve_selector(&available, selector)?;
            anyhow::ensure!(
                !selected.contains(&index),
                "output is selected more than once: {selector}"
            );
            selected.push(index);
        }
        let primary = self
            .primary
            .as_ref()
            .map(|selector| {
                let index = resolve_selector(&available, selector)?;
                selected
                    .iter()
                    .position(|selected| *selected == index)
                    .context("primary monitor is not among the selected outputs")
            })
            .transpose()?
            .unwrap_or(0);
        let sizes: Vec<_> = selected
            .iter()
            .map(|&index| {
                let size = available[index].size();
                (
                    self.preferred_size.0.map_or(size.width, u32::from),
                    self.preferred_size.1.map_or(size.height, u32::from),
                )
            })
            .collect();
        let names = selected
            .iter()
            .map(|&index| available[index].name().unwrap_or_else(|| format!("output {index}")))
            .collect();
        Ok((MonitorLayout::horizontal(&sizes, primary)?, names))
    }
}

fn resolve_selector(available: &[MonitorHandle], selector: &str) -> anyhow::Result<usize> {
    if let Ok(index) = selector.parse::<usize>() {
        anyhow::ensure!(
            index < available.len(),
            "unknown output ID {index}; use --list-monitors"
        );
        return Ok(index);
    }
    let matches: Vec<_> = available
        .iter()
        .enumerate()
        .filter(|(_, monitor)| monitor.name().as_deref() == Some(selector))
        .map(|(index, _)| index)
        .collect();
    anyhow::ensure!(!matches.is_empty(), "unknown output {selector:?}; use --list-monitors");
    anyhow::ensure!(
        matches.len() == 1,
        "ambiguous output name {selector:?}; use its ID from --list-monitors"
    );
    Ok(matches[0])
}

/// Lists local outputs without creating windows or connecting to a server.
pub fn list_monitors() -> anyhow::Result<()> {
    let event_loop = EventLoop::new()?;
    event_loop.run_app(&mut MonitorList)?;
    Ok(())
}

struct MonitorList;

impl ApplicationHandler for MonitorList {
    #[expect(
        clippy::print_stdout,
        reason = "--list-monitors reports local outputs to the terminal"
    )]
    fn resumed(&mut self, event_loop: &ActiveEventLoop) {
        println!("Local outputs (IDs apply to this enumeration; prefer names in saved commands):");
        for (index, monitor) in event_loop.available_monitors().enumerate() {
            let size = monitor.size();
            let name = monitor.name().unwrap_or_else(|| "(unnamed)".to_owned());
            println!(
                "{index}: {name:?} {}x{} scale {}",
                size.width,
                size.height,
                monitor.scale_factor()
            );
        }
        event_loop.exit();
    }

    fn window_event(&mut self, _event_loop: &ActiveEventLoop, _window_id: WindowId, _event: WindowEvent) {}
}
