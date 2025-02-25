use crate::{
    render_resource::{
        AsModuleDescriptorError, BindGroupLayout, BindGroupLayoutId, ComputePipeline,
        ComputePipelineDescriptor, ProcessShaderError, ProcessedShader,
        RawComputePipelineDescriptor, RawFragmentState, RawRenderPipelineDescriptor,
        RawVertexState, RenderPipeline, RenderPipelineDescriptor, Shader, ShaderImport,
        ShaderProcessor, ShaderReflectError,
    },
    renderer::RenderDevice,
    RenderWorld,
};
use bevy_asset::{AssetEvent, Assets, Handle};
use bevy_ecs::event::EventReader;
use bevy_ecs::system::{Res, ResMut};
use bevy_utils::{default, tracing::error, Entry, HashMap, HashSet};
use std::{hash::Hash, iter::FusedIterator, mem, ops::Deref, sync::Arc};
use thiserror::Error;
use wgpu::{
    BufferBindingType, PipelineLayoutDescriptor, ShaderModule,
    VertexBufferLayout as RawVertexBufferLayout,
};

enum PipelineDescriptor {
    RenderPipelineDescriptor(Box<RenderPipelineDescriptor>),
    ComputePipelineDescriptor(Box<ComputePipelineDescriptor>),
}

#[derive(Debug)]
pub enum Pipeline {
    RenderPipeline(RenderPipeline),
    ComputePipeline(ComputePipeline),
}

type CachedPipelineId = usize;

#[derive(Copy, Clone, Debug, Hash, Eq, PartialEq)]
pub struct CachedRenderPipelineId(CachedPipelineId);

impl CachedRenderPipelineId {
    pub const INVALID: Self = CachedRenderPipelineId(usize::MAX);
}

#[derive(Copy, Clone, Debug, Hash, Eq, PartialEq)]
pub struct CachedComputePipelineId(CachedPipelineId);

impl CachedComputePipelineId {
    pub const INVALID: Self = CachedComputePipelineId(usize::MAX);
}

struct CachedPipeline {
    descriptor: PipelineDescriptor,
    state: CachedPipelineState,
}

#[derive(Debug)]
pub enum CachedPipelineState {
    Queued,
    Ok(Pipeline),
    Err(PipelineCacheError),
}

impl CachedPipelineState {
    pub fn unwrap(&self) -> &Pipeline {
        match self {
            CachedPipelineState::Ok(pipeline) => pipeline,
            CachedPipelineState::Queued => {
                panic!("Pipeline has not been compiled yet. It is still in the 'Queued' state.")
            }
            CachedPipelineState::Err(err) => panic!("{}", err),
        }
    }
}

#[derive(Default)]
pub struct ShaderData {
    pipelines: HashSet<CachedPipelineId>,
    processed_shaders: HashMap<Vec<String>, Arc<ShaderModule>>,
    resolved_imports: HashMap<ShaderImport, Handle<Shader>>,
    dependents: HashSet<Handle<Shader>>,
}

#[derive(Default)]
struct ShaderCache {
    data: HashMap<Handle<Shader>, ShaderData>,
    shaders: HashMap<Handle<Shader>, Shader>,
    import_path_shaders: HashMap<ShaderImport, Handle<Shader>>,
    waiting_on_import: HashMap<ShaderImport, Vec<Handle<Shader>>>,
    processor: ShaderProcessor,
}

