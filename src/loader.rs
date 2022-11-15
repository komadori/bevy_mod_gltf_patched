use anyhow::Result;
use bevy::asset::{
    AssetIoError, AssetLoader, AssetPath, BoxedFuture, Handle, LoadContext, LoadedAsset,
};
use bevy::core::Name;
use bevy::core_pipeline::prelude::Camera3d;
use bevy::ecs::{entity::Entity, world::World};
use bevy::hierarchy::{BuildWorldChildren, WorldChildBuilder};
use bevy::log::warn;
use bevy::math::{Mat4, Vec3};
use bevy::pbr::{
    AlphaMode, DirectionalLight, DirectionalLightBundle, PbrBundle, PointLight, PointLightBundle,
    SpotLight, SpotLightBundle, StandardMaterial,
};
use bevy::render::{
    camera::{
        Camera, CameraRenderGraph, OrthographicProjection, PerspectiveProjection, Projection,
        ScalingMode,
    },
    color::Color,
    mesh::{
        skinning::{SkinnedMesh, SkinnedMeshInverseBindposes},
        Indices, Mesh, MeshVertexAttribute, VertexAttributeValues,
    },
    prelude::SpatialBundle,
    primitives::{Aabb, Frustum},
    render_resource::{
        AddressMode, Face, FilterMode, PrimitiveTopology, SamplerDescriptor, VertexFormat,
    },
    texture::{CompressedImageFormats, Image, ImageSampler, ImageType, TextureError},
    view::VisibleEntities,
};
use bevy::scene::Scene;
#[cfg(not(target_arch = "wasm32"))]
use bevy::tasks::IoTaskPool;
use bevy::transform::components::Transform;

use bevy::utils::{HashMap, HashSet};
use gltf::{
    accessor::{DataType, Dimensions},
    mesh::{
        util::{ReadColors, ReadIndices, ReadJoints, ReadTexCoords},
        Mode,
    },
    texture::{MagFilter, MinFilter, WrappingMode},
    Material, Node, Primitive,
};
use std::{collections::VecDeque, path::Path};
use thiserror::Error;

use crate::{Gltf, GltfNode};

