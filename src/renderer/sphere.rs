use wgpu::util::DeviceExt;

#[repr(C)]
#[derive(Copy, Clone, Debug, bytemuck::Pod, bytemuck::Zeroable)]
pub struct Vertex {
    pub position: [f32; 3],
    pub uv: [f32; 2],
}

impl Vertex {
    pub fn desc() -> wgpu::VertexBufferLayout<'static> {
        wgpu::VertexBufferLayout {
            array_stride: size_of::<Self>() as wgpu::BufferAddress,
            step_mode: wgpu::VertexStepMode::Vertex,
            attributes: &[
                wgpu::VertexAttribute {
                    offset: 0,
                    shader_location: 0,
                    format: wgpu::VertexFormat::Float32x3,
                },
                wgpu::VertexAttribute {
                    offset: 12,
                    shader_location: 1,
                    format: wgpu::VertexFormat::Float32x2,
                },
            ],
        }
    }
}

pub struct Sphere {
    pub vertex_buffer: wgpu::Buffer,
    pub index_buffer: wgpu::Buffer,
    pub index_count: u32,
}

impl Sphere {
    pub fn new(device: &wgpu::Device, sectors: u32, stacks: u32) -> Self {
        let radius = 1.0;
        let mut vertices = Vec::new();
        let mut indices = Vec::new();

        for i in 0..=stacks {
            let phi = std::f32::consts::PI * (i as f32 / stacks as f32);
            let (sin_phi, cos_phi) = phi.sin_cos();
            for j in 0..=sectors {
                let theta = 2.0 * std::f32::consts::PI * (j as f32 / sectors as f32);
                let (sin_theta, cos_theta) = theta.sin_cos();
                vertices.push(Vertex {
                    position: [
                        radius * sin_phi * sin_theta,
                        radius * cos_phi,
                        radius * sin_phi * cos_theta,
                    ],
                    uv: [j as f32 / sectors as f32, i as f32 / stacks as f32],
                });
            }
        }

        for i in 0..stacks {
            for j in 0..sectors {
                let first = i * (sectors + 1) + j;
                let second = first + sectors + 1;
                indices.extend_from_slice(&[
                    first,
                    second,
                    (first + 1),
                    second,
                    (second + 1),
                    (first + 1),
                ]);
            }
        }

        let vertex_buffer = device.create_buffer_init(&wgpu::util::BufferInitDescriptor {
            label: Some("Sphere VB"),
            contents: bytemuck::cast_slice(&vertices),
            usage: wgpu::BufferUsages::VERTEX,
        });
        let index_buffer = device.create_buffer_init(&wgpu::util::BufferInitDescriptor {
            label: Some("Sphere IB"),
            contents: bytemuck::cast_slice(&indices),
            usage: wgpu::BufferUsages::INDEX,
        });
        Self {
            vertex_buffer,
            index_buffer,
            index_count: indices.len() as u32,
        }
    }
}
