//! obj sink
mod material;
mod obj_writer;

use std::{
    f64::consts::FRAC_PI_2,
    path::PathBuf,
    sync::{mpsc, Mutex},
    time::Instant,
};

use ahash::{HashMap, HashMapExt};
use atlas_packer::{
    export::{AtlasExporter as _, JpegAtlasExporter},
    pack::TexturePacker,
    place::{GuillotineTexturePlacer, TexturePlacerConfig},
    texture::{DownsampleFactor, TextureCache},
};
use earcut::{utils3d::project3d_to_2d, Earcut};
use flatgeom::MultiPolygon;
use glam::{DMat4, DVec3, DVec4};
use indexmap::IndexSet;
use itertools::Itertools;
use material::{Material, Texture};
use obj_writer::write;
use rayon::prelude::*;
use serde::{Deserialize, Serialize};
use url::Url;

use nusamai_citygml::{
    object::{ObjectStereotype, Value},
    schema::Schema,
    GeometryType,
};
use nusamai_plateau::appearance;
use nusamai_projection::cartesian::geodetic_to_geocentric;

use crate::{
    get_parameter_value,
    parameters::*,
    pipeline::{Feedback, PipelineError, Receiver, Result},
    sink::{DataRequirements, DataSink, DataSinkProvider, SinkInfo},
    transformer,
    transformer::{TransformerConfig, TransformerOption, TransformerRegistry},
};

pub struct ObjAtlasSinkProvider {}

impl DataSinkProvider for ObjAtlasSinkProvider {
    fn info(&self) -> SinkInfo {
        SinkInfo {
            id_name: "obj_atlas".to_string(),
            name: "OBJ_ATLAS".to_string(),
        }
    }

    fn parameters(&self) -> Parameters {
        let mut params = Parameters::new();
        params.define(
            "@output".into(),
            ParameterEntry {
                description: "Output file path".into(),
                required: true,
                parameter: ParameterType::FileSystemPath(FileSystemPathParameter {
                    value: None,
                    must_exist: false,
                }),
                label: None,
            },
        );

        params.define(
            "transform".into(),
            ParameterEntry {
                description: "transform option".into(),
                required: false,
                parameter: ParameterType::String(StringParameter { value: None }),
                label: None,
            },
        );

        params.define(
            "split".into(),
            ParameterEntry {
                description: "Splitting objects".into(),
                required: true,
                parameter: ParameterType::Boolean(BooleanParameter { value: Some(false) }),
                label: Some("オブジェクトを分割する".into()),
            },
        );

        params
    }

    fn available_transformer(&self) -> TransformerRegistry {
        let mut settings: TransformerRegistry = TransformerRegistry::new();

        settings.insert(TransformerConfig {
            key: "use_texture".to_string(),
            label: "テクスチャの使用".to_string(),
            is_enabled: false,
            requirements: vec![transformer::Requirement::UseAppearance],
        });

        settings
    }

    fn create(&self, params: &Parameters) -> Box<dyn DataSink> {
        let output_path = get_parameter_value!(params, "@output", FileSystemPath);
        let transform_options = self.available_transformer();
        let is_split = get_parameter_value!(params, "split", Boolean).unwrap();

        Box::<ObjAtlasSink>::new(ObjAtlasSink {
            output_path: output_path.as_ref().unwrap().into(),
            transform_settings: transform_options,
            obj_options: ObjParams { is_split },
        })
    }
}

pub struct ObjAtlasSink {
    output_path: PathBuf,
    transform_settings: TransformerRegistry,
    obj_options: ObjParams,
}

struct ObjParams {
    is_split: bool,
}

#[derive(Debug)]
pub struct BoundingVolume {
    pub min_lng: f64,
    pub max_lng: f64,
    pub min_lat: f64,
    pub max_lat: f64,
    pub min_height: f64,
    pub max_height: f64,
}