impl ShaderCache {
    fn get(
        &mut self,
        render_device: &RenderDevice,
        pipeline: CachedPipelineId,
        handle: &Handle<Shader>,
        shader_defs: &[String],
    ) -> Result<Arc<ShaderModule>, PipelineCacheError> {
        let shader = self
            .shaders
            .get(handle)
            .ok_or_else(|| PipelineCacheError::ShaderNotLoaded(handle.clone_weak()))?;
        let data = self.data.entry(handle.clone_weak()).or_default();
        let n_asset_imports = shader
            .imports()
            .filter(|import| matches!(import, ShaderImport::AssetPath(_)))
            .count();
        let n_resolved_asset_imports = data
            .resolved_imports
            .keys()
            .filter(|import| matches!(import, ShaderImport::AssetPath(_)))
            .count();
        if n_asset_imports != n_resolved_asset_imports {
            return Err(PipelineCacheError::ShaderImportNotYetAvailable);
        }

        data.pipelines.insert(pipeline);

        // PERF: this shader_defs clone isn't great. use raw_entry_mut when it stabilizes
        let module = match data.processed_shaders.entry(shader_defs.to_vec()) {
            Entry::Occupied(entry) => entry.into_mut(),
            Entry::Vacant(entry) => {
                let mut shader_defs = shader_defs.to_vec();
                #[cfg(feature = "webgl")]
                shader_defs.push(String::from("NO_ARRAY_TEXTURES_SUPPORT"));

                // TODO: 3 is the value from CLUSTERED_FORWARD_STORAGE_BUFFER_COUNT declared in bevy_pbr
                // consider exposing this in shaders in a more generally useful way, such as:
                // # if AVAILABLE_STORAGE_BUFFER_BINDINGS == 3
                // /* use storage buffers here */
                // # elif
                // /* use uniforms here */
                if !matches!(
                    render_device.get_supported_read_only_binding_type(3),
                    BufferBindingType::Storage { .. }
                ) {
                    shader_defs.push(String::from("NO_STORAGE_BUFFERS_SUPPORT"));
                }

                let processed = self.processor.process(
                    shader,
                    &shader_defs,
                    &self.shaders,
                    &self.import_path_shaders,
                )?;
                let module_descriptor = match processed
                    .get_module_descriptor(render_device.features())
                {
                    Ok(module_descriptor) => module_descriptor,
                    Err(err) => {
                        return Err(PipelineCacheError::AsModuleDescriptorError(err, processed));
                    }
                };

                render_device
                    .wgpu_device()
                    .push_error_scope(wgpu::ErrorFilter::Validation);
                let shader_module = render_device.create_shader_module(&module_descriptor);
                let error = render_device.wgpu_device().pop_error_scope();

                // `now_or_never` will return Some if the future is ready and None otherwise.
                // On native platforms, wgpu will yield the error immediatly while on wasm it may take longer since the browser APIs are asynchronous.
                // So to keep the complexity of the ShaderCache low, we will only catch this error early on native platforms,
                // and on wasm the error will be handled by wgpu and crash the application.
                if let Some(Some(wgpu::Error::Validation { description, .. })) =
                    bevy_utils::futures::now_or_never(error)
                {
                    return Err(PipelineCacheError::CreateShaderModule(description));
                }

                entry.insert(Arc::new(shader_module))
            }
        };

        Ok(module.clone())
    }

    fn clear(&mut self, handle: &Handle<Shader>) -> Vec<CachedPipelineId> {
        let mut shaders_to_clear = vec![handle.clone_weak()];
        let mut pipelines_to_queue = Vec::new();
        while let Some(handle) = shaders_to_clear.pop() {
            if let Some(data) = self.data.get_mut(&handle) {
                data.processed_shaders.clear();
                pipelines_to_queue.extend(data.pipelines.iter().cloned());
                shaders_to_clear.extend(data.dependents.iter().map(|h| h.clone_weak()));
            }
        }

        pipelines_to_queue
    }

