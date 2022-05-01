use egui::{epaint::Primitive, Context};
use parking_lot::{Mutex, MutexGuard};
use std::mem::{size_of, size_of_val};
use windows::{
    core::HRESULT,
    Win32::{
        Foundation::{HWND, LPARAM, RECT, WPARAM},
        Graphics::{
            Direct3D::D3D11_PRIMITIVE_TOPOLOGY_TRIANGLELIST,
            Direct3D11::{
                ID3D11Device, ID3D11DeviceContext, ID3D11InputLayout, ID3D11RenderTargetView,
                ID3D11Texture2D, D3D11_BIND_VERTEX_BUFFER, D3D11_BUFFER_DESC,
                D3D11_INPUT_ELEMENT_DESC, D3D11_INPUT_PER_VERTEX_DATA, D3D11_SUBRESOURCE_DATA,
                D3D11_USAGE_DEFAULT, D3D11_VIEWPORT,
            },
            Dxgi::{Common::DXGI_FORMAT_R32G32B32_FLOAT, IDXGISwapChain},
        },
        UI::WindowsAndMessaging::GetClientRect,
    },
};

use crate::{
    input::{InputCollector, InputResult},
    mesh::{create_index_buffer, create_vertex_buffer},
    shader::CompiledShaders,
};

/// Heart and soul of this integration.
/// Main methods you are going to use are:
/// * [`Self::present`] - Should be called inside of hook or before present.
/// * [`Self::resize_buffers`] - Should be called **INSTEAD** of swapchain's `ResizeBuffers`.
/// * [`Self::wnd_proc`] - Should be called on each `WndProc`.
pub struct DirectX11App<T = ()> {
    render_view: Mutex<Option<ID3D11RenderTargetView>>,
    ui: Box<dyn FnMut(&Context, &mut T) + 'static>,
    input_layout: ID3D11InputLayout,
    input_collector: InputCollector,
    shaders: CompiledShaders,
    ctx: Mutex<Context>,
    state: Mutex<T>,
    hwnd: HWND,
}

impl<T> DirectX11App<T>
where
    T: Default,
{
    /// Creates new app with state set to default value.
    #[inline]
    pub fn new_with_default(
        ui: impl FnMut(&Context, &mut T) + 'static,
        swap_chain: &IDXGISwapChain,
    ) -> Self {
        Self::new_with_state(ui, swap_chain, T::default())
    }
}

impl<T> DirectX11App<T> {
    const INPUT_ELEMENTS_DESC: [D3D11_INPUT_ELEMENT_DESC; 1] = [
        D3D11_INPUT_ELEMENT_DESC {
            SemanticName: p_str!("POS"),
            SemanticIndex: 0,
            Format: DXGI_FORMAT_R32G32B32_FLOAT,
            InputSlot: 0,
            AlignedByteOffset: 0,
            InputSlotClass: D3D11_INPUT_PER_VERTEX_DATA,
            InstanceDataStepRate: 0,
        },
        // D3D11_INPUT_ELEMENT_DESC {
        // SemanticName: pc_str!("TEXCOORD"),
        // SemanticIndex: 0,
        // Format: DXGI_FORMAT_R32G32_FLOAT,
        // InputSlot: 0,
        // AlignedByteOffset: D3D11_APPEND_ALIGNED_ELEMENT,
        // InputSlotClass: D3D11_INPUT_PER_VERTEX_DATA,
        // InstanceDataStepRate: 0,
        // },
        // D3D11_INPUT_ELEMENT_DESC {
        // SemanticName: pc_str!("COLOR"),
        // SemanticIndex: 0,
        // Format: DXGI_FORMAT_R8G8B8A8_UINT,
        // InputSlot: 0,
        // AlignedByteOffset: D3D11_APPEND_ALIGNED_ELEMENT,
        // InputSlotClass: D3D11_INPUT_PER_VERTEX_DATA,
        // InstanceDataStepRate: 0,
        // },
    ];
}

