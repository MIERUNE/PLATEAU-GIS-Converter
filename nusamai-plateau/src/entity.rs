use std::sync::{Arc, RwLock};

use nusamai_citygml::geometry::GeometryStore;
use nusamai_citygml::object::Value;

use crate::appearance::AppearanceStore;

/// City objects, features, objects or data
#[derive(Debug, serde::Deserialize, serde::Serialize)]
pub struct Entity {
    /// Attribute tree
    pub root: Value,
    /// All geometries referenced by the attribute tree
    pub geometry_store: Arc<RwLock<GeometryStore>>,
    /// All appearances used in this city object
    pub appearance_store: Arc<RwLock<AppearanceStore>>,
}