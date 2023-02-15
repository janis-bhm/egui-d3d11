use crate::{
    backup::BackupState,
    input::{InputCollector, InputResult},
    mesh::{create_index_buffer, create_vertex_buffer, GpuMesh, GpuVertex},
    shader::CompiledShaders,
    texture::TextureAllocator,
};
use clipboard::{windows_clipboard::WindowsClipboardContext, ClipboardProvider};
use egui::{epaint::Primitive, Context};
use once_cell::sync::OnceCell;
use std::{mem::size_of, ops::DerefMut};
use windows::{
    core::HRESULT,
    Win32::{
        Foundation::{HWND, LPARAM, RECT, WPARAM},
        Graphics::{
            Direct3D::D3D11_PRIMITIVE_TOPOLOGY_TRIANGLELIST,
            Direct3D11::{
                ID3D11BlendState, ID3D11Device, ID3D11DeviceContext, ID3D11InputLayout,
                ID3D11RasterizerState, ID3D11RenderTargetView, ID3D11SamplerState, ID3D11Texture2D,
                D3D11_APPEND_ALIGNED_ELEMENT, D3D11_BLEND_DESC, D3D11_BLEND_INV_SRC_ALPHA,
                D3D11_BLEND_ONE, D3D11_BLEND_OP_ADD, D3D11_BLEND_SRC_ALPHA,
                D3D11_COLOR_WRITE_ENABLE_ALL, D3D11_COMPARISON_ALWAYS, D3D11_CULL_NONE,
                D3D11_FILL_SOLID, D3D11_FILTER_MIN_MAG_MIP_LINEAR, D3D11_INPUT_ELEMENT_DESC,
                D3D11_INPUT_PER_VERTEX_DATA, D3D11_RASTERIZER_DESC, D3D11_RENDER_TARGET_BLEND_DESC,
                D3D11_SAMPLER_DESC, D3D11_TEXTURE_ADDRESS_BORDER, D3D11_VIEWPORT,
            },
            Dxgi::{
                Common::{
                    DXGI_FORMAT_R32G32B32A32_FLOAT, DXGI_FORMAT_R32G32_FLOAT, DXGI_FORMAT_R32_UINT,
                },
                IDXGISwapChain, DXGI_SWAP_CHAIN_DESC,
            },
        },
        UI::WindowsAndMessaging::GetClientRect,
    },
};

#[allow(clippy::type_complexity)]
struct AppData<T> {
    render_view: Option<ID3D11RenderTargetView>,
    ui: Box<dyn FnMut(&Context, &mut T) + 'static>,
    tex_alloc: TextureAllocator,
    input_layout: ID3D11InputLayout,
    input_collector: InputCollector,
    shaders: CompiledShaders,
    backup: BackupState,
    ctx: Context,
    state: T,
}

#[cfg(feature = "parking-lot")]
use parking_lot::{Mutex, MutexGuard};
#[cfg(feature = "spin-lock")]
use spin::lock_api::{Mutex, MutexGuard};

use lock_api::MappedMutexGuard;

/// Heart and soul of this integration.
/// Main methods you are going to use are:
/// * [`Self::present`] - Should be called inside of hook or before present.
/// * [`Self::resize_buffers`] - Should be called **INSTEAD** of swapchain's `ResizeBuffers`.
/// * [`Self::wnd_proc`] - Should be called on each `WndProc`.
pub struct DirectX11App<T = ()> {
    data: Mutex<Option<AppData<T>>>,
    hwnd: OnceCell<HWND>,
}

impl<T> DirectX11App<T> {
    const INPUT_ELEMENTS_DESC: [D3D11_INPUT_ELEMENT_DESC; 3] = [
        D3D11_INPUT_ELEMENT_DESC {
            SemanticName: pc_str!("POSITION"),
            SemanticIndex: 0,
            Format: DXGI_FORMAT_R32G32_FLOAT,
            InputSlot: 0,
            AlignedByteOffset: 0,
            InputSlotClass: D3D11_INPUT_PER_VERTEX_DATA,
            InstanceDataStepRate: 0,
        },
        D3D11_INPUT_ELEMENT_DESC {
            SemanticName: pc_str!("TEXCOORD"),
            SemanticIndex: 0,
            Format: DXGI_FORMAT_R32G32_FLOAT,
            InputSlot: 0,
            AlignedByteOffset: D3D11_APPEND_ALIGNED_ELEMENT,
            InputSlotClass: D3D11_INPUT_PER_VERTEX_DATA,
            InstanceDataStepRate: 0,
        },
        D3D11_INPUT_ELEMENT_DESC {
            SemanticName: pc_str!("COLOR"),
            SemanticIndex: 0,
            Format: DXGI_FORMAT_R32G32B32A32_FLOAT,
            InputSlot: 0,
            AlignedByteOffset: D3D11_APPEND_ALIGNED_ELEMENT,
            InputSlotClass: D3D11_INPUT_PER_VERTEX_DATA,
            InstanceDataStepRate: 0,
        },
    ];
}

