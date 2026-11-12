use crate::types::Value;

const SPATIAL_TYPES: [&str; 9] = [
    "geometry",
    "point",
    "linestring",
    "polygon",
    "multipoint",
    "multilinestring",
    "multipolygon",
    "geometrycollection",
    "geomcollection",
];

const FUNCTION_NAMES: [(&str, &str); 21] = [
    ("point", "st_makepoint"),
    ("st_pointfromtext", "st_geomfromtext"),
    ("st_linefromtext", "st_geomfromtext"),
    ("st_linestringfromtext", "st_geomfromtext"),
    ("st_polyfromtext", "st_geomfromtext"),
    ("st_polygonfromtext", "st_geomfromtext"),
    ("st_mpointfromtext", "st_geomfromtext"),
    ("st_multipointfromtext", "st_geomfromtext"),
    ("st_mlinefromtext", "st_geomfromtext"),
    ("st_mpolyfromtext", "st_geomfromtext"),
    ("st_geomcollfromtext", "st_geomfromtext"),
    ("st_geometryfromtext", "st_geomfromtext"),
    ("st_pointfromwkb", "st_geomfromwkb"),
    ("st_linefromwkb", "st_geomfromwkb"),
    ("st_polyfromwkb", "st_geomfromwkb"),
    ("st_geometryfromwkb", "st_geomfromwkb"),
    ("st_aswkb", "st_asbinary"),
    ("st_aswkt", "st_astext"),
    ("st_distance_sphere", "st_distancesphere"),
    ("st_latitude", "st_y"),
    ("st_longitude", "st_x"),
];

/// Whether `word` (lower case) is one of MySQL's spatial column types, all stored as the
/// `geometry` type (D845).
pub(crate) fn is_spatial_type(word: &str) -> bool {
    SPATIAL_TYPES.contains(&word)
}

/// The PostGIS-compatible function a MySQL spatial function name stands for, when the two
/// differ only in name.
pub(crate) fn postgis_name(name: &str) -> Option<&'static str> {
    FUNCTION_NAMES
        .iter()
        .find(|(mysql, _)| *mysql == name)
        .map(|(_, postgis)| *postgis)
}

/// A geometry value as MySQL's own clients receive it: the SRID as four little-endian bytes,
/// then the geometry's little-endian well-known binary.
pub fn mysql_geometry_bytes(value: &Value) -> Option<Vec<u8>> {
    let ewkb = crate::geometry_type::geometry_of(value)?;
    let spatial = eggdb_geo::from_wkb(ewkb).ok()?;
    let srid = u32::try_from(spatial.srid.unwrap_or(0)).unwrap_or(0);
    let mut out = srid.to_le_bytes().to_vec();
    out.extend_from_slice(&eggdb_geo::to_wkb(&spatial, false));
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mysql_geometry_starts_with_its_srid() {
        let ewkb = crate::geometry_type::parse_text("SRID=4326;POINT(1 2)").unwrap();
        let bytes = mysql_geometry_bytes(&crate::geometry_type::value(ewkb)).unwrap();
        assert_eq!(&bytes[..4], &4326u32.to_le_bytes());
        assert_eq!(bytes.len(), 4 + 21);
        assert_eq!(postgis_name("st_aswkt"), Some("st_astext"));
        assert!(is_spatial_type("multipolygon"));
    }
}