impl BoundingVolume {
    fn update(&mut self, other: &Self) {
        self.min_lng = self.min_lng.min(other.min_lng);
        self.max_lng = self.max_lng.max(other.max_lng);
        self.min_lat = self.min_lat.min(other.min_lat);
        self.max_lat = self.max_lat.max(other.max_lat);
        self.min_height = self.min_height.min(other.min_height);
        self.max_height = self.max_height.max(other.max_height);
    }
}

impl Default for BoundingVolume {
    fn default() -> Self {
        Self {
            min_lng: f64::MAX,
            max_lng: f64::MIN,
            min_lat: f64::MAX,
            max_lat: f64::MIN,
            min_height: f64::MAX,
            max_height: f64::MIN,
        }
    }
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct Feature {
    // polygons [x, y, z, u, v]
    pub polygons: MultiPolygon<'static, [f64; 5]>,
    // material ids for each polygon
    pub polygon_material_ids: Vec<u32>,
    // materials
    pub materials: IndexSet<Material>,
    // feature_id
    pub feature_id: String,
}

type ClassifiedFeatures = HashMap<String, ClassFeatures>;

#[derive(Default, Debug)]
pub struct ClassFeatures {
    features: Vec<Feature>,
    bounding_volume: BoundingVolume,
}

pub type FeatureId = String;
pub type MaterialKey = String;
pub type ObjInfo = HashMap<FeatureId, FeatureMesh>;
pub type ObjMaterials = HashMap<MaterialKey, FeatureMaterial>;

pub struct FeatureMesh {
    pub vertices: Vec<[f64; 3]>,
    pub uvs: Vec<[f64; 2]>,
    pub primitives: HashMap<MaterialKey, Vec<u32>>,
}

pub struct FeatureMaterial {
    pub base_color: [f32; 4],
    pub texture_uri: Option<Url>,
}

impl DataSink for ObjAtlasSink {
    fn make_requirements(&mut self, properties: Vec<TransformerOption>) -> DataRequirements {
        let default_requirements: DataRequirements = DataRequirements {
            resolve_appearance: true,
            key_value: crate::transformer::KeyValueSpec::JsonifyObjectsAndArrays,
            ..Default::default()
        };

        for prop in properties {
            let _ = &self
                .transform_settings
                .update_transformer(&prop.key, prop.is_enabled);
        }

        self.transform_settings.build(default_requirements)
    }