impl<T> DirectX11App<T> {
    /// Creates new [`DirectX11App`] in const context. You are supposed to create a single static item to store the application state.
    pub const fn new() -> Self {
        Self {
            data: Mutex::new(None),
            hwnd: OnceCell::new(),
        }
    }

    /// Checks if the app is ready to draw and if it's safe to invoke `present`, `wndproc`, etc.
    /// `true` means that you have already called an `init_*` on the application.
    pub fn is_ready(&self) -> bool {
        self.hwnd.get().is_some()
    }

    /// Initializes application and state. You should call this only once!
    pub fn init_with_state_context(
        &self,
        swap: &IDXGISwapChain,
        ui: impl FnMut(&Context, &mut T) + 'static,
        state: T,
        context: Context,
    ) {
        unsafe {
            if self.hwnd.get().is_some() {
                panic_msg!("You must call init only once");
            }

            let mut swap_desc: DXGI_SWAP_CHAIN_DESC = Default::default();

            expect!(
                swap.GetDesc(&mut swap_desc),
                "Failed to get swapchain's descriptor"
            );

            let hwnd = swap_desc.OutputWindow;
            if hwnd.0 == -1 {
                panic_msg!("Invalid output window descriptor");
            }
            let _ = self.hwnd.set(hwnd);

            let dev: ID3D11Device = expect!(swap.GetDevice(), "Failed to get swapchain's device");

            let backbuffer: ID3D11Texture2D =
                expect!(swap.GetBuffer(0), "Failed to get swapchain's backbuffer");

            let mut render_view: Option<ID3D11RenderTargetView> = None;

            expect!(
                dev.CreateRenderTargetView(&backbuffer, None, Some(&mut render_view)),
                "Failed to create new render target view"
            );

            let shaders = CompiledShaders::new(&dev);

            let mut input_layout: Option<ID3D11InputLayout> = None;

            expect!(
                dev.CreateInputLayout(
                    &Self::INPUT_ELEMENTS_DESC,
                    shaders.bytecode(),
                    Some(&mut input_layout)
                ),
                "Failed to create input layout"
            );

            // this can only happen if the expect above fails
            let input_layout = expect!(input_layout, "Failed to create input layout");

            *self.data.lock() = Some(AppData {
                input_collector: InputCollector::new(hwnd),
                tex_alloc: TextureAllocator::default(),
                backup: BackupState::default(),
                ui: Box::new(ui),
                ctx: context,
                input_layout,
                render_view,
                shaders,
                state,
            });
        }
    }

    /// Initializes application and state. Sets egui's context to default value. You should call this only once!
    #[inline]
    pub fn init_with_state(
        &self,
        swap: &IDXGISwapChain,
        ui: impl FnMut(&Context, &mut T) + 'static,
        state: T,
    ) {
        self.init_with_state_context(swap, ui, state, Context::default())
    }

    /// Initializes application and state while allowing you to mutate the initial state of the egui's context. You should call this only once!
    #[inline]
    pub fn init_with_mutate(
        &self,
        swap: &IDXGISwapChain,
        ui: impl FnMut(&Context, &mut T) + 'static,
        mut state: T,
        mutate: impl FnOnce(&mut Context, &mut T),
    ) {
        let mut ctx = Context::default();
        mutate(&mut ctx, &mut state);

        self.init_with_state_context(swap, ui, state, ctx);
    }

    #[cfg(feature = "parking-lot")]
    pub fn lock_state(&self) -> MappedMutexGuard<'_, parking_lot::RawMutex, T> {
        MutexGuard::map(self.data.lock(), |app| &mut app.as_mut().unwrap().state)
    }

    #[cfg(feature = "spin-lock")]
    pub fn lock_state(&self) -> MappedMutexGuard<'_, spin::mutex::Mutex<()>, T> {
        MutexGuard::map(self.data.lock(), |app| &mut app.as_mut().unwrap().state)
    }

    fn lock_data(&self) -> impl DerefMut<Target = AppData<T>> + '_ {
        MutexGuard::map(self.data.lock(), |app| {
            expect!(app.as_mut(), "You need to call init first")
        })
    }
}

