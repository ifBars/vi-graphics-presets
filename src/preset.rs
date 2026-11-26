use serde::{Deserialize, Serialize};
use std::collections::HashSet;
use thiserror::Error;

const SCHEMA_VERSION: u32 = 1;
const MAX_SETTINGS: usize = 16;
const ALLOWED_KEYS: &[&str] = &[
    "imposter.trees.imposterDistScale",
    "imposter.trees.SLODDistanceScale",
];

/// One float entry from the runtime settings table.
#[repr(C)]
#[derive(Clone, Copy, Debug)]
pub struct RawRow {
    pub hash: u32,
    pub value: f32,
}

/// The console profile used to select platform-specific rows.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Platform {
    SeriesS,
    SeriesX,
}

/// Validated preset produced by [`parse_preset`].
#[derive(Debug)]
pub struct Preset {
    pub id: String,
    /// Plan and report changes while preserving the original rows.
    pub dry_run: bool,
    settings: Vec<Setting>,
}

#[derive(Debug)]
struct Setting {
    key: &'static str,
    multiply: f32,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct PresetJson {
    schema_version: u32,
    id: String,
    dry_run: bool,
    settings: Vec<SettingJson>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct SettingJson {
    key: String,
    multiply: f64,
}

/// Whether the matching row uses a platform suffix or the base key.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Selection {
    ActiveSuffix,
    Base,
}

/// A resolved value and its location in the input table.
#[derive(Debug, PartialEq, Serialize)]
pub struct Change {
    pub index: usize,
    pub key: String,
    pub hash: u32,
    pub before: f32,
    pub after: f32,
    pub selection: Selection,
}

#[derive(Debug, Error)]
pub enum PresetError {
    #[error("invalid preset JSON: {0}")]
    Json(#[from] serde_json::Error),
    #[error("unsupported preset schema version {0}")]
    SchemaVersion(u32),
    #[error("preset id must contain non-whitespace characters")]
    EmptyId,
    #[error("expected 1 through {MAX_SETTINGS} settings, found {0}")]
    SettingsCount(usize),
    #[error("setting {0} is not in the supported allowlist")]
    UnsupportedKey(String),
    #[error("setting {0} appears more than once")]
    DuplicateKey(String),
    #[error(
        "setting {key} has multiplier {multiply}; expected a finite value from 0.25 through 1.0"
    )]
    InvalidMultiplier { key: String, multiply: f64 },
    #[error("no active or base row found for setting {0}")]
    MissingRow(String),
    #[error("setting {key} has multiple rows for hash {hash:#010x}")]
    DuplicateRow { key: String, hash: u32 },
    #[error("setting {key} has an invalid scale {value} at hash {hash:#010x}")]
    InvalidScale { key: String, hash: u32, value: f32 },
}

/// Validates a JSON preset and canonicalizes its allowlisted keys.
pub fn parse_preset(json: &str) -> Result<Preset, PresetError> {
    let preset: PresetJson = serde_json::from_str(json)?;
    if preset.schema_version != SCHEMA_VERSION {
        return Err(PresetError::SchemaVersion(preset.schema_version));
    }
    if preset.id.trim().is_empty() {
        return Err(PresetError::EmptyId);
    }
    if !(1..=MAX_SETTINGS).contains(&preset.settings.len()) {
        return Err(PresetError::SettingsCount(preset.settings.len()));
    }

    let mut seen = HashSet::with_capacity(preset.settings.len());
    let mut settings = Vec::with_capacity(preset.settings.len());
    for setting in preset.settings {
        let Some(&canonical_key) = ALLOWED_KEYS
            .iter()
            .find(|key| key.eq_ignore_ascii_case(&setting.key))
        else {
            return Err(PresetError::UnsupportedKey(setting.key));
        };
        if !seen.insert(canonical_key) {
            return Err(PresetError::DuplicateKey(setting.key));
        }
        if !(0.25..=1.0).contains(&setting.multiply) {
            return Err(PresetError::InvalidMultiplier {
                key: setting.key,
                multiply: setting.multiply,
            });
        }
        settings.push(Setting {
            key: canonical_key,
            multiply: setting.multiply as f32,
        });
    }

    Ok(Preset {
        id: preset.id,
        dry_run: preset.dry_run,
        settings,
    })
}

