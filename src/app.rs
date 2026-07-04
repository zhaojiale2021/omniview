use std::sync::Arc;
use winit::{
    application::ApplicationHandler,
    event::{DeviceEvent, ElementState, MouseButton, MouseScrollDelta, WindowEvent},
    event_loop::ActiveEventLoop,
    window::{Window, WindowId},
};
use crate::renderer::Renderer;

pub struct App {
    pub window: Option<Arc<Window>>,
    pub renderer: Option<Renderer>,
    pub dragging: bool,
}

impl App {
    pub fn new() -> Self {
        Self { window: None, renderer: None, dragging: false }
    }
}

impl ApplicationHandler for App {
    fn resumed(&mut self, event_loop: &ActiveEventLoop) {
        let window_attrs = Window::default_attributes()
            .with_title("360° Video Player")
            .with_inner_size(winit::dpi::LogicalSize::new(1280.0, 720.0));
        let window = Arc::new(event_loop.create_window(window_attrs).unwrap());
        let renderer = pollster::block_on(Renderer::new(window.clone()));
        self.window = Some(window);
        self.renderer = Some(renderer);
    }

    fn window_event(&mut self, event_loop: &ActiveEventLoop, _id: WindowId, event: WindowEvent) {
        match event {
            WindowEvent::CloseRequested => event_loop.exit(),
            WindowEvent::Resized(s) => {
                if let Some(r) = &mut self.renderer {
                    r.resize(s.width, s.height);
                    r.update_camera_uniform();
                }
            }
            WindowEvent::MouseInput {
                state: ElementState::Pressed,
                button: MouseButton::Left,
                ..
            } => {
                self.dragging = true;
            }
            WindowEvent::MouseInput {
                state: ElementState::Released,
                button: MouseButton::Left,
                ..
            } => {
                self.dragging = false;
            }
            WindowEvent::MouseWheel { delta, .. } => {
                if let Some(r) = &mut self.renderer {
                    let scroll = match delta {
                        MouseScrollDelta::LineDelta(_, y) => y,
                        MouseScrollDelta::PixelDelta(p) => p.y as f32 / 10.0,
                    };
                    r.camera.handle_scroll(scroll);
                    r.update_camera_uniform();
                }
            }
            _ => {}
        }
    }

    fn device_event(&mut self, _el: &ActiveEventLoop, _id: winit::event::DeviceId, event: DeviceEvent) {
        if let DeviceEvent::MouseMotion { delta } = event {
            if self.dragging {
                if let Some(r) = &mut self.renderer {
                    r.camera.handle_mouse(delta.0, delta.1, r.size.1 as f64);
                    r.update_camera_uniform();
                }
            }
        }
    }

    fn about_to_wait(&mut self, _event_loop: &ActiveEventLoop) {
        let renderer = match &mut self.renderer { Some(r) => r, None => return };
        if let Err(e) = renderer.render() {
            match e {
                wgpu::SurfaceError::Lost | wgpu::SurfaceError::Outdated => {
                    let s = renderer.size;
                    renderer.resize(s.0, s.1);
                }
                wgpu::SurfaceError::OutOfMemory => tracing::error!("OOM"),
                wgpu::SurfaceError::Timeout => {}
            }
        }
    }
}
