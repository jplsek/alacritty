use std::ffi::c_void;
use std::mem;

use crate::gl;
use crate::gl::types::*;
use crate::renderer::shader::{ShaderError, ShaderProgram, ShaderVersion};

const TAB_BAR_SHADER_F: &str = include_str!("../../res/tab_bar.f.glsl");
const TAB_BAR_SHADER_V: &str = include_str!("../../res/tab_bar.v.glsl");

#[repr(C)]
#[derive(Clone, Copy, Debug)]
struct Vertex {
    x: f32,
    y: f32,
    texture_x: f32,
    texture_y: f32,
}

#[derive(Debug)]
pub struct TabBarRenderer {
    program: ShaderProgram,
    texture_uniform: GLint,
    texture: GLuint,
    vao: GLuint,
    vbo: GLuint,
    texture_size: (u32, u32),
}

impl TabBarRenderer {
    pub fn new(shader_version: ShaderVersion) -> Result<Self, ShaderError> {
        let program = ShaderProgram::new(shader_version, None, TAB_BAR_SHADER_V, TAB_BAR_SHADER_F)?;
        let texture_uniform = program.get_uniform_location(c"tabTexture")?;

        let mut texture = 0;
        let mut vao = 0;
        let mut vbo = 0;

        unsafe {
            gl::GenTextures(1, &mut texture);
            gl::BindTexture(gl::TEXTURE_2D, texture);
            gl::TexParameteri(gl::TEXTURE_2D, gl::TEXTURE_MIN_FILTER, gl::LINEAR as GLint);
            gl::TexParameteri(gl::TEXTURE_2D, gl::TEXTURE_MAG_FILTER, gl::LINEAR as GLint);
            gl::TexParameteri(gl::TEXTURE_2D, gl::TEXTURE_WRAP_S, gl::CLAMP_TO_EDGE as GLint);
            gl::TexParameteri(gl::TEXTURE_2D, gl::TEXTURE_WRAP_T, gl::CLAMP_TO_EDGE as GLint);
            gl::BindTexture(gl::TEXTURE_2D, 0);

            gl::GenVertexArrays(1, &mut vao);
            gl::GenBuffers(1, &mut vbo);
            gl::BindVertexArray(vao);
            gl::BindBuffer(gl::ARRAY_BUFFER, vbo);

            gl::VertexAttribPointer(
                0,
                2,
                gl::FLOAT,
                gl::FALSE,
                mem::size_of::<Vertex>() as GLsizei,
                std::ptr::null(),
            );
            gl::EnableVertexAttribArray(0);
            gl::VertexAttribPointer(
                1,
                2,
                gl::FLOAT,
                gl::FALSE,
                mem::size_of::<Vertex>() as GLsizei,
                (2 * mem::size_of::<f32>()) as *const c_void,
            );
            gl::EnableVertexAttribArray(1);

            gl::BindBuffer(gl::ARRAY_BUFFER, 0);
            gl::BindVertexArray(0);
        }

        Ok(Self { program, texture_uniform, texture, vao, vbo, texture_size: (0, 0) })
    }

    #[allow(clippy::too_many_arguments)]
    pub fn draw_rgb(
        &mut self,
        pixels: &[slint::Rgb8Pixel],
        width: u32,
        height: u32,
        surface_width: u32,
        surface_height: u32,
        x: u32,
        y: u32,
        updated: bool,
    ) {
        self.draw(
            pixels,
            gl::RGB,
            false,
            width,
            height,
            surface_width,
            surface_height,
            x,
            y,
            updated,
        );
    }

    #[allow(clippy::too_many_arguments)]
    pub fn draw_rgba(
        &mut self,
        pixels: &[slint::platform::software_renderer::PremultipliedRgbaColor],
        width: u32,
        height: u32,
        surface_width: u32,
        surface_height: u32,
        x: u32,
        y: u32,
        updated: bool,
    ) {
        self.draw(
            pixels,
            gl::RGBA,
            true,
            width,
            height,
            surface_width,
            surface_height,
            x,
            y,
            updated,
        );
    }