    fn set_shader(&mut self, handle: &Handle<Shader>, shader: Shader) -> Vec<CachedPipelineId> {
        let pipelines_to_queue = self.clear(handle);
        if let Some(path) = shader.import_path() {
            self.import_path_shaders
                .insert(path.clone(), handle.clone_weak());
            if let Some(waiting_shaders) = self.waiting_on_import.get_mut(path) {
                for waiting_shader in waiting_shaders.drain(..) {
                    // resolve waiting shader import
                    let data = self.data.entry(waiting_shader.clone_weak()).or_default();
                    data.resolved_imports
                        .insert(path.clone(), handle.clone_weak());
                    // add waiting shader as dependent of this shader
                    let data = self.data.entry(handle.clone_weak()).or_default();
                    data.dependents.insert(waiting_shader.clone_weak());
                }
            }
        }

        for import in shader.imports() {
            if let Some(import_handle) = self.import_path_shaders.get(import) {
                // resolve import because it is currently available
                let data = self.data.entry(handle.clone_weak()).or_default();
                data.resolved_imports
                    .insert(import.clone(), import_handle.clone_weak());
                // add this shader as a dependent of the import
                let data = self.data.entry(import_handle.clone_weak()).or_default();
                data.dependents.insert(handle.clone_weak());
            } else {
                let waiting = self.waiting_on_import.entry(import.clone()).or_default();
                waiting.push(handle.clone_weak());
            }
        }

        self.shaders.insert(handle.clone_weak(), shader);
        pipelines_to_queue
    }

    fn remove(&mut self, handle: &Handle<Shader>) -> Vec<CachedPipelineId> {
        let pipelines_to_queue = self.clear(handle);
        if let Some(shader) = self.shaders.remove(handle) {
            if let Some(import_path) = shader.import_path() {
                self.import_path_shaders.remove(import_path);
            }
        }

        pipelines_to_queue
    }
}

#[derive(Default)]
struct LayoutCache {
    layouts: HashMap<Vec<BindGroupLayoutId>, wgpu::PipelineLayout>,
}

impl LayoutCache {
    fn get(
        &mut self,
        render_device: &RenderDevice,
        bind_group_layouts: &[BindGroupLayout],
    ) -> &wgpu::PipelineLayout {
        let key = bind_group_layouts.iter().map(|l| l.id()).collect();
        self.layouts.entry(key).or_insert_with(|| {
            let bind_group_layouts = bind_group_layouts
                .iter()
                .map(|l| l.value())
                .collect::<Vec<_>>();
            render_device.create_pipeline_layout(&PipelineLayoutDescriptor {
                bind_group_layouts: &bind_group_layouts,
                ..default()
            })
        })
    }
}

pub struct PipelineCache {
    layout_cache: LayoutCache,
    shader_cache: ShaderCache,
    device: RenderDevice,
    pipelines: Vec<CachedPipeline>,
    waiting_pipelines: HashSet<CachedPipelineId>,
}

impl PipelineCache {
    pub fn new(device: RenderDevice) -> Self {
        Self {
            device,
            layout_cache: default(),
            shader_cache: default(),
            waiting_pipelines: default(),
            pipelines: default(),
        }
    }

    #[inline]
    pub fn get_render_pipeline_state(&self, id: CachedRenderPipelineId) -> &CachedPipelineState {
        &self.pipelines[id.0].state
    }

    #[inline]
    pub fn get_compute_pipeline_state(&self, id: CachedComputePipelineId) -> &CachedPipelineState {
        &self.pipelines[id.0].state
    }

    #[inline]
    pub fn get_render_pipeline_descriptor(
        &self,
        id: CachedRenderPipelineId,
    ) -> &RenderPipelineDescriptor {
        match &self.pipelines[id.0].descriptor {
            PipelineDescriptor::RenderPipelineDescriptor(descriptor) => descriptor,
            PipelineDescriptor::ComputePipelineDescriptor(_) => unreachable!(),
        }
    }

    #[inline]
    pub fn get_compute_pipeline_descriptor(
        &self,
        id: CachedComputePipelineId,
    ) -> &ComputePipelineDescriptor {
        match &self.pipelines[id.0].descriptor {
            PipelineDescriptor::RenderPipelineDescriptor(_) => unreachable!(),
            PipelineDescriptor::ComputePipelineDescriptor(descriptor) => descriptor,
        }
    }