/// Hashes the base settings key with ASCII case folding.
pub fn hash_key(key: &str) -> u32 {
    joaat(0, key)
}

/// Hashes the requested console suffix using the finalized base hash as its seed.
pub fn active_hash(key: &str, platform: &Platform) -> u32 {
    let suffix = match platform {
        Platform::SeriesS => ".xbs",
        Platform::SeriesX => ".xbx",
    };
    // The title finalizes the base hash before hashing the platform suffix.
    joaat(hash_key(key), suffix)
}

fn joaat(seed: u32, key: &str) -> u32 {
    let mut hash = seed;
    for byte in key.bytes() {
        hash = hash.wrapping_add(u32::from(byte.to_ascii_lowercase()));
        hash = hash.wrapping_add(hash << 10);
        hash ^= hash >> 6;
    }
    hash = hash.wrapping_add(hash << 3);
    hash ^= hash >> 11;
    hash.wrapping_add(hash << 15)
}

/// Resolves every setting against the unsorted table without changing any rows.
/// The planner returns an error if any setting cannot be resolved and validated.
/// Callers should honor [`Preset::dry_run`] before applying the changes.
pub fn plan(
    preset: &Preset,
    rows: &[RawRow],
    platform: Platform,
) -> Result<Vec<Change>, PresetError> {
    preset
        .settings
        .iter()
        .map(|setting| plan_setting(setting, rows, platform))
        .collect()
}

fn plan_setting(
    setting: &Setting,
    rows: &[RawRow],
    platform: Platform,
) -> Result<Change, PresetError> {
    let active = active_hash(setting.key, &platform);
    let base = hash_key(setting.key);
    let (index, row, selection) = match unique_row(rows, active, setting.key)? {
        Some((index, row)) => (index, row, Selection::ActiveSuffix),
        None => {
            let (index, row) = unique_row(rows, base, setting.key)?
                .ok_or_else(|| PresetError::MissingRow(setting.key.to_owned()))?;
            (index, row, Selection::Base)
        }
    };
    let after = row.value * setting.multiply;
    if !row.value.is_finite() || row.value < 0.0 || !after.is_finite() {
        return Err(PresetError::InvalidScale {
            key: setting.key.to_owned(),
            hash: row.hash,
            value: row.value,
        });
    }
    Ok(Change {
        index,
        key: setting.key.to_owned(),
        hash: row.hash,
        before: row.value,
        after,
        selection,
    })
}

fn unique_row<'a>(
    rows: &'a [RawRow],
    hash: u32,
    key: &str,
) -> Result<Option<(usize, &'a RawRow)>, PresetError> {
    let mut matching = rows.iter().enumerate().filter(|(_, row)| row.hash == hash);
    let row = matching.next();
    if matching.next().is_some() {
        return Err(PresetError::DuplicateRow {
            key: key.to_owned(),
            hash,
        });
    }
    Ok(row)
}

#[cfg(test)]
mod tests {
    use super::*;

    const IMPOSTER: &str = "imposter.trees.imposterDistScale";
    const SLOD: &str = "imposter.trees.SLODDistanceScale";
    const SMOKE: &str = include_str!("../presets/smoke-tree-distance.json");
    const LOW: &str = include_str!("../presets/low-tree-distance.json");
    const INSPECT: &str = include_str!("../presets/inspect-tree-distance.json");

    fn row(hash: u32, value: f32) -> RawRow {
        RawRow { hash, value }
    }

    #[test]
    fn hashes_match_the_title_for_both_platforms() {
        for (key, base, series_s, series_x) in [
            (IMPOSTER, 0x7ccc83af, 0x363fc0f8, 0x3577bf68),
            (SLOD, 0x43253e9a, 0x28a7fdd1, 0x2b2602cd),
        ] {
            assert_eq!(hash_key(key), base);
            assert_eq!(hash_key(&key.to_ascii_uppercase()), base);
            assert_eq!(active_hash(key, &Platform::SeriesS), series_s);
            assert_eq!(active_hash(key, &Platform::SeriesX), series_x);
        }
    }