impl<T: Default> DirectX11App<T> {
    /// Initializes application and sets the state to its default value. You should call this only once!
    #[inline]
    pub fn init_default(&self, swap: &IDXGISwapChain, ui: impl FnMut(&Context, &mut T) + 'static) {
        self.init_with_state_context(swap, ui, T::default(), Context::default());
    }
}

impl<T> DirectX11App<T> {
    /// Present call. Should be called once per original present call, before or inside of hook.
    #[allow(clippy::cast_ref_to_mut)]
    pub fn present(&self, swap_chain: &IDXGISwapChain) {
        unsafe {
            let this = &mut *self.lock_data();

            let (dev, ctx) = &get_device_and_context(swap_chain);

            this.backup.save(ctx);

            let screen = self.get_screen_size();

            if cfg!(feature = "clear") {
                // Use let_chains here once stabilized, didn't wanna add a nightly feature to the crate
                if let Some(render_view) = &this.render_view {
                    ctx.ClearRenderTargetView(render_view, [0.39, 0.58, 0.92, 1.].as_ptr());
                }
            }

            let output = this.ctx.run(this.input_collector.collect_input(), |ctx| {
                // Dont look here, it should be fine until someone tries to do something horrible.
                (this.ui)(ctx, &mut this.state);
            });

            if !output.textures_delta.is_empty() {
                this.tex_alloc
                    .process_deltas(dev, ctx, output.textures_delta);
            }

            if !output.platform_output.copied_text.is_empty() {
                let _ = WindowsClipboardContext.set_contents(output.platform_output.copied_text);
            }

            if output.shapes.is_empty() {
                this.backup.restore(ctx);
                return;
            }

            let primitives = this
                .ctx
                .tessellate(output.shapes)
                .into_iter()
                .filter_map(|prim| {
                    if let Primitive::Mesh(mesh) = prim.primitive {
                        GpuMesh::from_mesh(screen, mesh, prim.clip_rect)
                    } else {
                        panic!("Paint callbacks are not yet supported")
                    }
                })
                .collect::<Vec<_>>();

            self.set_blend_state(dev, ctx);
            self.set_raster_options(dev, ctx);
            self.set_sampler_state(dev, ctx);

            ctx.RSSetViewports(Some(&[self.get_viewport()]));
            ctx.OMSetRenderTargets(
                Some(&[expect!(
                    this.render_view.clone(),
                    "Failed to set render targets"
                )]),
                None,
            );
            ctx.IASetPrimitiveTopology(D3D11_PRIMITIVE_TOPOLOGY_TRIANGLELIST);
            ctx.IASetInputLayout(&this.input_layout);

            for mesh in primitives {
                let idx = create_index_buffer(dev, &mesh);
                let vtx = create_vertex_buffer(dev, &mesh);

                let texture = this.tex_alloc.get_by_id(mesh.texture_id);

                ctx.RSSetScissorRects(Some(&[RECT {
                    left: mesh.clip.left() as _,
                    top: mesh.clip.top() as _,
                    right: mesh.clip.right() as _,
                    bottom: mesh.clip.bottom() as _,
                }]));

                if let Some(texture) = texture {
                    ctx.PSSetShaderResources(0, Some(&[texture]));
                }

                ctx.IASetVertexBuffers(
                    0,
                    1,
                    Some(&Some(vtx)),
                    Some(&(size_of::<GpuVertex>() as _)),
                    Some(&0),
                );
                ctx.IASetIndexBuffer(&idx, DXGI_FORMAT_R32_UINT, 0);
                ctx.VSSetShader(&this.shaders.vertex, None);
                ctx.PSSetShader(&this.shaders.pixel, None);

                ctx.DrawIndexed(mesh.indices.len() as _, 0, 0);
            }

            this.backup.restore(ctx);
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
            let this = &mut *self.lock_data();
            drop(this.render_view.take());

            let result = original();

            let backbuffer: ID3D11Texture2D = expect!(
                swap_chain.GetBuffer(0),
                "Failed to get swapchain's backbuffer"
            );

            let device: ID3D11Device =
                expect!(swap_chain.GetDevice(), "Failed to get swapchain's device");

            let mut new_view: Option<ID3D11RenderTargetView> = None;

            expect!(
                device.CreateRenderTargetView(&backbuffer, None, Some(&mut new_view)),
                "Failed to create render target view"
            );

            this.render_view = new_view;
            result
        }
    }