    #[inline]
    pub fn get_render_pipeline(&self, id: CachedRenderPipelineId) -> Option<&RenderPipeline> {
        if let CachedPipelineState::Ok(Pipeline::RenderPipeline(pipeline)) =
            &self.pipelines[id.0].state
        {
            Some(pipeline)
        } else {
            None
        }
    }

    #[inline]
    pub fn get_compute_pipeline(&self, id: CachedComputePipelineId) -> Option<&ComputePipeline> {
        if let CachedPipelineState::Ok(Pipeline::ComputePipeline(pipeline)) =
            &self.pipelines[id.0].state
        {
            Some(pipeline)
        } else {
            None
        }
    }

    pub fn queue_render_pipeline(
        &mut self,
        descriptor: RenderPipelineDescriptor,
    ) -> CachedRenderPipelineId {
        let id = CachedRenderPipelineId(self.pipelines.len());
        self.pipelines.push(CachedPipeline {
            descriptor: PipelineDescriptor::RenderPipelineDescriptor(Box::new(descriptor)),
            state: CachedPipelineState::Queued,
        });
        self.waiting_pipelines.insert(id.0);
        id
    }

    pub fn queue_compute_pipeline(
        &mut self,
        descriptor: ComputePipelineDescriptor,
    ) -> CachedComputePipelineId {
        let id = CachedComputePipelineId(self.pipelines.len());
        self.pipelines.push(CachedPipeline {
            descriptor: PipelineDescriptor::ComputePipelineDescriptor(Box::new(descriptor)),
            state: CachedPipelineState::Queued,
        });
        self.waiting_pipelines.insert(id.0);
        id
    }

    fn set_shader(&mut self, handle: &Handle<Shader>, shader: &Shader) {
        let pipelines_to_queue = self.shader_cache.set_shader(handle, shader.clone());
        for cached_pipeline in pipelines_to_queue {
            self.pipelines[cached_pipeline].state = CachedPipelineState::Queued;
            self.waiting_pipelines.insert(cached_pipeline);
        }
    }

    fn remove_shader(&mut self, shader: &Handle<Shader>) {
        let pipelines_to_queue = self.shader_cache.remove(shader);
        for cached_pipeline in pipelines_to_queue {
            self.pipelines[cached_pipeline].state = CachedPipelineState::Queued;
            self.waiting_pipelines.insert(cached_pipeline);
        }
    }

    fn process_render_pipeline(
        &mut self,
        id: CachedPipelineId,
        descriptor: &RenderPipelineDescriptor,
    ) -> CachedPipelineState {
        let vertex_module = match self.shader_cache.get(
            &self.device,
            id,
            &descriptor.vertex.shader,
            &descriptor.vertex.shader_defs,
        ) {
            Ok(module) => module,
            Err(err) => {
                return CachedPipelineState::Err(err);
            }
        };

        let fragment_data = if let Some(fragment) = &descriptor.fragment {
            let fragment_module = match self.shader_cache.get(
                &self.device,
                id,
                &fragment.shader,
                &fragment.shader_defs,
            ) {
                Ok(module) => module,
                Err(err) => {
                    return CachedPipelineState::Err(err);
                }
            };
            Some((
                fragment_module,
                fragment.entry_point.deref(),
                &fragment.targets,
            ))
        } else {
            None
        };

        let vertex_buffer_layouts = descriptor
            .vertex
            .buffers
            .iter()
            .map(|layout| RawVertexBufferLayout {
                array_stride: layout.array_stride,
                attributes: &layout.attributes,
                step_mode: layout.step_mode,
            })
            .collect::<Vec<_>>();

        let layout = if let Some(layout) = &descriptor.layout {
            Some(self.layout_cache.get(&self.device, layout))
        } else {
            None
        };

        let descriptor = RawRenderPipelineDescriptor {
            multiview: None,
            depth_stencil: descriptor.depth_stencil.clone(),
            label: descriptor.label.as_deref(),
            layout,
            multisample: descriptor.multisample,
            primitive: descriptor.primitive,
            vertex: RawVertexState {
                buffers: &vertex_buffer_layouts,
                entry_point: descriptor.vertex.entry_point.deref(),
                module: &vertex_module,
            },
            fragment: fragment_data
                .as_ref()
                .map(|(module, entry_point, targets)| RawFragmentState {
                    entry_point,
                    module,
                    targets,
                }),
        };

        let pipeline = self.device.create_render_pipeline(&descriptor);

        CachedPipelineState::Ok(Pipeline::RenderPipeline(pipeline))
    }