/// An error that occurs when loading a glTF file.
#[derive(Error, Debug)]
pub enum GltfError {
    #[error("unsupported primitive mode")]
    UnsupportedPrimitive { mode: Mode },
    #[error("invalid glTF file: {0}")]
    Gltf(#[from] gltf::Error),
    #[error("binary blob is missing")]
    MissingBlob,
    #[error("failed to decode base64 mesh data")]
    Base64Decode(#[from] base64::DecodeError),
    #[error("unsupported buffer format")]
    BufferFormatUnsupported,
    #[error("invalid image mime type: {0}")]
    InvalidImageMimeType(String),
    #[error("You may need to add the feature for the file format: {0}")]
    ImageError(#[from] TextureError),
    #[error("failed to load an asset path: {0}")]
    AssetIoError(#[from] AssetIoError),
    #[error("Missing sampler for animation {0}")]
    MissingAnimationSampler(usize),
    #[error("failed to generate tangents: {0}")]
    GenerateTangentsError(#[from] bevy::render::mesh::GenerateTangentsError),
}

/// Loads glTF files with all of their data as their corresponding bevy representations.
pub struct GltfLoader {
    pub(crate) supported_compressed_formats: CompressedImageFormats,
    pub(crate) custom_vertex_attributes: HashMap<String, MeshVertexAttribute>,
}

impl AssetLoader for GltfLoader {
    fn load<'a>(
        &'a self,
        bytes: &'a [u8],
        load_context: &'a mut LoadContext,
    ) -> BoxedFuture<'a, Result<()>> {
        Box::pin(async move { Ok(load_gltf(bytes, load_context, self).await?) })
    }

    fn extensions(&self) -> &[&str] {
        &["gltf", "glb"]
    }
}

/// Represents whether integer data requires normalization
#[derive(Copy, Clone)]
struct Normalization(bool);

impl Normalization {
    fn apply_either<T, U>(
        self,
        value: T,
        normalized_ctor: impl Fn(T) -> U,
        unnormalized_ctor: impl Fn(T) -> U,
    ) -> U {
        if self.0 {
            normalized_ctor(value)
        } else {
            unnormalized_ctor(value)
        }
    }
}

/// An error that occurs when accessing buffer data
enum AccessFailed {
    MalformedData,
    UnsupportedFormat,
}

/// Helper for reading buffer data
struct BufferAccessor<'a> {
    accessor: gltf::Accessor<'a>,
    buffer_data: &'a Vec<Vec<u8>>,
    normalization: Normalization,
}

impl<'a> BufferAccessor<'a> {
    /// Creates an iterator over the elements in this accessor
    fn iter<T: gltf::accessor::Item>(self) -> Result<gltf::accessor::Iter<'a, T>, AccessFailed> {
        gltf::accessor::Iter::new(self.accessor, |buffer: gltf::Buffer| {
            self.buffer_data.get(buffer.index()).map(|v| v.as_slice())
        })
        .ok_or(AccessFailed::MalformedData)
    }

    /// Applies the element iterator to a constructor or fails if normalization is required
    fn with_no_norm<T: gltf::accessor::Item, U>(
        self,
        ctor: impl Fn(gltf::accessor::Iter<'a, T>) -> U,
    ) -> Result<U, AccessFailed> {
        if self.normalization.0 {
            return Err(AccessFailed::UnsupportedFormat);
        }
        self.iter().map(ctor)
    }

    /// Applies the element iterator and the normalization flag to a constructor
    fn with_norm<T: gltf::accessor::Item, U>(
        self,
        ctor: impl Fn(gltf::accessor::Iter<'a, T>, Normalization) -> U,
    ) -> Result<U, AccessFailed> {
        let normalized = self.normalization;
        self.iter().map(|v| ctor(v, normalized))
    }
}

/// An enum of the iterators user by different vertex attribute formats
enum VertexAttributeIter<'a> {
    // For reading native WGPU formats
    F32(gltf::accessor::Iter<'a, f32>),
    U32(gltf::accessor::Iter<'a, u32>),
    F32x2(gltf::accessor::Iter<'a, [f32; 2]>),
    U32x2(gltf::accessor::Iter<'a, [u32; 2]>),
    F32x3(gltf::accessor::Iter<'a, [f32; 3]>),
    U32x3(gltf::accessor::Iter<'a, [u32; 3]>),
    F32x4(gltf::accessor::Iter<'a, [f32; 4]>),
    U32x4(gltf::accessor::Iter<'a, [u32; 4]>),
    S16x2(gltf::accessor::Iter<'a, [i16; 2]>, Normalization),
    U16x2(gltf::accessor::Iter<'a, [u16; 2]>, Normalization),
    S16x4(gltf::accessor::Iter<'a, [i16; 4]>, Normalization),
    U16x4(gltf::accessor::Iter<'a, [u16; 4]>, Normalization),
    S8x2(gltf::accessor::Iter<'a, [i8; 2]>, Normalization),
    U8x2(gltf::accessor::Iter<'a, [u8; 2]>, Normalization),
    S8x4(gltf::accessor::Iter<'a, [i8; 4]>, Normalization),
    U8x4(gltf::accessor::Iter<'a, [u8; 4]>, Normalization),
    // Additional on-disk formats used for RGB colors
    U16x3(gltf::accessor::Iter<'a, [u16; 3]>, Normalization),
    U8x3(gltf::accessor::Iter<'a, [u8; 3]>, Normalization),
}

impl<'a> VertexAttributeIter<'a> {
    /// Creates an iterator over the elements in a vertex attribute accessor
    fn from_accessor(
        accessor: gltf::Accessor<'a>,
        buffer_data: &'a Vec<Vec<u8>>,
    ) -> Result<VertexAttributeIter<'a>, AccessFailed> {
        let normalization = Normalization(accessor.normalized());
        let format = (accessor.data_type(), accessor.dimensions());
        let acc = BufferAccessor {
            accessor,
            buffer_data,
            normalization,
        };
        match format {
            (DataType::F32, Dimensions::Scalar) => acc.with_no_norm(VertexAttributeIter::F32),
            (DataType::U32, Dimensions::Scalar) => acc.with_no_norm(VertexAttributeIter::U32),
            (DataType::F32, Dimensions::Vec2) => acc.with_no_norm(VertexAttributeIter::F32x2),
            (DataType::U32, Dimensions::Vec2) => acc.with_no_norm(VertexAttributeIter::U32x2),
            (DataType::F32, Dimensions::Vec3) => acc.with_no_norm(VertexAttributeIter::F32x3),
            (DataType::U32, Dimensions::Vec3) => acc.with_no_norm(VertexAttributeIter::U32x3),
            (DataType::F32, Dimensions::Vec4) => acc.with_no_norm(VertexAttributeIter::F32x4),
            (DataType::U32, Dimensions::Vec4) => acc.with_no_norm(VertexAttributeIter::U32x4),
            (DataType::I16, Dimensions::Vec2) => acc.with_norm(VertexAttributeIter::S16x2),
            (DataType::U16, Dimensions::Vec2) => acc.with_norm(VertexAttributeIter::U16x2),
            (DataType::I16, Dimensions::Vec4) => acc.with_norm(VertexAttributeIter::S16x4),
            (DataType::U16, Dimensions::Vec4) => acc.with_norm(VertexAttributeIter::U16x4),
            (DataType::I8, Dimensions::Vec2) => acc.with_norm(VertexAttributeIter::S8x2),
            (DataType::U8, Dimensions::Vec2) => acc.with_norm(VertexAttributeIter::U8x2),
            (DataType::I8, Dimensions::Vec4) => acc.with_norm(VertexAttributeIter::S8x4),
            (DataType::U8, Dimensions::Vec4) => acc.with_norm(VertexAttributeIter::U8x4),
            (DataType::U16, Dimensions::Vec3) => acc.with_norm(VertexAttributeIter::U16x3),
            (DataType::U8, Dimensions::Vec3) => acc.with_norm(VertexAttributeIter::U8x3),
            _ => Err(AccessFailed::UnsupportedFormat),
        }
    }

    /// Materializes values for any supported format of vertex attribute
    fn into_any_values(self) -> Result<VertexAttributeValues, AccessFailed> {
        match self {
            VertexAttributeIter::F32(it) => Ok(VertexAttributeValues::Float32(it.collect())),
            VertexAttributeIter::U32(it) => Ok(VertexAttributeValues::Uint32(it.collect())),
            VertexAttributeIter::F32x2(it) => Ok(VertexAttributeValues::Float32x2(it.collect())),
            VertexAttributeIter::U32x2(it) => Ok(VertexAttributeValues::Uint32x2(it.collect())),
            VertexAttributeIter::F32x3(it) => Ok(VertexAttributeValues::Float32x3(it.collect())),
            VertexAttributeIter::U32x3(it) => Ok(VertexAttributeValues::Uint32x3(it.collect())),
            VertexAttributeIter::F32x4(it) => Ok(VertexAttributeValues::Float32x4(it.collect())),
            VertexAttributeIter::U32x4(it) => Ok(VertexAttributeValues::Uint32x4(it.collect())),
            VertexAttributeIter::S16x2(it, n) => Ok(n.apply_either(
                it.collect(),
                VertexAttributeValues::Snorm16x2,
                VertexAttributeValues::Sint16x2,
            )),
            VertexAttributeIter::U16x2(it, n) => Ok(n.apply_either(
                it.collect(),
                VertexAttributeValues::Unorm16x2,
                VertexAttributeValues::Uint16x2,
            )),
            VertexAttributeIter::S16x4(it, n) => Ok(n.apply_either(
                it.collect(),
                VertexAttributeValues::Snorm16x4,
                VertexAttributeValues::Sint16x4,
            )),
            VertexAttributeIter::U16x4(it, n) => Ok(n.apply_either(
                it.collect(),
                VertexAttributeValues::Unorm16x4,
                VertexAttributeValues::Uint16x4,
            )),
            VertexAttributeIter::S8x2(it, n) => Ok(n.apply_either(
                it.collect(),
                VertexAttributeValues::Snorm8x2,
                VertexAttributeValues::Sint8x2,
            )),
            VertexAttributeIter::U8x2(it, n) => Ok(n.apply_either(
                it.collect(),
                VertexAttributeValues::Unorm8x2,
                VertexAttributeValues::Uint8x2,
            )),
            VertexAttributeIter::S8x4(it, n) => Ok(n.apply_either(
                it.collect(),
                VertexAttributeValues::Snorm8x4,
                VertexAttributeValues::Sint8x4,
            )),
            VertexAttributeIter::U8x4(it, n) => Ok(n.apply_either(
                it.collect(),
                VertexAttributeValues::Unorm8x4,
                VertexAttributeValues::Uint8x4,
            )),
            _ => Err(AccessFailed::UnsupportedFormat),
        }
    }

    /// Materializes RGBA values, converting compatible formats to Float32x4
    fn into_rgba_values(self) -> Result<VertexAttributeValues, AccessFailed> {
        match self {
            VertexAttributeIter::U8x3(it, Normalization(true)) => Ok(
                VertexAttributeValues::Float32x4(ReadColors::RgbU8(it).into_rgba_f32().collect()),
            ),
            VertexAttributeIter::U16x3(it, Normalization(true)) => Ok(
                VertexAttributeValues::Float32x4(ReadColors::RgbU16(it).into_rgba_f32().collect()),
            ),
            VertexAttributeIter::F32x3(it) => Ok(VertexAttributeValues::Float32x4(
                ReadColors::RgbF32(it).into_rgba_f32().collect(),
            )),
            VertexAttributeIter::U8x4(it, Normalization(true)) => Ok(
                VertexAttributeValues::Float32x4(ReadColors::RgbaU8(it).into_rgba_f32().collect()),
            ),
            VertexAttributeIter::U16x4(it, Normalization(true)) => Ok(
                VertexAttributeValues::Float32x4(ReadColors::RgbaU16(it).into_rgba_f32().collect()),
            ),
            s => s.into_any_values(),
        }
    }

    /// Materializes joint index values, converting compatible formats to Uint16x4
    fn into_joint_index_values(self) -> Result<VertexAttributeValues, AccessFailed> {
        match self {
            VertexAttributeIter::U8x4(it, Normalization(false)) => Ok(
                VertexAttributeValues::Uint16x4(ReadJoints::U8(it).into_u16().collect()),
            ),
            s => s.into_any_values(),
        }
    }

    /// Materializes texture coordinate values, converting compatible formats to Float32x2
    fn into_tex_coord_values(self) -> Result<VertexAttributeValues, AccessFailed> {
        match self {
            VertexAttributeIter::U8x2(it, Normalization(true)) => Ok(
                VertexAttributeValues::Float32x2(ReadTexCoords::U8(it).into_f32().collect()),
            ),
            VertexAttributeIter::U16x2(it, Normalization(true)) => Ok(
                VertexAttributeValues::Float32x2(ReadTexCoords::U16(it).into_f32().collect()),
            ),
            s => s.into_any_values(),
        }
    }
}

enum VertexAttributeConversion {
    Any,
    Rgba,
    JointIndex,
    TexCoord,
}

/// Loads an entire glTF file.
async fn load_gltf<'a, 'b>(
    bytes: &'a [u8],
    load_context: &'a mut LoadContext<'b>,
    loader: &GltfLoader,
) -> Result<(), GltfError> {
    let gltf = gltf::Gltf::from_slice(bytes)?;
    let buffer_data = load_buffers(&gltf, load_context, load_context.path()).await?;

    let mut materials = vec![];
    let mut named_materials = HashMap::default();
    let mut linear_textures = HashSet::default();
    for material in gltf.materials() {
        let handle = load_material(&material, load_context);
        if let Some(name) = material.name() {
            named_materials.insert(name.to_string(), handle.clone());
        }
        materials.push(handle);
        if let Some(texture) = material.normal_texture() {
            linear_textures.insert(texture.texture().index());
        }
        if let Some(texture) = material.occlusion_texture() {
            linear_textures.insert(texture.texture().index());
        }
        if let Some(texture) = material
            .pbr_metallic_roughness()
            .metallic_roughness_texture()
        {
            linear_textures.insert(texture.texture().index());
        }
    }

    #[cfg(feature = "bevy::animation")]
    let paths = {
        let mut paths = HashMap::<usize, (usize, Vec<Name>)>::new();
        for scene in gltf.scenes() {
            for node in scene.nodes() {
                let root_index = node.index();
                paths_recur(node, &[], &mut paths, root_index);
            }
        }
        paths
    };

    #[cfg(feature = "bevy::animation")]
    let (animations, named_animations, animation_roots) = {
        let mut animations = vec![];
        let mut named_animations = HashMap::default();
        let mut animation_roots = HashSet::default();
        for animation in gltf.animations() {
            let mut animation_clip = bevy::animation::AnimationClip::default();
            for channel in animation.channels() {
                match channel.sampler().interpolation() {
                    gltf::animation::Interpolation::Linear => (),
                    other => warn!(
                        "Animation interpolation {:?} is not supported, will use linear",
                        other
                    ),
                };
                let node = channel.target().node();
                let reader = channel.reader(|buffer| Some(&buffer_data[buffer.index()]));
                let keyframe_timestamps: Vec<f32> = if let Some(inputs) = reader.read_inputs() {
                    match inputs {
                        gltf::accessor::Iter::Standard(times) => times.collect(),
                        gltf::accessor::Iter::Sparse(_) => {
                            warn!("Sparse accessor not supported for animation sampler input");
                            continue;
                        }
                    }
                } else {
                    warn!("Animations without a sampler input are not supported");
                    return Err(GltfError::MissingAnimationSampler(animation.index()));
                };

                let keyframes = if let Some(outputs) = reader.read_outputs() {
                    match outputs {
                        gltf::animation::util::ReadOutputs::Translations(tr) => {
                            bevy::animation::Keyframes::Translation(tr.map(Vec3::from).collect())
                        }
                        gltf::animation::util::ReadOutputs::Rotations(rots) => {
                            bevy::animation::Keyframes::Rotation(
                                rots.into_f32().map(bevy::math::Quat::from_array).collect(),
                            )
                        }
                        gltf::animation::util::ReadOutputs::Scales(scale) => {
                            bevy::animation::Keyframes::Scale(scale.map(Vec3::from).collect())
                        }
                        gltf::animation::util::ReadOutputs::MorphTargetWeights(_) => {
                            warn!("Morph animation property not yet supported");
                            continue;
                        }
                    }
                } else {
                    warn!("Animations without a sampler output are not supported");
                    return Err(GltfError::MissingAnimationSampler(animation.index()));
                };

                if let Some((root_index, path)) = paths.get(&node.index()) {
                    animation_roots.insert(root_index);
                    animation_clip.add_curve_to_path(
                        bevy::animation::EntityPath {
                            parts: path.clone(),
                        },
                        bevy::animation::VariableCurve {
                            keyframe_timestamps,
                            keyframes,
                        },
                    );
                } else {
                    warn!(
                        "Animation ignored for node {}: part of its hierarchy is missing a name",
                        node.index()
                    );
                }
            }
            let handle = load_context.set_labeled_asset(
                &format!("Animation{}", animation.index()),
                LoadedAsset::new(animation_clip),
            );
            if let Some(name) = animation.name() {
                named_animations.insert(name.to_string(), handle.clone());
            }
            animations.push(handle);
        }
        (animations, named_animations, animation_roots)
    };

    let mut meshes = vec![];
    let mut named_meshes = HashMap::default();
    for mesh in gltf.meshes() {
        let mut primitives = vec![];
        for primitive in mesh.primitives() {
            let primitive_label = primitive_label(&mesh, &primitive);
            let primitive_topology = get_primitive_topology(primitive.mode())?;

            let mut mesh = Mesh::new(primitive_topology);

            // Read vertex attributes
            for (semantic, accessor) in primitive.attributes() {
                if let Some((attribute, conversion)) = match &semantic {
                    gltf::Semantic::Positions => {
                        Some((Mesh::ATTRIBUTE_POSITION, VertexAttributeConversion::Any))
                    }
                    gltf::Semantic::Normals => {
                        Some((Mesh::ATTRIBUTE_NORMAL, VertexAttributeConversion::Any))
                    }
                    gltf::Semantic::Tangents => {
                        Some((Mesh::ATTRIBUTE_TANGENT, VertexAttributeConversion::Any))
                    }
                    gltf::Semantic::Colors(0) => {
                        Some((Mesh::ATTRIBUTE_COLOR, VertexAttributeConversion::Rgba))
                    }
                    gltf::Semantic::TexCoords(0) => {
                        Some((Mesh::ATTRIBUTE_UV_0, VertexAttributeConversion::TexCoord))
                    }
                    gltf::Semantic::Joints(0) => Some((
                        Mesh::ATTRIBUTE_JOINT_INDEX,
                        VertexAttributeConversion::JointIndex,
                    )),
                    gltf::Semantic::Weights(0) => {
                        Some((Mesh::ATTRIBUTE_JOINT_WEIGHT, VertexAttributeConversion::Any))
                    }
                    gltf::Semantic::Extras(name) => loader
                        .custom_vertex_attributes
                        .get(name)
                        .map(|attr| (attr.clone(), VertexAttributeConversion::Any)),
                    _ => None,
                } {
                    let raw_iter =
                        VertexAttributeIter::from_accessor(accessor.clone(), &buffer_data);
                    let converted_values = raw_iter.and_then(|iter| match conversion {
                        VertexAttributeConversion::Any => iter.into_any_values(),
                        VertexAttributeConversion::Rgba => iter.into_rgba_values(),
                        VertexAttributeConversion::TexCoord => iter.into_tex_coord_values(),
                        VertexAttributeConversion::JointIndex => iter.into_joint_index_values(),
                    });
                    match converted_values {
                        Ok(values) => {
                            let loaded_format = VertexFormat::from(&values);
                            if attribute.format == loaded_format {
                                mesh.insert_attribute(attribute, values);
                            } else {
                                warn!("Vertex attribute {:?} has format {:?} but expected {:?} for target attribute {}",
                                    semantic, loaded_format, attribute.format, attribute.name
                                );
                            }
                        }
                        Err(AccessFailed::MalformedData) => {
                            warn!(
                                "Malformed vertex attribute data in accessor {}",
                                accessor.index()
                            );
                        }
                        Err(AccessFailed::UnsupportedFormat) => {
                            warn!("Unsupported vertex attribute format {:?},{:?},normalized={} in accessor {}",
                                accessor.data_type(), accessor.dimensions(), accessor.normalized(), accessor.index()
                            );
                        }
                    }
                } else {
                    warn!("Unrecognised vertex attribute {:?}", semantic);
                }
            }

            // Read vertex indices
            let reader = primitive.reader(|buffer| Some(buffer_data[buffer.index()].as_slice()));
            if let Some(indices) = reader.read_indices() {
                mesh.set_indices(Some(match indices {
                    ReadIndices::U8(is) => Indices::U16(is.map(|x| x as u16).collect()),
                    ReadIndices::U16(is) => Indices::U16(is.collect()),
                    ReadIndices::U32(is) => Indices::U32(is.collect()),
                }));
            };

            if mesh.attribute(Mesh::ATTRIBUTE_NORMAL).is_none()
                && matches!(mesh.primitive_topology(), PrimitiveTopology::TriangleList)
            {
                let vertex_count_before = mesh.count_vertices();
                mesh.duplicate_vertices();
                mesh.compute_flat_normals();
                let vertex_count_after = mesh.count_vertices();

                if vertex_count_before != vertex_count_after {
                    bevy::log::debug!("Missing vertex normals in indexed geometry, computing them as flat. Vertex count increased from {} to {}", vertex_count_before, vertex_count_after);
                } else {
                    bevy::log::debug!(
                        "Missing vertex normals in indexed geometry, computing them as flat."
                    );
                }
            }

            if let Some(vertex_attribute) = reader
                .read_tangents()
                .map(|v| VertexAttributeValues::Float32x4(v.collect()))
            {
                mesh.insert_attribute(Mesh::ATTRIBUTE_TANGENT, vertex_attribute);
            } else if mesh.attribute(Mesh::ATTRIBUTE_NORMAL).is_some()
                && primitive.material().normal_texture().is_some()
            {
                bevy::log::debug!(
                    "Missing vertex tangents, computing them using the mikktspace algorithm"
                );
                if let Err(err) = mesh.generate_tangents() {
                    bevy::log::warn!(
                        "Failed to generate vertex tangents using the mikktspace algorithm: {:?}",
                        err
                    );
                }
            }

            let mesh = load_context.set_labeled_asset(&primitive_label, LoadedAsset::new(mesh));
            primitives.push(super::GltfPrimitive {
                mesh,
                material: primitive
                    .material()
                    .index()
                    .and_then(|i| materials.get(i).cloned()),
            });
        }

        let handle = load_context.set_labeled_asset(
            &mesh_label(&mesh),
            LoadedAsset::new(super::GltfMesh { primitives }),
        );
        if let Some(name) = mesh.name() {
            named_meshes.insert(name.to_string(), handle.clone());
        }
        meshes.push(handle);
    }

    let mut nodes_intermediate = vec![];
    let mut named_nodes_intermediate = HashMap::default();
    for node in gltf.nodes() {
        let node_label = node_label(&node);
        nodes_intermediate.push((
            node_label,
            GltfNode {
                children: vec![],
                mesh: node
                    .mesh()
                    .map(|mesh| mesh.index())
                    .and_then(|i| meshes.get(i).cloned()),
                transform: match node.transform() {
                    gltf::scene::Transform::Matrix { matrix } => {
                        Transform::from_matrix(bevy::math::Mat4::from_cols_array_2d(&matrix))
                    }
                    gltf::scene::Transform::Decomposed {
                        translation,
                        rotation,
                        scale,
                    } => Transform {
                        translation: bevy::math::Vec3::from(translation),
                        rotation: bevy::math::Quat::from_array(rotation),
                        scale: bevy::math::Vec3::from(scale),
                    },
                },
            },
            node.children()
                .map(|child| child.index())
                .collect::<Vec<_>>(),
        ));
        if let Some(name) = node.name() {
            named_nodes_intermediate.insert(name, node.index());
        }
    }
    let nodes = resolve_node_hierarchy(nodes_intermediate, load_context.path())
        .into_iter()
        .map(|(label, node)| load_context.set_labeled_asset(&label, LoadedAsset::new(node)))
        .collect::<Vec<bevy::asset::Handle<GltfNode>>>();
    let named_nodes = named_nodes_intermediate
        .into_iter()
        .filter_map(|(name, index)| {
            nodes
                .get(index)
                .map(|handle| (name.to_string(), handle.clone()))
        })
        .collect();

    // TODO: use the threaded impl on wasm once wasm thread pool doesn't deadlock on it
    // See https://github.com/bevyengine/bevy/issues/1924 for more details
    // The taskpool use is also avoided when there is only one texture for performance reasons and
    // to avoid https://github.com/bevyengine/bevy/pull/2725
    if gltf.textures().len() == 1 || cfg!(target_arch = "wasm32") {
        for gltf_texture in gltf.textures() {
            let (texture, label) = load_texture(
                gltf_texture,
                &buffer_data,
                &linear_textures,
                load_context,
                loader.supported_compressed_formats,
            )
            .await?;
            load_context.set_labeled_asset(&label, LoadedAsset::new(texture));
        }
    } else {
        #[cfg(not(target_arch = "wasm32"))]
        IoTaskPool::get()
            .scope(|scope| {
                gltf.textures().for_each(|gltf_texture| {
                    let linear_textures = &linear_textures;
                    let load_context: &LoadContext = load_context;
                    let buffer_data = &buffer_data;
                    scope.spawn(async move {
                        load_texture(
                            gltf_texture,
                            buffer_data,
                            linear_textures,
                            load_context,
                            loader.supported_compressed_formats,
                        )
                        .await
                    });
                });
            })
            .into_iter()
            .filter_map(|res| {
                if let Err(err) = res.as_ref() {
                    warn!("Error loading glTF texture: {}", err);
                }
                res.ok()
            })
            .for_each(|(texture, label)| {
                load_context.set_labeled_asset(&label, LoadedAsset::new(texture));
            });
    }

    let skinned_mesh_inverse_bindposes: Vec<_> = gltf
        .skins()
        .map(|gltf_skin| {
            let reader = gltf_skin.reader(|buffer| Some(&buffer_data[buffer.index()]));
            let inverse_bindposes: Vec<Mat4> = reader
                .read_inverse_bind_matrices()
                .unwrap()
                .map(|mat| Mat4::from_cols_array_2d(&mat))
                .collect();

            load_context.set_labeled_asset(
                &skin_label(&gltf_skin),
                LoadedAsset::new(SkinnedMeshInverseBindposes::from(inverse_bindposes)),
            )
        })
        .collect();

    let mut scenes = vec![];
    let mut named_scenes = HashMap::default();
    let mut active_camera_found = false;
    for scene in gltf.scenes() {
        let mut err = None;
        let mut world = World::default();
        let mut node_index_to_entity_map = HashMap::new();
        let mut entity_to_skin_index_map = HashMap::new();

        world
            .spawn(SpatialBundle::VISIBLE_IDENTITY)
            .with_children(|parent| {
                for node in scene.nodes() {
                    let result = load_node(
                        &node,
                        parent,
                        load_context,
                        &mut node_index_to_entity_map,
                        &mut entity_to_skin_index_map,
                        &mut active_camera_found,
                    );
                    if result.is_err() {
                        err = Some(result);
                        return;
                    }
                }
            });
        if let Some(Err(err)) = err {
            return Err(err);
        }

        #[cfg(feature = "bevy::animation")]
        {
            // for each node root in a scene, check if it's the root of an animation
            // if it is, add the AnimationPlayer component
            for node in scene.nodes() {
                if animation_roots.contains(&node.index()) {
                    world
                        .entity_mut(*node_index_to_entity_map.get(&node.index()).unwrap())
                        .insert(bevy::animation::AnimationPlayer::default());
                }
            }
        }

        for (&entity, &skin_index) in &entity_to_skin_index_map {
            let mut entity = world.entity_mut(entity);
            let skin = gltf.skins().nth(skin_index).unwrap();
            let joint_entities: Vec<_> = skin
                .joints()
                .map(|node| node_index_to_entity_map[&node.index()])
                .collect();

            entity.insert(SkinnedMesh {
                inverse_bindposes: skinned_mesh_inverse_bindposes[skin_index].clone(),
                joints: joint_entities,
            });
        }

        let scene_handle = load_context
            .set_labeled_asset(&scene_label(&scene), LoadedAsset::new(Scene::new(world)));

        if let Some(name) = scene.name() {
            named_scenes.insert(name.to_string(), scene_handle.clone());
        }
        scenes.push(scene_handle);
    }

    load_context.set_default_asset(LoadedAsset::new(Gltf {
        default_scene: gltf
            .default_scene()
            .and_then(|scene| scenes.get(scene.index()))
            .cloned(),
        scenes,
        named_scenes,
        meshes,
        named_meshes,
        materials,
        named_materials,
        nodes,
        named_nodes,
        #[cfg(feature = "bevy::animation")]
        animations,
        #[cfg(feature = "bevy::animation")]
        named_animations,
    }));

    Ok(())
}

fn node_name(node: &Node) -> Name {
    let name = node
        .name()
        .map(|s| s.to_string())
        .unwrap_or_else(|| format!("GltfNode{}", node.index()));
    Name::new(name)
}

#[cfg(feature = "bevy::animation")]
fn paths_recur(
    node: Node,
    current_path: &[Name],
    paths: &mut HashMap<usize, (usize, Vec<Name>)>,
    root_index: usize,
) {
    let mut path = current_path.to_owned();
    path.push(node_name(&node));
    for child in node.children() {
        paths_recur(child, &path, paths, root_index);
    }
    paths.insert(node.index(), (root_index, path));
}

/// Loads a glTF texture as a bevy [`Image`] and returns it together with its label.
async fn load_texture<'a>(
    gltf_texture: gltf::Texture<'a>,
    buffer_data: &[Vec<u8>],
    linear_textures: &HashSet<usize>,
    load_context: &LoadContext<'a>,
    supported_compressed_formats: CompressedImageFormats,
) -> Result<(Image, String), GltfError> {
    let is_srgb = !linear_textures.contains(&gltf_texture.index());
    let mut texture = match gltf_texture.source().source() {
        gltf::image::Source::View { view, mime_type } => {
            let start = view.offset();
            let end = view.offset() + view.length();
            let buffer = &buffer_data[view.buffer().index()][start..end];
            Image::from_buffer(
                buffer,
                ImageType::MimeType(mime_type),
                supported_compressed_formats,
                is_srgb,
            )?
        }
        gltf::image::Source::Uri { uri, mime_type } => {
            let uri = percent_encoding::percent_decode_str(uri)
                .decode_utf8()
                .unwrap();
            let uri = uri.as_ref();
            let (bytes, image_type) = if let Ok(data_uri) = DataUri::parse(uri) {
                (data_uri.decode()?, ImageType::MimeType(data_uri.mime_type))
            } else {
                let parent = load_context.path().parent().unwrap();
                let image_path = parent.join(uri);
                let bytes = load_context.read_asset_bytes(image_path.clone()).await?;

                let extension = Path::new(uri).extension().unwrap().to_str().unwrap();
                let image_type = ImageType::Extension(extension);

                (bytes, image_type)
            };

            Image::from_buffer(
                &bytes,
                mime_type.map(ImageType::MimeType).unwrap_or(image_type),
                supported_compressed_formats,
                is_srgb,
            )?
        }
    };
    texture.sampler_descriptor = ImageSampler::Descriptor(texture_sampler(&gltf_texture));

    Ok((texture, texture_label(&gltf_texture)))
}

/// Loads a glTF material as a bevy [`StandardMaterial`] and returns it.
fn load_material(material: &Material, load_context: &mut LoadContext) -> Handle<StandardMaterial> {
    let material_label = material_label(material);

    let pbr = material.pbr_metallic_roughness();

    let color = pbr.base_color_factor();
    let base_color_texture = pbr.base_color_texture().map(|info| {
        // TODO: handle info.tex_coord() (the *set* index for the right texcoords)
        let label = texture_label(&info.texture());
        let path = AssetPath::new_ref(load_context.path(), Some(&label));
        load_context.get_handle(path)
    });

    let normal_map_texture: Option<Handle<Image>> =
        material.normal_texture().map(|normal_texture| {
            // TODO: handle normal_texture.scale
            // TODO: handle normal_texture.tex_coord() (the *set* index for the right texcoords)
            let label = texture_label(&normal_texture.texture());
            let path = AssetPath::new_ref(load_context.path(), Some(&label));
            load_context.get_handle(path)
        });

    let metallic_roughness_texture = pbr.metallic_roughness_texture().map(|info| {
        // TODO: handle info.tex_coord() (the *set* index for the right texcoords)
        let label = texture_label(&info.texture());
        let path = AssetPath::new_ref(load_context.path(), Some(&label));
        load_context.get_handle(path)
    });

    let occlusion_texture = material.occlusion_texture().map(|occlusion_texture| {
        // TODO: handle occlusion_texture.tex_coord() (the *set* index for the right texcoords)
        // TODO: handle occlusion_texture.strength() (a scalar multiplier for occlusion strength)
        let label = texture_label(&occlusion_texture.texture());
        let path = AssetPath::new_ref(load_context.path(), Some(&label));
        load_context.get_handle(path)
    });

    let emissive = material.emissive_factor();
    let emissive_texture = material.emissive_texture().map(|info| {
        // TODO: handle occlusion_texture.tex_coord() (the *set* index for the right texcoords)
        // TODO: handle occlusion_texture.strength() (a scalar multiplier for occlusion strength)
        let label = texture_label(&info.texture());
        let path = AssetPath::new_ref(load_context.path(), Some(&label));
        load_context.get_handle(path)
    });

    load_context.set_labeled_asset(
        &material_label,
        LoadedAsset::new(StandardMaterial {
            base_color: Color::rgba(color[0], color[1], color[2], color[3]),
            base_color_texture,
            perceptual_roughness: pbr.roughness_factor(),
            metallic: pbr.metallic_factor(),
            metallic_roughness_texture,
            normal_map_texture,
            double_sided: material.double_sided(),
            cull_mode: if material.double_sided() {
                None
            } else {
                Some(Face::Back)
            },
            occlusion_texture,
            emissive: Color::rgba(emissive[0], emissive[1], emissive[2], 1.0),
            emissive_texture,
            unlit: material.unlit(),
            alpha_mode: alpha_mode(material),
            ..Default::default()
        }),
    )
}

/// Loads a glTF node.
fn load_node(
    gltf_node: &gltf::Node,
    world_builder: &mut WorldChildBuilder,
    load_context: &mut LoadContext,
    node_index_to_entity_map: &mut HashMap<usize, Entity>,
    entity_to_skin_index_map: &mut HashMap<Entity, usize>,
    active_camera_found: &mut bool,
) -> Result<(), GltfError> {
    let transform = gltf_node.transform();
    let mut gltf_error = None;
    let mut node = world_builder.spawn(SpatialBundle::from(Transform::from_matrix(
        Mat4::from_cols_array_2d(&transform.matrix()),
    )));

    node.insert(node_name(gltf_node));

    if let Some(extras) = gltf_node.extras() {
        node.insert(super::GltfExtras {
            value: extras.get().to_string(),
        });
    }

    // create camera node
    if let Some(camera) = gltf_node.camera() {
        let projection = match camera.projection() {
            gltf::camera::Projection::Orthographic(orthographic) => {
                let xmag = orthographic.xmag();
                let orthographic_projection: OrthographicProjection = OrthographicProjection {
                    far: orthographic.zfar(),
                    near: orthographic.znear(),
                    scaling_mode: ScalingMode::FixedHorizontal(1.0),
                    scale: xmag,
                    ..Default::default()
                };

                Projection::Orthographic(orthographic_projection)
            }
            gltf::camera::Projection::Perspective(perspective) => {
                let mut perspective_projection: PerspectiveProjection = PerspectiveProjection {
                    fov: perspective.yfov(),
                    near: perspective.znear(),
                    ..Default::default()
                };
                if let Some(zfar) = perspective.zfar() {
                    perspective_projection.far = zfar;
                }
                if let Some(aspect_ratio) = perspective.aspect_ratio() {
                    perspective_projection.aspect_ratio = aspect_ratio;
                }
                Projection::Perspective(perspective_projection)
            }
        };

        node.insert((
            projection,
            Camera {
                is_active: !*active_camera_found,
                ..Default::default()
            },
            VisibleEntities::default(),
            Frustum::default(),
            Camera3d::default(),
            CameraRenderGraph::new(bevy::core_pipeline::core_3d::graph::NAME),
        ));

        *active_camera_found = true;
    }

    // Map node index to entity
    node_index_to_entity_map.insert(gltf_node.index(), node.id());

    node.with_children(|parent| {
        if let Some(mesh) = gltf_node.mesh() {
            // append primitives
            for primitive in mesh.primitives() {
                let material = primitive.material();
                let material_label = material_label(&material);

                // This will make sure we load the default material now since it would not have been
                // added when iterating over all the gltf materials (since the default material is
                // not explicitly listed in the gltf).
                if !load_context.has_labeled_asset(&material_label) {
                    load_material(&material, load_context);
                }

                let primitive_label = primitive_label(&mesh, &primitive);
                let bounds = primitive.bounding_box();
                let mesh_asset_path =
                    AssetPath::new_ref(load_context.path(), Some(&primitive_label));
                let material_asset_path =
                    AssetPath::new_ref(load_context.path(), Some(&material_label));

                let mut mesh_entity = parent.spawn(PbrBundle {
                    mesh: load_context.get_handle(mesh_asset_path),
                    material: load_context.get_handle(material_asset_path),
                    ..Default::default()
                });
                mesh_entity.insert(Aabb::from_min_max(
                    Vec3::from_slice(&bounds.min),
                    Vec3::from_slice(&bounds.max),
                ));

                if let Some(extras) = primitive.extras() {
                    mesh_entity.insert(super::GltfExtras {
                        value: extras.get().to_string(),
                    });
                }
                if let Some(name) = mesh.name() {
                    mesh_entity.insert(Name::new(name.to_string()));
                }
                // Mark for adding skinned mesh
                if let Some(skin) = gltf_node.skin() {
                    entity_to_skin_index_map.insert(mesh_entity.id(), skin.index());
                }
            }
        }

        if let Some(light) = gltf_node.light() {
            match light.kind() {
                gltf::khr_lights_punctual::Kind::Directional => {
                    let mut entity = parent.spawn(DirectionalLightBundle {
                        directional_light: DirectionalLight {
                            color: Color::from(light.color()),
                            // NOTE: KHR_punctual_lights defines the intensity units for directional
                            // lights in lux (lm/m^2) which is what we need.
                            illuminance: light.intensity(),
                            ..Default::default()
                        },
                        ..Default::default()
                    });
                    if let Some(name) = light.name() {
                        entity.insert(Name::new(name.to_string()));
                    }
                    if let Some(extras) = light.extras() {
                        entity.insert(super::GltfExtras {
                            value: extras.get().to_string(),
                        });
                    }
                }
                gltf::khr_lights_punctual::Kind::Point => {
                    let mut entity = parent.spawn(PointLightBundle {
                        point_light: PointLight {
                            color: Color::from(light.color()),
                            // NOTE: KHR_punctual_lights defines the intensity units for point lights in
                            // candela (lm/sr) which is luminous intensity and we need luminous power.
                            // For a point light, luminous power = 4 * pi * luminous intensity
                            intensity: light.intensity() * std::f32::consts::PI * 4.0,
                            range: light.range().unwrap_or(20.0),
                            radius: light.range().unwrap_or(0.0),
                            ..Default::default()
                        },
                        ..Default::default()
                    });
                    if let Some(name) = light.name() {
                        entity.insert(Name::new(name.to_string()));
                    }
                    if let Some(extras) = light.extras() {
                        entity.insert(super::GltfExtras {
                            value: extras.get().to_string(),
                        });
                    }
                }
                gltf::khr_lights_punctual::Kind::Spot {
                    inner_cone_angle,
                    outer_cone_angle,
                } => {
                    let mut entity = parent.spawn(SpotLightBundle {
                        spot_light: SpotLight {
                            color: Color::from(light.color()),
                            // NOTE: KHR_punctual_lights defines the intensity units for spot lights in
                            // candela (lm/sr) which is luminous intensity and we need luminous power.
                            // For a spot light, we map luminous power = 4 * pi * luminous intensity
                            intensity: light.intensity() * std::f32::consts::PI * 4.0,
                            range: light.range().unwrap_or(20.0),
                            radius: light.range().unwrap_or(0.0),
                            inner_angle: inner_cone_angle,
                            outer_angle: outer_cone_angle,
                            ..Default::default()
                        },
                        ..Default::default()
                    });
                    if let Some(name) = light.name() {
                        entity.insert(Name::new(name.to_string()));
                    }
                    if let Some(extras) = light.extras() {
                        entity.insert(super::GltfExtras {
                            value: extras.get().to_string(),
                        });
                    }
                }
            }
        }

        // append other nodes
        for child in gltf_node.children() {
            if let Err(err) = load_node(
                &child,
                parent,
                load_context,
                node_index_to_entity_map,
                entity_to_skin_index_map,
                active_camera_found,
            ) {
                gltf_error = Some(err);
                return;
            }
        }
    });
    if let Some(err) = gltf_error {
        Err(err)
    } else {
        Ok(())
    }
}

/// Returns the label for the `mesh`.
fn mesh_label(mesh: &gltf::Mesh) -> String {
    format!("Mesh{}", mesh.index())
}

/// Returns the label for the `mesh` and `primitive`.
fn primitive_label(mesh: &gltf::Mesh, primitive: &Primitive) -> String {
    format!("Mesh{}/Primitive{}", mesh.index(), primitive.index())
}

/// Returns the label for the `material`.
fn material_label(material: &gltf::Material) -> String {
    if let Some(index) = material.index() {
        format!("Material{index}")
    } else {
        "MaterialDefault".to_string()
    }
}

/// Returns the label for the `texture`.
fn texture_label(texture: &gltf::Texture) -> String {
    format!("Texture{}", texture.index())
}

/// Returns the label for the `node`.
fn node_label(node: &gltf::Node) -> String {
    format!("Node{}", node.index())
}

/// Returns the label for the `scene`.
fn scene_label(scene: &gltf::Scene) -> String {
    format!("Scene{}", scene.index())
}

fn skin_label(skin: &gltf::Skin) -> String {
    format!("Skin{}", skin.index())
}

/// Extracts the texture sampler data from the glTF texture.
fn texture_sampler<'a>(texture: &gltf::Texture) -> SamplerDescriptor<'a> {
    let gltf_sampler = texture.sampler();

    SamplerDescriptor {
        address_mode_u: texture_address_mode(&gltf_sampler.wrap_s()),
        address_mode_v: texture_address_mode(&gltf_sampler.wrap_t()),

        mag_filter: gltf_sampler
            .mag_filter()
            .map(|mf| match mf {
                MagFilter::Nearest => FilterMode::Nearest,
                MagFilter::Linear => FilterMode::Linear,
            })
            .unwrap_or(SamplerDescriptor::default().mag_filter),

        min_filter: gltf_sampler
            .min_filter()
            .map(|mf| match mf {
                MinFilter::Nearest
                | MinFilter::NearestMipmapNearest
                | MinFilter::NearestMipmapLinear => FilterMode::Nearest,
                MinFilter::Linear
                | MinFilter::LinearMipmapNearest
                | MinFilter::LinearMipmapLinear => FilterMode::Linear,
            })
            .unwrap_or(SamplerDescriptor::default().min_filter),

        mipmap_filter: gltf_sampler
            .min_filter()
            .map(|mf| match mf {
                MinFilter::Nearest
                | MinFilter::Linear
                | MinFilter::NearestMipmapNearest
                | MinFilter::LinearMipmapNearest => FilterMode::Nearest,
                MinFilter::NearestMipmapLinear | MinFilter::LinearMipmapLinear => {
                    FilterMode::Linear
                }
            })
            .unwrap_or(SamplerDescriptor::default().mipmap_filter),

        ..Default::default()
    }
}

/// Maps the texture address mode form glTF to wgpu.
fn texture_address_mode(gltf_address_mode: &gltf::texture::WrappingMode) -> AddressMode {
    match gltf_address_mode {
        WrappingMode::ClampToEdge => AddressMode::ClampToEdge,
        WrappingMode::Repeat => AddressMode::Repeat,
        WrappingMode::MirroredRepeat => AddressMode::MirrorRepeat,
    }
}

/// Maps the `primitive_topology` form glTF to `wgpu`.
fn get_primitive_topology(mode: Mode) -> Result<PrimitiveTopology, GltfError> {
    match mode {
        Mode::Points => Ok(PrimitiveTopology::PointList),
        Mode::Lines => Ok(PrimitiveTopology::LineList),
        Mode::LineStrip => Ok(PrimitiveTopology::LineStrip),
        Mode::Triangles => Ok(PrimitiveTopology::TriangleList),
        Mode::TriangleStrip => Ok(PrimitiveTopology::TriangleStrip),
        mode => Err(GltfError::UnsupportedPrimitive { mode }),
    }
}

fn alpha_mode(material: &Material) -> AlphaMode {
    match material.alpha_mode() {
        gltf::material::AlphaMode::Opaque => AlphaMode::Opaque,
        gltf::material::AlphaMode::Mask => AlphaMode::Mask(material.alpha_cutoff().unwrap_or(0.5)),
        gltf::material::AlphaMode::Blend => AlphaMode::Blend,
    }
}

/// Loads the raw glTF buffer data for a specific glTF file.
async fn load_buffers(
    gltf: &gltf::Gltf,
    load_context: &LoadContext<'_>,
    asset_path: &Path,
) -> Result<Vec<Vec<u8>>, GltfError> {
    const VALID_MIME_TYPES: &[&str] = &["application/octet-stream", "application/gltf-buffer"];

    let mut buffer_data = Vec::new();
    for buffer in gltf.buffers() {
        match buffer.source() {
            gltf::buffer::Source::Uri(uri) => {
                let uri = percent_encoding::percent_decode_str(uri)
                    .decode_utf8()
                    .unwrap();
                let uri = uri.as_ref();
                let buffer_bytes = match DataUri::parse(uri) {
                    Ok(data_uri) if VALID_MIME_TYPES.contains(&data_uri.mime_type) => {
                        data_uri.decode()?
                    }
                    Ok(_) => return Err(GltfError::BufferFormatUnsupported),
                    Err(()) => {
                        // TODO: Remove this and add dep
                        let buffer_path = asset_path.parent().unwrap().join(uri);
                        load_context.read_asset_bytes(buffer_path).await?
                    }
                };
                buffer_data.push(buffer_bytes);
            }
            gltf::buffer::Source::Bin => {
                if let Some(blob) = gltf.blob.as_deref() {
                    buffer_data.push(blob.into());
                } else {
                    return Err(GltfError::MissingBlob);
                }
            }
        }
    }

    Ok(buffer_data)
}

fn resolve_node_hierarchy(
    nodes_intermediate: Vec<(String, GltfNode, Vec<usize>)>,
    asset_path: &Path,
) -> Vec<(String, GltfNode)> {
    let mut has_errored = false;
    let mut empty_children = VecDeque::new();
    let mut parents = vec![None; nodes_intermediate.len()];
    let mut unprocessed_nodes = nodes_intermediate
        .into_iter()
        .enumerate()
        .map(|(i, (label, node, children))| {
            for child in &children {
                if let Some(parent) = parents.get_mut(*child) {
                    *parent = Some(i);
                } else if !has_errored {
                    has_errored = true;
                    warn!("Unexpected child in GLTF Mesh {}", child);
                }
            }
            let children = children.into_iter().collect::<HashSet<_>>();
            if children.is_empty() {
                empty_children.push_back(i);
            }
            (i, (label, node, children))
        })
        .collect::<HashMap<_, _>>();
    let mut nodes = std::collections::HashMap::<usize, (String, GltfNode)>::new();
    while let Some(index) = empty_children.pop_front() {
        let (label, node, children) = unprocessed_nodes.remove(&index).unwrap();
        assert!(children.is_empty());
        nodes.insert(index, (label, node));
        if let Some(parent_index) = parents[index] {
            let (_, parent_node, parent_children) =
                unprocessed_nodes.get_mut(&parent_index).unwrap();

            assert!(parent_children.remove(&index));
            if let Some((_, child_node)) = nodes.get(&index) {
                parent_node.children.push(child_node.clone());
            }
            if parent_children.is_empty() {
                empty_children.push_back(parent_index);
            }
        }
    }
    if !unprocessed_nodes.is_empty() {
        warn!("GLTF model must be a tree: {:?}", asset_path);
    }
    let mut nodes_to_sort = nodes.into_iter().collect::<Vec<_>>();
    nodes_to_sort.sort_by_key(|(i, _)| *i);
    nodes_to_sort
        .into_iter()
        .map(|(_, resolved)| resolved)
        .collect()
}

struct DataUri<'a> {
    mime_type: &'a str,
    base64: bool,
    data: &'a str,
}

fn split_once(input: &str, delimiter: char) -> Option<(&str, &str)> {
    let mut iter = input.splitn(2, delimiter);
    Some((iter.next()?, iter.next()?))
}

impl<'a> DataUri<'a> {
    fn parse(uri: &'a str) -> Result<DataUri<'a>, ()> {
        let uri = uri.strip_prefix("data:").ok_or(())?;
        let (mime_type, data) = split_once(uri, ',').ok_or(())?;

        let (mime_type, base64) = match mime_type.strip_suffix(";base64") {
            Some(mime_type) => (mime_type, true),
            None => (mime_type, false),
        };

        Ok(DataUri {
            mime_type,
            base64,
            data,
        })
    }

    fn decode(&self) -> Result<Vec<u8>, base64::DecodeError> {
        if self.base64 {
            base64::decode(self.data)
        } else {
            Ok(self.data.as_bytes().to_owned())
        }
    }
}

#[cfg(test)]
mod test {
    use std::path::PathBuf;

    use super::resolve_node_hierarchy;
    use crate::GltfNode;

    impl GltfNode {
        fn empty() -> Self {
            GltfNode {
                children: vec![],
                mesh: None,
                transform: bevy::transform::prelude::Transform::IDENTITY,
            }
        }
    }
    #[test]
    fn node_hierarchy_single_node() {
        let result = resolve_node_hierarchy(
            vec![("l1".to_string(), GltfNode::empty(), vec![])],
            PathBuf::new().as_path(),
        );

        assert_eq!(result.len(), 1);
        assert_eq!(result[0].0, "l1");
        assert_eq!(result[0].1.children.len(), 0);
    }

    #[test]
    fn node_hierarchy_no_hierarchy() {
        let result = resolve_node_hierarchy(
            vec![
                ("l1".to_string(), GltfNode::empty(), vec![]),
                ("l2".to_string(), GltfNode::empty(), vec![]),
            ],
            PathBuf::new().as_path(),
        );

        assert_eq!(result.len(), 2);
        assert_eq!(result[0].0, "l1");
        assert_eq!(result[0].1.children.len(), 0);
        assert_eq!(result[1].0, "l2");
        assert_eq!(result[1].1.children.len(), 0);
    }

    #[test]
    fn node_hierarchy_simple_hierarchy() {
        let result = resolve_node_hierarchy(
            vec![
                ("l1".to_string(), GltfNode::empty(), vec![1]),
                ("l2".to_string(), GltfNode::empty(), vec![]),
            ],
            PathBuf::new().as_path(),
        );

        assert_eq!(result.len(), 2);
        assert_eq!(result[0].0, "l1");
        assert_eq!(result[0].1.children.len(), 1);
        assert_eq!(result[1].0, "l2");
        assert_eq!(result[1].1.children.len(), 0);
    }

    #[test]
    fn node_hierarchy_hierarchy() {
        let result = resolve_node_hierarchy(
            vec![
                ("l1".to_string(), GltfNode::empty(), vec![1]),
                ("l2".to_string(), GltfNode::empty(), vec![2]),
                ("l3".to_string(), GltfNode::empty(), vec![3, 4, 5]),
                ("l4".to_string(), GltfNode::empty(), vec![6]),
                ("l5".to_string(), GltfNode::empty(), vec![]),
                ("l6".to_string(), GltfNode::empty(), vec![]),
                ("l7".to_string(), GltfNode::empty(), vec![]),
            ],
            PathBuf::new().as_path(),
        );

        assert_eq!(result.len(), 7);
        assert_eq!(result[0].0, "l1");
        assert_eq!(result[0].1.children.len(), 1);
        assert_eq!(result[1].0, "l2");
        assert_eq!(result[1].1.children.len(), 1);
        assert_eq!(result[2].0, "l3");
        assert_eq!(result[2].1.children.len(), 3);
        assert_eq!(result[3].0, "l4");
        assert_eq!(result[3].1.children.len(), 1);
        assert_eq!(result[4].0, "l5");
        assert_eq!(result[4].1.children.len(), 0);
        assert_eq!(result[5].0, "l6");
        assert_eq!(result[5].1.children.len(), 0);
        assert_eq!(result[6].0, "l7");
        assert_eq!(result[6].1.children.len(), 0);
    }

    #[test]
    fn node_hierarchy_cyclic() {
        let result = resolve_node_hierarchy(
            vec![
                ("l1".to_string(), GltfNode::empty(), vec![1]),
                ("l2".to_string(), GltfNode::empty(), vec![0]),
            ],
            PathBuf::new().as_path(),
        );

        assert_eq!(result.len(), 0);
    }

    #[test]
    fn node_hierarchy_missing_node() {
        let result = resolve_node_hierarchy(
            vec![
                ("l1".to_string(), GltfNode::empty(), vec![2]),
                ("l2".to_string(), GltfNode::empty(), vec![]),
            ],
            PathBuf::new().as_path(),
        );

        assert_eq!(result.len(), 1);
        assert_eq!(result[0].0, "l2");
        assert_eq!(result[0].1.children.len(), 0);
    }
}