impl<T> DirectX11App<T> {
    /// Returns lock to state of the app.
    pub fn state(&self) -> MutexGuard<T> {
        self.state.lock()
    }

    /// Creates new app with state initialized from closule call.
    #[inline]
    pub fn new_with(
        ui: impl FnMut(&Context, &mut T) + 'static,
        swap_chain: &IDXGISwapChain,
        state: impl FnOnce() -> T,
    ) -> Self {
        Self::new_with_state(ui, swap_chain, state())
    }

    /// Creates new app with explicit state value.
    pub fn new_with_state(
        ui: impl FnMut(&Context, &mut T) + 'static,
        swap_chain: &IDXGISwapChain,
        state: T,
    ) -> Self {
        unsafe {
            let hwnd =
                expect!(swap_chain.GetDesc(), "Failed to get swapchain's descriptor").OutputWindow;

            if hwnd.0 == -1 {
                if !cfg!(feature = "no-msgs") {
                    panic!("Invalid output window descriptor");
                } else {
                    unreachable!()
                }
            }

            let device: ID3D11Device =
                expect!(swap_chain.GetDevice(), "Failed to get swapchain's device");

            let backbuffer: ID3D11Texture2D = expect!(
                swap_chain.GetBuffer(0),
                "Failed to get swapchain's backbuffer"
            );

            let render_view = Mutex::new(Some(expect!(
                device.CreateRenderTargetView(backbuffer, 0 as _),
                "Failed to create render target view"
            )));

            let shaders = CompiledShaders::new(&device);
            let input_layout = expect!(
                device.CreateInputLayout(
                    Self::INPUT_ELEMENTS_DESC.as_ptr() as _,
                    Self::INPUT_ELEMENTS_DESC.len() as _,
                    shaders.bytecode_ptr() as _,
                    shaders.bytecode_len()
                ),
                "Failed to create input layout"
            );

            if cfg!(debug_assertions) {
                eprintln!("Initialization finished");
            }

            Self {
                input_collector: InputCollector::new(hwnd),
                ctx: Mutex::new(Context::default()),
                state: Mutex::new(state),
                ui: Box::new(ui),
                input_layout,
                render_view,
                shaders,
                hwnd,
            }
        }
    }

    /// Present call. Should be called once per original present call, before or inside of hook.
    pub fn present(&self, swap_chain: &IDXGISwapChain, _sync_interval: u32, _flags: u32) {
        const TRIANGLE: [f32; 9] = [0.0, 0.5, 0.0, 0.5, -0.5, 0.0, -0.5, -0.5, 0.0];

        unsafe {
            let (dev, context) = &get_device_and_context(swap_chain);

            let view_lock = &*self.render_view.lock();
            let state_lock = &mut *self.state.lock();
            let ctx_lock = &mut *self.ctx.lock();

            let output = ctx_lock.run(self.input_collector.collect_input(), |ctx| {
                // Dont look here, it should be fine until someone tries to do something horrible.
                (*(self.ui.as_ref() as *const _ as *mut dyn FnMut(&Context, &mut T)))(
                    ctx, state_lock,
                )
            });

            if !output.platform_output.copied_text.is_empty() {
                // @TODO: Paste text
            }

            let primitives = ctx_lock
                .tessellate(output.shapes)
                .into_iter()
                .filter_map(|prim| {
                    if let Primitive::Mesh(mesh) = prim.primitive {
                        Some((prim.clip_rect, mesh))
                    } else {
                        panic!("Paint callbacks are not yet supported")
                    }
                })
                .collect::<Vec<_>>();

            for (clip, mesh) in primitives {
                let index_buffer = create_index_buffer(dev, &mesh);
                let vertex_buffer = create_vertex_buffer(dev, &mesh);
            }

            let desc = D3D11_BUFFER_DESC {
                ByteWidth: size_of_val(&TRIANGLE) as _,
                Usage: D3D11_USAGE_DEFAULT,
                BindFlags: D3D11_BIND_VERTEX_BUFFER.0,
                CPUAccessFlags: 0,
                MiscFlags: 0,
                StructureByteStride: 0,
            };

            let data = D3D11_SUBRESOURCE_DATA {
                pSysMem: TRIANGLE.as_ptr() as _,
                SysMemPitch: (3 * size_of::<f32>()) as _,
                SysMemSlicePitch: 0,
            };

            let buf = expect!(dev.CreateBuffer(&desc, &data), "Failed to create buffer");
            if cfg!(feature = "clear") {
                context.ClearRenderTargetView(view_lock, [0.39, 0.58, 0.92, 1.].as_ptr());
            }
            context.RSSetViewports(1, &self.get_viewport() as _);
            context.OMSetRenderTargets(1, view_lock, None);

            context.IASetPrimitiveTopology(D3D11_PRIMITIVE_TOPOLOGY_TRIANGLELIST);
            context.IASetInputLayout(&self.input_layout);

            let strides = (3 * size_of::<f32>()) as u32;
            let offsets = 0u32;
            context.IASetVertexBuffers(0, 1, &Some(buf), &strides as _, &offsets);
            context.VSSetShader(&self.shaders.vertex, &None, 0);
            context.PSSetShader(&self.shaders.pixel, &None, 0);
            context.Draw(3, 0);
        }
    }