    fn process_compute_pipeline(
        &mut self,
        id: CachedPipelineId,
        descriptor: &ComputePipelineDescriptor,
    ) -> CachedPipelineState {
        let compute_module = match self.shader_cache.get(
            &self.device,
            id,
            &descriptor.shader,
            &descriptor.shader_defs,
        ) {
            Ok(module) => module,
            Err(err) => {
                return CachedPipelineState::Err(err);
            }
        };

        let layout = if let Some(layout) = &descriptor.layout {
            Some(self.layout_cache.get(&self.device, layout))
        } else {
            None
        };

        let descriptor = RawComputePipelineDescriptor {
            label: descriptor.label.as_deref(),
            layout,
            module: &compute_module,
            entry_point: descriptor.entry_point.as_ref(),
        };

        let pipeline = self.device.create_compute_pipeline(&descriptor);

        CachedPipelineState::Ok(Pipeline::ComputePipeline(pipeline))
    }

    pub fn process_queue(&mut self) {
        let waiting_pipelines = mem::take(&mut self.waiting_pipelines);
        let mut pipelines = mem::take(&mut self.pipelines);

        for id in waiting_pipelines {
            let pipeline = &mut pipelines[id];
            match &pipeline.state {
                CachedPipelineState::Ok(_) => continue,
                CachedPipelineState::Queued => {}
                CachedPipelineState::Err(err) => {
                    match err {
                        PipelineCacheError::ShaderNotLoaded(_)
                        | PipelineCacheError::ShaderImportNotYetAvailable => { /* retry */ }
                        // shader could not be processed ... retrying won't help
                        PipelineCacheError::ProcessShaderError(err) => {
                            error!("failed to process shader: {}", err);
                            continue;
                        }
                        PipelineCacheError::AsModuleDescriptorError(err, source) => {
                            log_shader_error(source, err);
                            continue;
                        }
                        PipelineCacheError::CreateShaderModule(description) => {
                            error!("failed to create shader module: {}", description);
                            continue;
                        }
                    }
                }
            }

            pipeline.state = match &pipeline.descriptor {
                PipelineDescriptor::RenderPipelineDescriptor(descriptor) => {
                    self.process_render_pipeline(id, descriptor)
                }
                PipelineDescriptor::ComputePipelineDescriptor(descriptor) => {
                    self.process_compute_pipeline(id, descriptor)
                }
            };

            if let CachedPipelineState::Err(_) = pipeline.state {
                self.waiting_pipelines.insert(id);
            }
        }

        self.pipelines = pipelines;
    }

    pub(crate) fn process_pipeline_queue_system(mut cache: ResMut<Self>) {
        cache.process_queue();
    }

    pub(crate) fn extract_shaders(
        mut world: ResMut<RenderWorld>,
        shaders: Res<Assets<Shader>>,
        mut events: EventReader<AssetEvent<Shader>>,
    ) {
        let mut cache = world.resource_mut::<Self>();
        for event in events.iter() {
            match event {
                AssetEvent::Created { handle } | AssetEvent::Modified { handle } => {
                    if let Some(shader) = shaders.get(handle) {
                        cache.set_shader(handle, shader);
                    }
                }
                AssetEvent::Removed { handle } => cache.remove_shader(handle),
            }
        }
    }
}