    /// Call on each `WndProc` occurence.
    /// Returns `true` if message was recognized and dispatched by input handler,
    /// `false` otherwise.
    #[inline]
    pub fn wnd_proc(&self, umsg: u32, wparam: WPARAM, lparam: LPARAM) -> InputResult {
        self.lock_data()
            .input_collector
            .process(umsg, wparam.0, lparam.0)
    }
}

impl<T> DirectX11App<T> {
    #[inline]
    fn get_screen_size(&self) -> (f32, f32) {
        let mut rect = RECT::default();
        unsafe {
            GetClientRect(
                *expect!(self.hwnd.get(), "You need to call init first"),
                &mut rect,
            );
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

    fn set_blend_state(&self, dev: &ID3D11Device, ctx: &ID3D11DeviceContext) {
        let mut targets: [D3D11_RENDER_TARGET_BLEND_DESC; 8] = Default::default();
        targets[0].BlendEnable = true.into();
        targets[0].SrcBlend = D3D11_BLEND_SRC_ALPHA;
        targets[0].DestBlend = D3D11_BLEND_INV_SRC_ALPHA;
        targets[0].BlendOp = D3D11_BLEND_OP_ADD;
        targets[0].SrcBlendAlpha = D3D11_BLEND_ONE;
        targets[0].DestBlendAlpha = D3D11_BLEND_INV_SRC_ALPHA;
        targets[0].BlendOpAlpha = D3D11_BLEND_OP_ADD;
        targets[0].RenderTargetWriteMask = D3D11_COLOR_WRITE_ENABLE_ALL.0 as _;

        let blend_desc = D3D11_BLEND_DESC {
            AlphaToCoverageEnable: false.into(),
            IndependentBlendEnable: false.into(),
            RenderTarget: targets,
        };

        unsafe {
            let mut blend_state: Option<ID3D11BlendState> = None;

            expect!(
                dev.CreateBlendState(&blend_desc, Some(&mut blend_state)),
                "Failed to create blend state"
            );

            let blend_state = expect!(blend_state, "Failed to create blend state");

            ctx.OMSetBlendState(
                &blend_state,
                Some([0f32, 0f32, 0f32, 0f32].as_ptr()),
                0xffffffff,
            );
        }
    }

    fn set_raster_options(&self, dev: &ID3D11Device, ctx: &ID3D11DeviceContext) {
        let raster_desc = D3D11_RASTERIZER_DESC {
            FillMode: D3D11_FILL_SOLID,
            CullMode: D3D11_CULL_NONE,
            FrontCounterClockwise: false.into(),
            DepthBias: false.into(),
            DepthBiasClamp: 0.,
            SlopeScaledDepthBias: 0.,
            DepthClipEnable: false.into(),
            ScissorEnable: true.into(),
            MultisampleEnable: false.into(),
            AntialiasedLineEnable: false.into(),
        };

        unsafe {
            let mut options: Option<ID3D11RasterizerState> = None;

            expect!(
                dev.CreateRasterizerState(&raster_desc, Some(&mut options)),
                "Failed to create rasterizer state"
            );
            if let Some(options) = options {
                ctx.RSSetState(&options);
            }
        }
    }

    fn set_sampler_state(&self, dev: &ID3D11Device, ctx: &ID3D11DeviceContext) {
        let desc = D3D11_SAMPLER_DESC {
            Filter: D3D11_FILTER_MIN_MAG_MIP_LINEAR,
            AddressU: D3D11_TEXTURE_ADDRESS_BORDER,
            AddressV: D3D11_TEXTURE_ADDRESS_BORDER,
            AddressW: D3D11_TEXTURE_ADDRESS_BORDER,
            MipLODBias: 0.,
            ComparisonFunc: D3D11_COMPARISON_ALWAYS,
            MinLOD: 0.,
            MaxLOD: 0.,
            BorderColor: [1., 1., 1., 1.],
            ..Default::default()
        };

        unsafe {
            let mut sampler: Option<ID3D11SamplerState> = None;

            expect!(
                dev.CreateSamplerState(&desc, Some(&mut sampler)),
                "Failed to create sampler"
            );

            if let Some(sampler) = sampler {
                ctx.PSSetSamplers(0, Some(&[sampler]));
            }
        }
    }
}

unsafe fn get_device_and_context(swap: &IDXGISwapChain) -> (ID3D11Device, ID3D11DeviceContext) {
    let device: ID3D11Device = expect!(swap.GetDevice(), "Failed to get swapchain's device");
    let ctx = device.GetImmediateContext();

    (
        device,
        expect!(ctx, "Failed to get device's immediate context"),
    )
}
