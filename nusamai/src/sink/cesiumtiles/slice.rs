//! Polygon slicing algorithm based on [geojson-vt](https://github.com/mapbox/geojson-vt).

use ahash::HashSet;
use flatgeom::{MultiPolygon, Polygon, Polygon2, Polygon3};
use hashbrown::HashMap;
use indexmap::IndexSet;
use itertools::Itertools;
use nusamai_citygml::{
    geometry::GeometryType,
    object::{ObjectStereotype, Value},
};
use nusamai_plateau::{appearance, Entity};
use serde::{Deserialize, Serialize};
use tinymvt::TileZXY;

use super::{material::Material, tiling};
use crate::sink::cesiumtiles::{material::Texture, tiling::zxy_from_lng_lat};

#[derive(Serialize, Deserialize)]
pub struct SlicedFeature {
    // polygons [x, y, z, u, v]
    pub polygons: MultiPolygon<'static, [f64; 5]>,
    // material ids for each polygon
    pub polygon_material_ids: Vec<u32>,
    // materials
    pub materials: IndexSet<Material>,
    // attribute values
    pub attributes: nusamai_citygml::object::Value,
}

pub fn slice_to_tiles<E>(
    entity: &Entity,
    min_zoom: u8,
    max_zoom: u8,
    send_feature: impl Fn(TileZXY, SlicedFeature) -> Result<(), E>,
) -> Result<(), E> {
    let ellipsoid = nusamai_projection::ellipsoid::wgs84();

    assert!(
        max_zoom >= min_zoom,
        "max_z must be greater than or equal to min_z"
    );

    let slicing_enabled = true;

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

    let mut sliced_tiles: HashMap<(u8, u32, u32), SlicedFeature> = HashMap::new();
    let mut materials: IndexSet<Material> = IndexSet::new();
    let default_material = appearance::Material::default();

    let (lng_center, lat_center, approx_dx, approx_dy, approx_dh) = {
        let mut min_lng = f64::MAX;
        let mut max_lng = f64::MIN;
        let mut min_lat = f64::MAX;
        let mut max_lat = f64::MIN;
        let mut min_height = f64::MAX;
        let mut max_height = f64::MIN;
        geom_store.vertices.iter().for_each(|&[lng, lat, height]| {
            min_lng = min_lng.min(lng);
            max_lng = max_lng.max(lng);
            min_lat = min_lat.min(lat);
            max_lat = max_lat.max(lat);
            min_height = min_height.min(height);
            max_height = max_height.max(height);
        });
        let approx_dx =
            ellipsoid.a() * min_lat.to_radians().cos() * (max_lng - min_lng).to_radians();
        let approx_dy = ellipsoid.a() * (max_lng - min_lng).to_radians();
        let approx_dh = max_height - min_height;
        (
            (min_lng + max_lng) / 2.0,
            (min_lat + max_lat) / 2.0,
            approx_dx,
            approx_dy,
            approx_dh,
        )
    };
    let mut ring_buffer: Vec<[f64; 5]> = Vec::new();

    let available_lods: HashSet<u8> = geometries
        .iter()
        .map(|entry| entry.lod)
        .sorted()
        .dedup()
        .collect();

    geometries.iter().for_each(|entry| {
        match entry.ty {
            GeometryType::Solid | GeometryType::Surface | GeometryType::Triangle => {
                // for each polygon
                for (((idx_poly, poly_uv), poly_mat), poly_tex) in geom_store
                    .multipolygon
                    .iter_range(entry.pos as usize..(entry.pos + entry.len) as usize)
                    .zip_eq(
                        geom_store
                            .polygon_uvs
                            .iter_range(entry.pos as usize..(entry.pos + entry.len) as usize),
                    )
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
                    let poly = idx_poly.transform(|c| geom_store.vertices[*c as usize]);
                    let orig_mat = poly_mat
                        .and_then(|idx| appearance_store.materials.get(idx as usize))
                        .unwrap_or(&default_material)
                        .clone();
                    let orig_tex =
                        poly_tex.and_then(|idx| appearance_store.textures.get(idx as usize));

                    let mat = Material {
                        base_color: orig_mat.diffuse_color.into(),
                        base_texture: orig_tex.map(|tex| Texture {
                            uri: tex.image_url.clone(),
                        }),
                    };
                    let (mat_idx, _) = materials.insert_full(mat);

                    // Slice polygon for each zoom level
                    for zoom in min_zoom..=max_zoom {
                        if zoom <= max_zoom {
                            let geom_error = {
                                let (_, _, y) =
                                    tiling::scheme::zxy_from_lng_lat(zoom, lng_center, lat_center);
                                tiling::scheme::geometric_error(zoom, y)
                            };

                            // If you have multiple LODs, extract the appropriate LOD according to the geometricError.
                            // This works when the "All LOD" option is used.
                            if !should_process_entry(entry.lod, geom_error, &available_lods) {
                                continue;
                            }

                            // Skip the feature if the size is small for geometricError.
                            let threshold = geom_error * 0.5;
                            if approx_dx < threshold
                                && approx_dy < threshold
                                && approx_dh < threshold
                            {
                                continue;
                            }
                        }

                        if slicing_enabled {
                            // slicing enabled
                            slice_polygon(zoom, &poly, &poly_uv, |(z, x, y), poly| {
                                let sliced_feature =
                                    sliced_tiles.entry((z, x, y)).or_insert_with(|| {
                                        SlicedFeature {
                                            polygons: MultiPolygon::new(),
                                            attributes: entity.root.clone(),
                                            polygon_material_ids: Default::default(),
                                            materials: Default::default(), // set later
                                        }
                                    });
                                sliced_feature.polygons.push(poly);
                                sliced_feature.polygon_material_ids.push(mat_idx as u32);
                            });
                        } else {
                            // slicing disabled
                            let (z, x, y) = zxy_from_lng_lat(zoom, lng_center, lat_center);
                            let sliced_feature =
                                sliced_tiles
                                    .entry((z, x, y))
                                    .or_insert_with(|| SlicedFeature {
                                        polygons: MultiPolygon::new(),
                                        attributes: entity.root.clone(),
                                        polygon_material_ids: Default::default(),
                                        materials: Default::default(), // set later
                                    });
                            poly.rings().zip_eq(poly_uv.rings()).enumerate().for_each(
                                |(ri, (ring, uv_ring))| {
                                    ring.iter_closed().zip_eq(uv_ring.iter_closed()).for_each(
                                        |(c, uv)| {
                                            ring_buffer.push([c[0], c[1], c[2], uv[0], uv[1]]);
                                        },
                                    );
                                    if ri == 0 {
                                        sliced_feature.polygons.add_exterior(ring_buffer.drain(..));
                                        sliced_feature.polygon_material_ids.push(mat_idx as u32);
                                    } else {
                                        sliced_feature.polygons.add_interior(ring_buffer.drain(..));
                                    }
                                },
                            );
                        }
                    }
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

    // Send tiled features
    for ((z, x, y), mut sliced_feature) in sliced_tiles {
        sliced_feature.materials.clone_from(&materials);
        send_feature((z, x, y), sliced_feature)?;
    }
    Ok(())

    // TODO: linestring, point
}

/// Slice a polygon into tiles. The slicing algorithm is based on [geojson-vt](https://github.com/mapbox/geojson-vt).
fn slice_polygon(
    zoom: u8,
    poly: &Polygon3,
    poly_uv: &Polygon2,
    mut send_polygon: impl FnMut(TileZXY, &Polygon<'static, [f64; 5]>),
) {
    if poly.exterior().is_empty() {
        return;
    }

    let mut ring_buffer: Vec<[f64; 5]> = Vec::with_capacity(poly.exterior().len() + 1);

    // Slice along Y-axis
    let y_range = {
        let (min_y, max_y) = poly
            .exterior()
            .iter()
            .fold((f64::MAX, f64::MIN), |(min_y, max_y), c| {
                (min_y.min(c[1]), max_y.max(c[1]))
            });
        tiling::iter_y_slice(zoom, min_y, max_y)
    };

    let mut y_sliced_polys = MultiPolygon::new();

    for yi in y_range.clone() {
        let (k1, k2) = tiling::y_slice_range(zoom, yi);

        for (ri, (ring, uv_ring)) in poly.rings().zip_eq(poly_uv.rings()).enumerate() {
            if ring.raw_coords().is_empty() {
                continue;
            }

            ring_buffer.clear();
            ring.iter_closed()
                .zip_eq(uv_ring.iter_closed())
                .fold(None, |a, b| {
                    let Some((a, a_uv)) = a else { return Some(b) };
                    let (b, b_uv) = b;

                    if a[1] < k1 {
                        if b[1] > k1 {
                            let t = (k1 - a[1]) / (b[1] - a[1]);
                            let x = (b[0] - a[0]) * t + a[0];
                            let z = (b[2] - a[2]) * t + a[2];
                            let u = (b_uv[0] - a_uv[0]) * t + a_uv[0];
                            let v = (b_uv[1] - a_uv[1]) * t + a_uv[1];
                            ring_buffer.push([x, k1, z, u, v])
                        }
                    } else if a[1] > k2 {
                        if b[1] < k2 {
                            let t = (k2 - a[1]) / (b[1] - a[1]);
                            let x = (b[0] - a[0]) * t + a[0];
                            let z = (b[2] - a[2]) * t + a[2];
                            let u = (b_uv[0] - a_uv[0]) * t + a_uv[0];
                            let v = (b_uv[1] - a_uv[1]) * t + a_uv[1];
                            ring_buffer.push([x, k2, z, u, v])
                        }
                    } else {
                        ring_buffer.push([a[0], a[1], a[2], a_uv[0], a_uv[1]])
                    }

                    if b[1] < k1 && a[1] > k1 {
                        let t = (k1 - a[1]) / (b[1] - a[1]);
                        let x = (b[0] - a[0]) * t + a[0];
                        let z = (b[2] - a[2]) * t + a[2];
                        let u = (b_uv[0] - a_uv[0]) * t + a_uv[0];
                        let v = (b_uv[1] - a_uv[1]) * t + a_uv[1];
                        ring_buffer.push([x, k1, z, u, v])
                    } else if b[1] > k2 && a[1] < k2 {
                        let t = (k2 - a[1]) / (b[1] - a[1]);
                        let x = (b[0] - a[0]) * t + a[0];
                        let z = (b[2] - a[2]) * t + a[2];
                        let u = (b_uv[0] - a_uv[0]) * t + a_uv[0];
                        let v = (b_uv[1] - a_uv[1]) * t + a_uv[1];
                        ring_buffer.push([x, k2, z, u, v])
                    }

                    Some((b, b_uv))
                })
                .unwrap();

            match ri {
                0 => y_sliced_polys.add_exterior(ring_buffer.drain(..)),
                _ => y_sliced_polys.add_interior(ring_buffer.drain(..)),
            }
        }
    }

    // Slice along X-axis
    let mut poly_buf: Polygon<[f64; 5]> = Polygon::new();
    for (yi, y_sliced_poly) in y_range.zip_eq(y_sliced_polys.iter()) {
        let x_iter = {
            let (min_x, max_x) = y_sliced_poly
                .exterior()
                .iter()
                .fold((f64::MAX, f64::MIN), |(min_x, max_x), c| {
                    (min_x.min(c[0]), max_x.max(c[0]))
                });

            tiling::iter_x_slice(zoom, yi, min_x, max_x)
        };

        for (xi, xs) in x_iter {
            let (k1, k2) = tiling::x_slice_range(zoom, xi, xs);

            let key = (
                zoom,
                xi.rem_euclid(1 << zoom) as u32, // handling geometry crossing the antimeridian
                yi,
            );
            poly_buf.clear();

            for ring in y_sliced_poly.rings() {
                if ring.raw_coords().is_empty() {
                    continue;
                }

                ring_buffer.clear();
                ring.iter_closed()
                    .fold(None, |a, b| {
                        let Some(a) = a else { return Some(b) };

                        if a[0] < k1 {
                            if b[0] > k1 {
                                let t = (k1 - a[0]) / (b[0] - a[0]);
                                let y = (b[1] - a[1]) * t + a[1];
                                let z = (b[2] - a[2]) * t + a[2];
                                let u = (b[3] - a[3]) * t + a[3];
                                let v = (b[4] - a[4]) * t + a[4];
                                ring_buffer.push([k1, y, z, u, v])
                            }
                        } else if a[0] > k2 {
                            if b[0] < k2 {
                                let t = (k2 - a[0]) / (b[0] - a[0]);
                                let y = (b[1] - a[1]) * t + a[1];
                                let z = (b[2] - a[2]) * t + a[2];
                                let u = (b[3] - a[3]) * t + a[3];
                                let v = (b[4] - a[4]) * t + a[4];
                                ring_buffer.push([k2, y, z, u, v])
                            }
                        } else {
                            ring_buffer.push(a)
                        }

                        if b[0] < k1 && a[0] > k1 {
                            let t = (k1 - a[0]) / (b[0] - a[0]);
                            let y = (b[1] - a[1]) * t + a[1];
                            let z = (b[2] - a[2]) * t + a[2];
                            let u = (b[3] - a[3]) * t + a[3];
                            let v = (b[4] - a[4]) * t + a[4];
                            ring_buffer.push([k1, y, z, u, v])
                        } else if b[0] > k2 && a[0] < k2 {
                            let t = (k2 - a[0]) / (b[0] - a[0]);
                            let y = (b[1] - a[1]) * t + a[1];
                            let z = (b[2] - a[2]) * t + a[2];
                            let u = (b[3] - a[3]) * t + a[3];
                            let v = (b[4] - a[4]) * t + a[4];
                            ring_buffer.push([k2, y, z, u, v])
                        }

                        Some(b)
                    })
                    .unwrap();

                poly_buf.add_ring(ring_buffer.drain(..))
            }

            send_polygon(key, &poly_buf);
        }
    }
}

fn desired_lod(geom_error: f64) -> u8 {
    if geom_error >= 30.0 {
        1
    } else if geom_error >= 20.0 {
        2
    } else if geom_error >= 5.0 {
        3
    } else {
        4
    }
}

fn should_process_entry(entry_lod: u8, geom_error: f64, available_lods: &HashSet<u8>) -> bool {
    let desired_lod = desired_lod(geom_error);

    let possible_lods: Vec<u8> = available_lods
        .iter()
        .cloned()
        .filter(|&lod| lod >= desired_lod)
        .collect();

    if !possible_lods.is_empty() {
        let selected_lod = *possible_lods.iter().min().unwrap();
        entry_lod == selected_lod
    } else {
        let selected_lod = *available_lods.iter().max().unwrap();
        entry_lod == selected_lod
    }
}