fn log_shader_error(source: &ProcessedShader, error: &AsModuleDescriptorError) {
    use codespan_reporting::{
        diagnostic::{Diagnostic, Label},
        files::SimpleFile,
        term,
    };

    match error {
        AsModuleDescriptorError::ShaderReflectError(error) => match error {
            ShaderReflectError::WgslParse(error) => {
                let source = source
                    .get_wgsl_source()
                    .expect("non-wgsl source for wgsl error");
                let msg = error.emit_to_string(source);
                error!("failed to process shader:\n{}", msg);
            }
            ShaderReflectError::GlslParse(errors) => {
                let source = source
                    .get_glsl_source()
                    .expect("non-glsl source for glsl error");
                let files = SimpleFile::new("glsl", source);
                let config = codespan_reporting::term::Config::default();
                let mut writer = term::termcolor::Ansi::new(Vec::new());

                for err in errors {
                    let mut diagnostic = Diagnostic::error().with_message(err.kind.to_string());

                    if let Some(range) = err.meta.to_range() {
                        diagnostic = diagnostic.with_labels(vec![Label::primary((), range)]);
                    }

                    term::emit(&mut writer, &config, &files, &diagnostic)
                        .expect("cannot write error");
                }

                let msg = writer.into_inner();
                let msg = String::from_utf8_lossy(&msg);

                error!("failed to process shader: \n{}", msg);
            }
            ShaderReflectError::SpirVParse(error) => {
                error!("failed to process shader:\n{}", error);
            }
            ShaderReflectError::Validation(error) => {
                let (filename, source) = match source {
                    ProcessedShader::Wgsl(source) => ("wgsl", source.as_ref()),
                    ProcessedShader::Glsl(source, _) => ("glsl", source.as_ref()),
                    ProcessedShader::SpirV(_) => {
                        error!("failed to process shader:\n{}", error);
                        return;
                    }
                };

                let files = SimpleFile::new(filename, source);
                let config = term::Config::default();
                let mut writer = term::termcolor::Ansi::new(Vec::new());

                let diagnostic = Diagnostic::error()
                    .with_message(error.to_string())
                    .with_labels(
                        error
                            .spans()
                            .map(|(span, desc)| {
                                Label::primary((), span.to_range().unwrap())
                                    .with_message(desc.to_owned())
                            })
                            .collect(),
                    )
                    .with_notes(
                        ErrorSources::of(error)
                            .map(|source| source.to_string())
                            .collect(),
                    );

                term::emit(&mut writer, &config, &files, &diagnostic).expect("cannot write error");

                let msg = writer.into_inner();
                let msg = String::from_utf8_lossy(&msg);

                error!("failed to process shader: \n{}", msg);
            }
        },
        AsModuleDescriptorError::WgslConversion(error) => {
            error!("failed to convert shader to wgsl: \n{}", error);
        }
        AsModuleDescriptorError::SpirVConversion(error) => {
            error!("failed to convert shader to spirv: \n{}", error);
        }
    }
}

#[derive(Error, Debug)]
pub enum PipelineCacheError {
    #[error(
        "Pipeline cound not be compiled because the following shader is not loaded yet: {0:?}"
    )]
    ShaderNotLoaded(Handle<Shader>),
    #[error(transparent)]
    ProcessShaderError(#[from] ProcessShaderError),
    #[error("{0}")]
    AsModuleDescriptorError(AsModuleDescriptorError, ProcessedShader),
    #[error("Shader import not yet available.")]
    ShaderImportNotYetAvailable,
    #[error("Could not create shader module: {0}")]
    CreateShaderModule(String),
}

struct ErrorSources<'a> {
    current: Option<&'a (dyn std::error::Error + 'static)>,
}

impl<'a> ErrorSources<'a> {
    fn of(error: &'a dyn std::error::Error) -> Self {
        Self {
            current: error.source(),
        }
    }
}

impl<'a> Iterator for ErrorSources<'a> {
    type Item = &'a (dyn std::error::Error + 'static);

    fn next(&mut self) -> Option<Self::Item> {
        let current = self.current;
        self.current = self.current.and_then(std::error::Error::source);
        current
    }
}

impl<'a> FusedIterator for ErrorSources<'a> {}
