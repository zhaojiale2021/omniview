use glam::Mat4;

pub struct OrbitCamera {
    pub yaw: f32,
    pub pitch: f32,
    pub fov: f32,
    /// Set by any mutating operation (drag/scroll/reset) and cleared after
    /// the uniform is re-uploaded, so the render loop skips the per-frame
    /// `write_buffer` when the camera has not moved.
    pub dirty: bool,
}

impl Default for OrbitCamera {
    fn default() -> Self {
        Self::new()
    }
}

impl OrbitCamera {
    pub fn new() -> Self {
        Self {
            yaw: 0.0,
            pitch: 0.0,
            fov: 90.0,
            dirty: true,
        }
    }

    pub fn view_proj_matrix(&self, aspect: f32) -> [[f32; 4]; 4] {
        let proj = Mat4::perspective_rh(self.fov.to_radians(), aspect, 0.1, 100.0);
        let view = Mat4::from_euler(
            glam::EulerRot::YXZ,
            self.yaw.to_radians(),
            self.pitch.to_radians(),
            0.0,
        );
        (proj * view).to_cols_array_2d()
    }

    /// Handle mouse drag. delta_x/y in pixels. One screen-width drag ≈ half a revolution.
    pub fn handle_mouse(&mut self, delta_x: f64, delta_y: f64, window_width: f64) {
        // Sensitivity: one full-window-width drag = ~180 degrees
        let scale = 180.0 / window_width.max(1.0);
        self.yaw += (delta_x as f32) * scale as f32;
        self.pitch += (delta_y as f32) * scale as f32;
        self.pitch = self.pitch.clamp(-89.0, 89.0);
        self.dirty = true;
    }

    pub fn handle_scroll(&mut self, delta: f32) {
        self.fov = (self.fov - delta * 2.0).clamp(30.0, 120.0);
        self.dirty = true;
    }

    pub fn reset(&mut self) {
        self.yaw = 0.0;
        self.pitch = 0.0;
        self.fov = 90.0;
        self.dirty = true;
    }
}