    fn run(&mut self, upstream: Receiver, feedback: &Feedback, _schema: &Schema) -> Result<()> {
        let preprocessing_start = Instant::now();

        let ellipsoid = nusamai_projection::ellipsoid::wgs84();

        let classified_features: Mutex<ClassifiedFeatures> = Default::default();

        // Construct a Feature classified by typename from Entity
        // Feature has polygons, attributes, and materials.
        // The coordinates of polygon store the actual coordinate values (WGS84) and UV coordinates, not the index.
        let _ = upstream.into_iter().par_bridge().try_for_each(|parcel| {
            feedback.ensure_not_canceled()?;

            let entity = parcel.entity;

            // entity must be a Feature
            let Value::Object(obj) = &entity.root else {
                return Ok(());
            };
            let ObjectStereotype::Feature { geometries, .. } = &obj.stereotype else {
                return Ok(());
            };

            let geom_store = entity.geometry_store.read().unwrap();
            if geom_store.multipolygon.is_empty() {
                return Ok(());
            }
            let appearance_store = entity.appearance_store.read().unwrap();

            let feature_id = obj.stereotype.id().map(|id| id.to_string()).unwrap();

            let mut materials: IndexSet<Material> = IndexSet::new();
            let default_material = appearance::Material::default();

            let mut feature = Feature {
                polygons: MultiPolygon::new(),
                polygon_material_ids: Default::default(),
                materials: Default::default(),
                feature_id,
            };

            let mut local_bvol = BoundingVolume::default();

            geometries.iter().for_each(|entry| {
                match entry.ty {
                    GeometryType::Solid | GeometryType::Surface | GeometryType::Triangle => {
                        // extract the polygon, material, and texture
                        for (((idx_poly, poly_uv), poly_mat), poly_tex) in
                            geom_store
                                .multipolygon
                                .iter_range(entry.pos as usize..(entry.pos + entry.len) as usize)
                                .zip_eq(geom_store.polygon_uvs.iter_range(
                                    entry.pos as usize..(entry.pos + entry.len) as usize,
                                ))
                                .zip_eq(
                                    geom_store.polygon_materials
                                        [entry.pos as usize..(entry.pos + entry.len) as usize]
                                        .iter(),
                                )
                                .zip_eq(
                                    geom_store.polygon_textures
                                        [entry.pos as usize..(entry.pos + entry.len) as usize]
                                        .iter(),
                                )
                        {
                            // convert to idx_poly to polygon
                            let poly = idx_poly.transform(|c| geom_store.vertices[*c as usize]);
                            let orig_mat = poly_mat
                                .and_then(|idx| appearance_store.materials.get(idx as usize))
                                .unwrap_or(&default_material)
                                .clone();
                            let orig_tex = poly_tex
                                .and_then(|idx| appearance_store.textures.get(idx as usize));

                            let mat = Material {
                                base_color: orig_mat.diffuse_color.into(),
                                base_texture: orig_tex.map(|tex| Texture {
                                    uri: tex.image_url.clone(),
                                }),
                            };

                            let (mat_idx, _) = materials.insert_full(mat);

                            let mut ring_buffer: Vec<[f64; 5]> = Vec::new();

                            poly.rings().zip_eq(poly_uv.rings()).enumerate().for_each(
                                |(ri, (ring, uv_ring))| {
                                    ring.iter_closed().zip_eq(uv_ring.iter_closed()).for_each(
                                        |(c, uv)| {
                                            let [lng, lat, height] = c;
                                            ring_buffer.push([lng, lat, height, uv[0], uv[1]]);

                                            local_bvol.min_lng = local_bvol.min_lng.min(lng);
                                            local_bvol.max_lng = local_bvol.max_lng.max(lng);
                                            local_bvol.min_lat = local_bvol.min_lat.min(lat);
                                            local_bvol.max_lat = local_bvol.max_lat.max(lat);
                                            local_bvol.min_height =
                                                local_bvol.min_height.min(height);
                                            local_bvol.max_height =
                                                local_bvol.max_height.max(height);
                                        },
                                    );
                                    if ri == 0 {
                                        feature.polygons.add_exterior(ring_buffer.drain(..));
                                        feature.polygon_material_ids.push(mat_idx as u32);
                                    } else {
                                        feature.polygons.add_interior(ring_buffer.drain(..));
                                    }
                                },
                            );
                        }
                    }
                    GeometryType::Curve => {
                        // TODO: implement
                    }
                    GeometryType::Point => {
                        // TODO: implement
                    }
                }
            });

            feature.materials = materials;

            {
                let mut locked_features = classified_features.lock().unwrap();
                let feats = locked_features.entry(obj.typename.to_string()).or_default();
                feats.features.push(feature);
                feats.bounding_volume.update(&local_bvol);
            }

            Ok::<(), PipelineError>(())
        });

        let classified_features = classified_features.into_inner().unwrap();

        // Bounding volume for the entire dataset
        let global_bvol = {
            let mut global_bvol = BoundingVolume::default();
            for features in classified_features.values() {
                global_bvol.update(&features.bounding_volume);
            }
            global_bvol
        };

        // Transformation matrix to convert geodetic coordinates to geocentric and offset to the center
        let transform_matrix = {
            let bounds = &global_bvol;
            let center_lng = (bounds.min_lng + bounds.max_lng) / 2.0;
            let center_lat = (bounds.min_lat + bounds.max_lat) / 2.0;

            let psi = ((1. - ellipsoid.e_sq()) * center_lat.to_radians().tan()).atan();

            let (tx, ty, tz) = geodetic_to_geocentric(&ellipsoid, center_lng, center_lat, 0.);
            let h = (tx * tx + ty * ty + tz * tz).sqrt();

            DMat4::from_translation(DVec3::new(0., -h, 0.))
                * DMat4::from_rotation_x(-(FRAC_PI_2 - psi))
                * DMat4::from_rotation_y((-center_lng - 90.).to_radians())
        };
        let _ = transform_matrix.inverse();

        let duration = preprocessing_start.elapsed();
        feedback.info(format!("preprocessing {:?}", duration));

        // Create the information needed to output an OBJ file and write it to a file
        classified_features
            .into_par_iter()
            .try_for_each(|(typename, mut features)| {
                feedback.ensure_not_canceled()?;

                // Texture cache
                let texture_cache = TextureCache::new(100_000_000);

                // file output destination
                let mut folder_path = self.output_path.clone();
                let base_folder_name = typename.replace(':', "_").to_string();
                folder_path.push(&base_folder_name);

                let texture_folder_name = "textures";
                let atlas_dir = folder_path.join(texture_folder_name);
                std::fs::create_dir_all(&atlas_dir)?;

                // initialize texture packer
                let config = TexturePlacerConfig {
                    width: 4096,
                    height: 4096,
                    padding: 0,
                };
                let placer = GuillotineTexturePlacer::new(config.clone());
                let exporter = JpegAtlasExporter::default();
                let ext = exporter.clone().get_extension().to_string();
                // todo: 並列処理出来る機構を考える
                let packer = Mutex::new(TexturePacker::new(placer, exporter));

                let atlas_packing_start = Instant::now();

                // Coordinate transformation
                {
                    for feature in features.features.iter_mut() {
                        feedback.ensure_not_canceled()?;

                        feature
                            .polygons
                            .transform_inplace(|&[lng, lat, height, u, v]| {
                                let (x, y, z) =
                                    geodetic_to_geocentric(&ellipsoid, lng, lat, height);
                                let v_xyz = DVec4::new(x, z, -y, 1.0);
                                let v_enu = transform_matrix * v_xyz;
                                [v_enu[0], v_enu[1], v_enu[2], u, v]
                            });
                    }
                }

                let features = features.features.iter().collect::<Vec<_>>();
                let chunk_num = features.len() / 8;

                // parallel processing
                // generate texture atlas and update materials
                let (mesh_sender, mesh_receiver) = mpsc::channel();
                let (material_sender, material_receiver) = mpsc::channel();
                features.par_chunks(chunk_num).for_each_with(
                    (mesh_sender, material_sender),
                    |(mesh_sender, material_sender), chunk| {
                        for feature in chunk {
                            let mut feature_mesh = FeatureMesh {
                                vertices: Vec::new(),
                                uvs: Vec::new(),
                                primitives: HashMap::new(),
                            };

                            for (poly_count, (mut poly, &orig_mat_id)) in feature
                                .polygons
                                .iter()
                                .zip_eq(feature.polygon_material_ids.iter())
                                .enumerate()
                            {
                                let mut new_mat = feature.materials[orig_mat_id as usize].clone();
                                let t = new_mat.base_texture.clone();
                                if let Some(base_texture) = t {
                                    // textured
                                    let original_vertices = poly
                                        .raw_coords()
                                        .iter()
                                        .map(|[x, y, z, u, v]| (*x, *y, *z, *u, *v))
                                        .collect::<Vec<(f64, f64, f64, f64, f64)>>();

                                    let texture = texture_cache.get_or_insert(
                                        &original_vertices
                                            .iter()
                                            .map(|(_, _, _, u, v)| (*u, *v))
                                            .collect::<Vec<_>>(),
                                        &base_texture.uri.to_file_path().unwrap(),
                                        &DownsampleFactor::new(&1.0).value(),
                                    );

                                    // Unique id required for placement in atlas
                                    let texture_id = format!(
                                        "{}_{}_{}",
                                        base_folder_name, feature.feature_id, poly_count
                                    );
                                    let info =
                                        packer.lock().unwrap().add_texture(texture_id, texture);

                                    let atlas_placed_uv_coords = info
                                        .placed_uv_coords
                                        .iter()
                                        .map(|(u, v)| ({ *u }, { *v }))
                                        .collect::<Vec<(f64, f64)>>();
                                    let updated_vertices = original_vertices
                                        .iter()
                                        .zip(atlas_placed_uv_coords.iter())
                                        .map(|((x, y, z, _, _), (u, v))| (*x, *y, *z, *u, *v))
                                        .collect::<Vec<(f64, f64, f64, f64, f64)>>();

                                    // Apply the UV coordinates placed in the atlas to the original polygon
                                    poly.transform_inplace(|&[x, y, z, _, _]| {
                                        let (u, v) = updated_vertices
                                            .iter()
                                            .find(|(x_, y_, z_, _, _)| {
                                                (*x_ - x).abs() < 1e-6
                                                    && (*y_ - y).abs() < 1e-6
                                                    && (*z_ - z).abs() < 1e-6
                                            })
                                            .map(|(_, _, _, u, v)| (*u, *v))
                                            .unwrap();
                                        [x, y, z, u, v]
                                    });

                                    let atlas_file_name = info.atlas_id.to_string();

                                    let atlas_uri =
                                        atlas_dir.join(atlas_file_name).with_extension(ext.clone());

                                    // update material
                                    new_mat = material::Material {
                                        base_color: new_mat.base_color,
                                        base_texture: Some(material::Texture {
                                            uri: Url::from_file_path(atlas_uri).unwrap(),
                                        }),
                                    };
                                }

                                let poly_material = new_mat;
                                let poly_color = poly_material.base_color;
                                let poly_texture = poly_material.base_texture.as_ref();
                                let texture_name = poly_texture.map_or_else(
                                    || "".to_string(),
                                    |t| {
                                        t.uri
                                            .to_file_path()
                                            .unwrap()
                                            .file_stem()
                                            .unwrap()
                                            .to_str()
                                            .unwrap()
                                            .to_string()
                                    },
                                );
                                let poly_material_key =
                                    poly_material.base_texture.as_ref().map_or_else(
                                        || {
                                            format!(
                                                "material_{}_{}_{}",
                                                poly_color[0], poly_color[1], poly_color[2]
                                            )
                                        },
                                        |_| {
                                            format!(
                                                "{}_{}_{}",
                                                base_folder_name, texture_folder_name, texture_name
                                            )
                                        },
                                    );

                                // all_materials.insert(
                                //     poly_material_key.clone(),
                                //     FeatureMaterial {
                                //         base_color: poly_color,
                                //         texture_uri: poly_texture.map(|t| t.uri.clone()),
                                //     },
                                // );
                                material_sender
                                    .send((
                                        poly_material_key.clone(),
                                        FeatureMaterial {
                                            base_color: poly_color,
                                            texture_uri: poly_texture.map(|t| t.uri.clone()),
                                        },
                                    ))
                                    .unwrap();

                                let num_outer = match poly.hole_indices().first() {
                                    Some(&v) => v as usize,
                                    None => poly.raw_coords().len(),
                                };
                                let mut earcutter = Earcut::new();
                                let mut buf3d: Vec<[f64; 3]> = Vec::new();
                                let mut buf2d: Vec<[f64; 2]> = Vec::new();
                                let mut index_buf: Vec<u32> = Vec::new();

                                buf3d.clear();
                                buf3d.extend(
                                    poly.raw_coords().iter().map(|&[x, y, z, _, _]| [x, y, z]),
                                );

                                if project3d_to_2d(&buf3d, num_outer, &mut buf2d) {
                                    earcutter.earcut(
                                        buf2d.iter().cloned(),
                                        poly.hole_indices(),
                                        &mut index_buf,
                                    );
                                    feature_mesh
                                        .primitives
                                        .entry(poly_material_key.clone())
                                        .or_default()
                                        .extend(index_buf.iter().map(|&idx| {
                                            let [x, y, z, u, v] = poly.raw_coords()[idx as usize];
                                            feature_mesh.vertices.push([x, y, z]);
                                            feature_mesh.uvs.push([u, v]);
                                            (feature_mesh.vertices.len() - 1) as u32
                                        }));
                                }
                            }
                            // all_meshes.insert(feature.feature_id.clone(), feature_mesh);
                            mesh_sender
                                .send((feature.feature_id.clone(), feature_mesh))
                                .unwrap();
                        }
                    },
                );

                // {
                //     for feature in features.iter_mut() {
                //         feedback.ensure_not_canceled()?;

                //         let mut feature_mesh = FeatureMesh {
                //             vertices: Vec::new(),
                //             uvs: Vec::new(),
                //             primitives: HashMap::new(),
                //         };

                //         for (poly_count, (mut poly, &orig_mat_id)) in feature
                //             .polygons
                //             .iter()
                //             .zip_eq(feature.polygon_material_ids.iter())
                //             .enumerate()
                //         {
                //             let mut new_mat = feature.materials[orig_mat_id as usize].clone();
                //             let t = new_mat.base_texture.clone();
                //             if let Some(base_texture) = t {
                //                 // textured
                //                 let original_vertices = poly
                //                     .raw_coords()
                //                     .iter()
                //                     .map(|[x, y, z, u, v]| (*x, *y, *z, *u, *v))
                //                     .collect::<Vec<(f64, f64, f64, f64, f64)>>();

                //                 let texture = texture_cache.get_or_insert(
                //                     &original_vertices
                //                         .iter()
                //                         .map(|(_, _, _, u, v)| (*u, *v))
                //                         .collect::<Vec<_>>(),
                //                     &base_texture.uri.to_file_path().unwrap(),
                //                     &DownsampleFactor::new(&1.0).value(),
                //                 );

                //                 // Unique id required for placement in atlas
                //                 let texture_id = format!(
                //                     "{}_{}_{}",
                //                     base_folder_name, feature.feature_id, poly_count
                //                 );
                //                 let info = packer.add_texture(texture_id, texture);

                //                 let atlas_placed_uv_coords = info
                //                     .placed_uv_coords
                //                     .iter()
                //                     .map(|(u, v)| ({ *u }, { *v }))
                //                     .collect::<Vec<(f64, f64)>>();
                //                 let updated_vertices = original_vertices
                //                     .iter()
                //                     .zip(atlas_placed_uv_coords.iter())
                //                     .map(|((x, y, z, _, _), (u, v))| (*x, *y, *z, *u, *v))
                //                     .collect::<Vec<(f64, f64, f64, f64, f64)>>();

                //                 // Apply the UV coordinates placed in the atlas to the original polygon
                //                 poly.transform_inplace(|&[x, y, z, _, _]| {
                //                     let (u, v) = updated_vertices
                //                         .iter()
                //                         .find(|(x_, y_, z_, _, _)| {
                //                             (*x_ - x).abs() < 1e-6
                //                                 && (*y_ - y).abs() < 1e-6
                //                                 && (*z_ - z).abs() < 1e-6
                //                         })
                //                         .map(|(_, _, _, u, v)| (*u, *v))
                //                         .unwrap();
                //                     [x, y, z, u, v]
                //                 });

                //                 let atlas_file_name = info.atlas_id.to_string();

                //                 let atlas_uri =
                //                     atlas_dir.join(atlas_file_name).with_extension(ext.clone());

                //                 // update material
                //                 new_mat = material::Material {
                //                     base_color: new_mat.base_color,
                //                     base_texture: Some(material::Texture {
                //                         uri: Url::from_file_path(atlas_uri).unwrap(),
                //                     }),
                //                 };
                //             }

                //             let poly_material = new_mat;
                //             let poly_color = poly_material.base_color;
                //             let poly_texture = poly_material.base_texture.as_ref();
                //             let texture_name = poly_texture.map_or_else(
                //                 || "".to_string(),
                //                 |t| {
                //                     t.uri
                //                         .to_file_path()
                //                         .unwrap()
                //                         .file_stem()
                //                         .unwrap()
                //                         .to_str()
                //                         .unwrap()
                //                         .to_string()
                //                 },
                //             );
                //             let poly_material_key =
                //                 poly_material.base_texture.as_ref().map_or_else(
                //                     || {
                //                         format!(
                //                             "material_{}_{}_{}",
                //                             poly_color[0], poly_color[1], poly_color[2]
                //                         )
                //                     },
                //                     |_| {
                //                         format!(
                //                             "{}_{}_{}",
                //                             base_folder_name, texture_folder_name, texture_name
                //                         )
                //                     },
                //                 );

                //             all_materials.insert(
                //                 poly_material_key.clone(),
                //                 FeatureMaterial {
                //                     base_color: poly_color,
                //                     texture_uri: poly_texture.map(|t| t.uri.clone()),
                //                 },
                //             );

                //             let num_outer = match poly.hole_indices().first() {
                //                 Some(&v) => v as usize,
                //                 None => poly.raw_coords().len(),
                //             };
                //             let mut earcutter = Earcut::new();
                //             let mut buf3d: Vec<[f64; 3]> = Vec::new();
                //             let mut buf2d: Vec<[f64; 2]> = Vec::new();
                //             let mut index_buf: Vec<u32> = Vec::new();

                //             buf3d.clear();
                //             buf3d
                //                 .extend(poly.raw_coords().iter().map(|&[x, y, z, _, _]| [x, y, z]));

                //             if project3d_to_2d(&buf3d, num_outer, &mut buf2d) {
                //                 earcutter.earcut(
                //                     buf2d.iter().cloned(),
                //                     poly.hole_indices(),
                //                     &mut index_buf,
                //                 );
                //                 feature_mesh
                //                     .primitives
                //                     .entry(poly_material_key.clone())
                //                     .or_default()
                //                     .extend(index_buf.iter().map(|&idx| {
                //                         let [x, y, z, u, v] = poly.raw_coords()[idx as usize];
                //                         feature_mesh.vertices.push([x, y, z]);
                //                         feature_mesh.uvs.push([u, v]);
                //                         (feature_mesh.vertices.len() - 1) as u32
                //                     }));
                //             }
                //         }
                //         all_meshes.insert(feature.feature_id.clone(), feature_mesh);
                //     }
                // }

                let mut packer = packer.into_inner().unwrap();
                packer.finalize();

                // receive mesh and material
                let mut all_meshes = ObjInfo::new();
                let mut all_materials = ObjMaterials::new();
                for d in mesh_receiver.iter() {
                    let (feature_id, feature_mesh) = d;
                    all_meshes.insert(feature_id, feature_mesh);
                }
                for d in material_receiver.iter() {
                    let (material_key, feature_material) = d;
                    all_materials.insert(material_key, feature_material);
                }

                let duration = atlas_packing_start.elapsed();
                feedback.info(format!("atlas packing process {:?}", duration));

                let atlas_export_start = Instant::now();

                packer.export(&atlas_dir, &texture_cache, config.width, config.height);

                let duration = atlas_export_start.elapsed();
                feedback.info(format!("atlas export process {:?}", duration));

                feedback.ensure_not_canceled()?;

                let obj_export_start = Instant::now();

                // Write OBJ file
                write(
                    all_meshes,
                    all_materials,
                    folder_path,
                    self.obj_options.is_split,
                )?;

                let duration = obj_export_start.elapsed();
                feedback.info(format!("obj export process {:?}", duration));

                Ok::<(), PipelineError>(())
            })?;

        Ok(())
    }
}