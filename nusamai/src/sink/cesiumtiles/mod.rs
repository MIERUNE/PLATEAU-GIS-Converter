//! 3D Tiles sink

mod slice;
mod sort;
mod tiling;

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::mpsc;

use ext_sort::{buffer::mem::MemoryLimitedBufferBuilder, ExternalSorter, ExternalSorterBuilder};
use hashbrown::HashMap;
use itertools::Itertools;
use rayon::prelude::*;
use serde::{Deserialize, Serialize};

use nusamai_citygml::object;
use nusamai_citygml::schema::Schema;
use nusamai_geometry::MultiPolygon;
use nusamai_mvt::geometry::GeometryEncoder;
use nusamai_mvt::tag::TagsEncoder;
use nusamai_mvt::tileid::TileIdMethod;

use crate::parameters::*;
use crate::pipeline::{Feedback, Receiver};
use crate::sink::{DataSink, DataSinkProvider, SinkInfo};
use crate::{get_parameter_value, transformer};
use slice::slice_cityobj_geoms;
use sort::BincodeExternalChunk;

pub struct MVTSinkProvider {}

impl DataSinkProvider for MVTSinkProvider {
    fn info(&self) -> SinkInfo {
        SinkInfo {
            name: "Vector Tiles (MVT)".to_string(),
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
            },
        );
        // TODO: min Zoom
        // TODO: max Zoom
        params
    }

    fn create(&self, params: &Parameters) -> Box<dyn DataSink> {
        let output_path = get_parameter_value!(params, "@output", FileSystemPath);

        Box::<MVTSink>::new(MVTSink {
            output_path: output_path.as_ref().unwrap().into(),
        })
    }
}

struct MVTSink {
    output_path: PathBuf,
}

#[derive(Serialize, Deserialize, deepsize::DeepSizeOf)]
struct SerializedSlicedFeature {
    tile_id: u64,
    #[serde(with = "serde_bytes")]
    body: Vec<u8>,
}

#[derive(Serialize, Deserialize)]
struct SlicedFeature<'a> {
    geometry: MultiPolygon<'a, 3>,
    properties: nusamai_citygml::object::Value,
}

impl DataSink for MVTSink {
    fn make_transform_requirements(&self) -> transformer::Requirements {
        use transformer::RequirementItem;

        transformer::Requirements {
            mergedown: RequirementItem::Recommended(transformer::Mergedown::Full),
            ..Default::default()
        }
    }

    fn run(&mut self, upstream: Receiver, feedback: &Feedback, _schema: &Schema) {
        let (sender_sliced, receiver_sliced) = mpsc::sync_channel(2000);
        let (sender_sorted, receiver_sorted) = mpsc::sync_channel(2000);

        let tile_id_conv = TileIdMethod::Hilbert;

        // TODO: refactoring

        std::thread::scope(|s| {
            // Slicing geometry along the tile boundaries
            {
                let feedback = feedback.clone();
                s.spawn(move || {
                    geometry_slicing_stage(feedback, upstream, tile_id_conv, sender_sliced);
                });
            }

            // Sort features by tile_id (using external sorter)
            {
                s.spawn(move || {
                    feature_sorting_stage(receiver_sliced, sender_sorted);
                });
            }

            // Group sorted features and write them into MVT tiles
            {
                let feedback = feedback.clone();
                let output_path = &self.output_path;
                s.spawn(move || {
                    // Run in a separate thread pool to avoid deadlocks
                    let pool = rayon::ThreadPoolBuilder::new()
                        .use_current_thread()
                        .build()
                        .unwrap();
                    pool.install(|| {
                        tile_writing_stage(output_path, feedback, receiver_sorted, tile_id_conv);
                    })
                });
            }
        });
    }
}

fn geometry_slicing_stage(
    feedback: Feedback,
    upstream: mpsc::Receiver<crate::pipeline::Parcel>,
    tile_id_conv: TileIdMethod,
    sender_sliced: mpsc::SyncSender<SerializedSlicedFeature>,
) {
    // Convert CityObjects to sliced features
    let _ = upstream.into_iter().par_bridge().try_for_each(|parcel| {
        if feedback.is_cancelled() {
            return Err(());
        }

        let max_detail = 12; // 4096
        let buffer_pixels = 5;
        slice_cityobj_geoms(
            &parcel.entity,
            7,
            16,
            max_detail,
            buffer_pixels,
            |(z, x, y), mpoly| {
                let feature = SlicedFeature {
                    geometry: mpoly,
                    properties: parcel.entity.root.clone(),
                };
                let bytes = bincode::serialize(&feature).unwrap();
                let sfeat = SerializedSlicedFeature {
                    tile_id: tile_id_conv.zxy_to_id(z, x, y),
                    body: bytes,
                };

                if sender_sliced.send(sfeat).is_err() {
                    log::info!("sink cancelled");
                    return Err(());
                };
                Ok(())
            },
        )
    });
}

fn feature_sorting_stage(
    receiver_sliced: mpsc::Receiver<SerializedSlicedFeature>,
    sender_sorted: mpsc::SyncSender<(u64, Vec<SerializedSlicedFeature>)>,
) {
    let sorter: ExternalSorter<
        SerializedSlicedFeature,
        std::io::Error,
        MemoryLimitedBufferBuilder,
        BincodeExternalChunk<_>,
        // TODO: Use Binpack instead of RMP ?
        // TODO: Implement an external sorter by ourselves?
    > = ExternalSorterBuilder::new()
        .with_tmp_dir(Path::new("./"))
        .with_buffer(MemoryLimitedBufferBuilder::new(200 * 1024 * 1024)) // TODO
        .with_threads_number(8) // TODO
        .build()
        .unwrap();
    let sorted = sorter
        .sort_by(receiver_sliced.into_iter().map(Ok), |a, b| {
            a.tile_id.cmp(&b.tile_id)
        })
        .unwrap();

    for (tile_id, ser_feats) in &sorted
        .map(Result::unwrap)
        .group_by(|ser_feat| ser_feat.tile_id)
    {
        let ser_feats: Vec<_> = ser_feats.collect();
        if sender_sorted.send((tile_id, ser_feats)).is_err() {
            log::info!("sink cancelled?");
            return;
        };
    }
}

fn tile_writing_stage(
    output_path: &Path,
    feedback: Feedback,
    receiver_sorted: mpsc::Receiver<(u64, Vec<SerializedSlicedFeature>)>,
    tile_id_conv: TileIdMethod,
) {
    let detail = 12;
    let extent = 2u32.pow(detail);

    let _ = receiver_sorted
        .into_iter()
        .par_bridge()
        .try_for_each(|(tile_id, sfeats)| {
            if feedback.is_cancelled() {
                return Err(());
            }
            let (zoom, x, y) = tile_id_conv.id_to_zxy(tile_id);

            // TODO:

            Ok(())
        });
}