    /// Call when resizing buffers.
    /// Do not call the original function before it, instead call it inside of the `original` closure.
    /// # Behavior
    /// In `origin` closure make sure to call the original `ResizeBuffers`.
    pub fn resize_buffers(
        &self,
        swap_chain: &IDXGISwapChain,
        original: impl FnOnce() -> HRESULT,
    ) -> HRESULT {
        unsafe {
            let view_lock = &mut *self.render_view.lock();
            std::ptr::drop_in_place(view_lock);

            let result = original();

            let backbuffer: ID3D11Texture2D = expect!(
                swap_chain.GetBuffer(0),
                "Failed to get swapchain's backbuffer"
            );

            let device: ID3D11Device =
                expect!(swap_chain.GetDevice(), "Failed to get swapchain's device");

            let new_view = expect!(
                device.CreateRenderTargetView(backbuffer, 0 as _),
                "Failed to create render target view"
            );

            *view_lock = Some(new_view);
            result
        }
    }

    /// Call on each `WndProc` occurence.
    /// Returns `true` if message was recognized and dispatched by input handler,
    /// `false` otherwise.
    #[inline]
    pub fn wnd_proc(&self, umsg: u32, wparam: WPARAM, lparam: LPARAM) -> InputResult {
        self.input_collector.process(umsg, wparam.0, lparam.0)
    }
}

impl<T> DirectX11App<T> {
    #[inline]
    fn get_screen_size(&self) -> (f32, f32) {
        let mut rect = RECT::default();
        unsafe {
            GetClientRect(self.hwnd, &mut rect);
        }
        (
            (rect.right - rect.left) as f32,
            (rect.bottom - rect.top) as f32,
        )
    }

    #[inline]
    fn get_viewport(&self) -> D3D11_VIEWPORT {
        let (w, h) = self.get_screen_size();
        D3D11_VIEWPORT {
            TopLeftX: 0.,
            TopLeftY: 0.,
            Width: w,
            Height: h,
            MinDepth: 0.,
            MaxDepth: 1.,
        }
    }
}

unsafe fn get_device_and_context(swap: &IDXGISwapChain) -> (ID3D11Device, ID3D11DeviceContext) {
    let device: ID3D11Device = expect!(swap.GetDevice(), "Failed to get swapchain's device");
    let mut ctx = None;
    device.GetImmediateContext(&mut ctx);
    (
        device,
        expect!(ctx, "Failed to get device's immediate context"),
    )
}