    #[allow(clippy::too_many_arguments)]
    fn draw<T>(
        &mut self,
        pixels: &[T],
        pixel_format: GLenum,
        premultiplied_alpha: bool,
        width: u32,
        height: u32,
        surface_width: u32,
        surface_height: u32,
        x: u32,
        y: u32,
        updated: bool,
    ) {
        if pixels.is_empty()
            || width == 0
            || height == 0
            || surface_width == 0
            || surface_height == 0
        {
            return;
        }

        let left = 2. * x as f32 / surface_width as f32 - 1.;
        let right = 2. * (x + width) as f32 / surface_width as f32 - 1.;
        let top = 1. - 2. * y as f32 / surface_height as f32;
        let bottom = 1. - 2. * (y + height) as f32 / surface_height as f32;
        let vertices = [
            Vertex { x: left, y: top, texture_x: 0., texture_y: 0. },
            Vertex { x: left, y: bottom, texture_x: 0., texture_y: 1. },
            Vertex { x: right, y: top, texture_x: 1., texture_y: 0. },
            Vertex { x: right, y: top, texture_x: 1., texture_y: 0. },
            Vertex { x: left, y: bottom, texture_x: 0., texture_y: 1. },
            Vertex { x: right, y: bottom, texture_x: 1., texture_y: 1. },
        ];

        unsafe {
            // Alacritty's text renderers cache the currently bound glyph atlas. Preserve the
            // binding so their cache remains synchronized after compositing the Slint texture.
            let mut active_texture = 0;
            gl::GetIntegerv(gl::ACTIVE_TEXTURE, &mut active_texture);
            gl::ActiveTexture(gl::TEXTURE0);

            let mut texture = 0;
            gl::GetIntegerv(gl::TEXTURE_BINDING_2D, &mut texture);

            // The glyph atlas keeps this at 1 so narrow RGB glyph rows are uploaded without
            // padding. Restore the actual state rather than assuming OpenGL's default of 4.
            let mut unpack_alignment = 0;
            gl::GetIntegerv(gl::UNPACK_ALIGNMENT, &mut unpack_alignment);

            gl::Viewport(0, 0, surface_width as GLsizei, surface_height as GLsizei);
            if premultiplied_alpha {
                gl::BlendFunc(gl::ONE, gl::ONE_MINUS_SRC_ALPHA);
            } else {
                gl::BlendFunc(gl::SRC_ALPHA, gl::ONE_MINUS_SRC_ALPHA);
            }

            gl::BindTexture(gl::TEXTURE_2D, self.texture);
            if self.texture_size != (width, height) {
                gl::PixelStorei(gl::UNPACK_ALIGNMENT, 1);
                gl::TexImage2D(
                    gl::TEXTURE_2D,
                    0,
                    pixel_format as GLint,
                    width as GLsizei,
                    height as GLsizei,
                    0,
                    pixel_format,
                    gl::UNSIGNED_BYTE,
                    pixels.as_ptr().cast(),
                );
                gl::PixelStorei(gl::UNPACK_ALIGNMENT, unpack_alignment);
                self.texture_size = (width, height);
            } else if updated {
                gl::PixelStorei(gl::UNPACK_ALIGNMENT, 1);
                gl::TexSubImage2D(
                    gl::TEXTURE_2D,
                    0,
                    0,
                    0,
                    width as GLsizei,
                    height as GLsizei,
                    pixel_format,
                    gl::UNSIGNED_BYTE,
                    pixels.as_ptr().cast(),
                );
                gl::PixelStorei(gl::UNPACK_ALIGNMENT, unpack_alignment);
            }

            gl::UseProgram(self.program.id());
            gl::Uniform1i(self.texture_uniform, 0);
            gl::BindVertexArray(self.vao);
            gl::BindBuffer(gl::ARRAY_BUFFER, self.vbo);
            gl::BufferData(
                gl::ARRAY_BUFFER,
                mem::size_of_val(&vertices) as isize,
                vertices.as_ptr().cast(),
                gl::STREAM_DRAW,
            );
            gl::DrawArrays(gl::TRIANGLES, 0, vertices.len() as GLsizei);

            gl::BindBuffer(gl::ARRAY_BUFFER, 0);
            gl::BindVertexArray(0);
            gl::UseProgram(0);
            gl::BindTexture(gl::TEXTURE_2D, texture as GLuint);
            gl::ActiveTexture(active_texture as GLenum);
            gl::BlendFunc(gl::SRC1_COLOR, gl::ONE_MINUS_SRC1_COLOR);
        }
    }
}

impl Drop for TabBarRenderer {
    fn drop(&mut self) {
        unsafe {
            gl::DeleteBuffers(1, &self.vbo);
            gl::DeleteVertexArrays(1, &self.vao);
            gl::DeleteTextures(1, &self.texture);
        }
    }
}