    #[test]
    fn chooses_the_requested_platform_before_the_base() {
        let preset = parse_preset(SMOKE).unwrap();
        let base = row(hash_key(SLOD), 0.5);
        let series_x = row(active_hash(SLOD, &Platform::SeriesX), 4.0);
        let series_s = row(active_hash(SLOD, &Platform::SeriesS), 2.0);
        let rows = [series_x, base, series_s, base];

        let s = &plan(&preset, &rows, Platform::SeriesS).unwrap()[0];
        assert_eq!(
            (s.index, s.before, s.after, s.selection),
            (2, 2.0, 1.5, Selection::ActiveSuffix)
        );
        let x = &plan(&preset, &rows, Platform::SeriesX).unwrap()[0];
        assert_eq!(
            (x.index, x.after, x.selection),
            (0, 3.0, Selection::ActiveSuffix)
        );
        let fallback = &plan(&preset, &rows[..2], Platform::SeriesS).unwrap()[0];
        assert_eq!(
            (fallback.index, fallback.after, fallback.selection),
            (1, 0.375, Selection::Base)
        );
    }

    #[test]
    fn rejects_missing_or_ambiguous_winners_for_the_entire_plan() {
        let preset = parse_preset(LOW).unwrap();
        let slod = row(hash_key(SLOD), 1.0);
        let base = row(hash_key(IMPOSTER), 1.0);
        let active = row(active_hash(IMPOSTER, &Platform::SeriesS), 2.0);

        assert!(
            matches!(plan(&preset, &[slod], Platform::SeriesS), Err(PresetError::MissingRow(key)) if key == IMPOSTER)
        );
        assert!(
            matches!(plan(&preset, &[slod, base, base], Platform::SeriesS), Err(PresetError::DuplicateRow { hash, .. }) if hash == base.hash)
        );
        assert!(
            matches!(plan(&preset, &[slod, base, active, active], Platform::SeriesS), Err(PresetError::DuplicateRow { hash, .. }) if hash == active.hash)
        );
    }

    #[test]
    fn rejects_invalid_scales_and_preserves_zero() {
        let preset = parse_preset(SMOKE).unwrap();
        let hash = hash_key(SLOD);
        for invalid in [f32::NAN, f32::INFINITY, f32::NEG_INFINITY, -1.0] {
            assert!(matches!(
                plan(&preset, &[row(hash, invalid)], Platform::SeriesX),
                Err(PresetError::InvalidScale { .. })
            ));
        }
        let rows = [row(hash, 0.0)];
        assert_eq!(
            plan(&preset, &rows, Platform::SeriesX).unwrap()[0].after,
            0.0
        );
    }

    #[test]
    fn rejects_invalid_schema_and_multipliers() {
        for invalid in [
            SMOKE.replace("0.75", "NaN"),
            SMOKE.replace("0.75", "\"0.75\""),
            SMOKE.replace("0.75", "1e999"),
            SMOKE.replace("0.75", "1.00000000001"),
            SMOKE.replace("0.75", "0.24999999999"),
            SMOKE.replace("\"multiply\"", "\"set\""),
            SMOKE.replace("\"dry_run\"", "\"unexpected\""),
            SMOKE.replace("\"schema_version\": 1", "\"schema_version\": 2"),
            SMOKE.replace(SLOD, "unverified.setting"),
            LOW.replace(IMPOSTER, &SLOD.to_ascii_uppercase()),
        ] {
            assert!(parse_preset(&invalid).is_err(), "accepted {invalid}");
        }
    }

    #[test]
    fn inspect_preset_plans_the_same_changes_without_requesting_writes() {
        let inspect = parse_preset(INSPECT).unwrap();
        let low = parse_preset(LOW).unwrap();
        assert!(inspect.dry_run);
        assert!(!low.dry_run);
        let rows = [row(hash_key(SLOD), 2.0), row(hash_key(IMPOSTER), 1.5)];
        assert_eq!(
            plan(&inspect, &rows, Platform::SeriesX).unwrap(),
            plan(&low, &rows, Platform::SeriesX).unwrap()
        );
    }
}
