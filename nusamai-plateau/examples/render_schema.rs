use nusamai_citygml::{schema::Schema, CityGMLElement};
use nusamai_plateau::models::TopLevelCityObject;

use std::io;

fn main() {
    let mut schema = Schema::default();
    TopLevelCityObject::collect_schema(&mut schema);
    schema.types.sort_keys();
    serde_json::to_writer_pretty(io::stdout(), &schema).unwrap();
}