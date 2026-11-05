use crate::als::Scale;
use anyhow::{ensure, Context, Result};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::fs::{self, File, OpenOptions};
use std::num::NonZeroU64;
use std::ops::Deref;
use std::path::{Path, PathBuf};

#[derive(Debug, PartialEq, Eq, Clone, Copy)]
pub enum Kind {
    Brightness,
    Dim,
    Temperature,
}

impl Kind {
    pub fn name(self) -> &'static str {
        match self {
            Self::Brightness => "brightness",
            Self::Dim => "dim",
            Self::Temperature => "temperature",
        }
    }

    pub fn unit(self) -> &'static str {
        match self {
            Self::Brightness => "",
            Self::Dim => "%",
            Self::Temperature => "K",
        }
    }
}

#[derive(Debug, PartialEq, Eq, Clone)]
pub struct Data {
    pub output_name: String,
    pub entries: Vec<Observation>,
    legacy_entries_v1: Option<Vec<LegacyEntryV1>>,
    kind: Kind,
    thresholds: HashMap<u64, String>,
    scale: Scale,
    maximum: Option<NonZeroU64>,
}

#[derive(Debug, PartialEq, Eq, Serialize, Deserialize, Clone)]
struct LegacyEntryV1 {
    lux: String,
    luma: u8,
    brightness: u64,
}

#[derive(Debug, PartialEq, Eq, Hash, Serialize, Clone)]
pub struct Entry {
    pub als: u64,
    pub luma: u8,
    #[serde(rename = "value")]
    pub brightness: u64,
}

#[derive(Debug, PartialEq, Eq, Clone)]
pub struct Observation {
    entry: Entry,
    maximum: Option<NonZeroU64>,
}

impl Deref for Observation {
    type Target = Entry;

    fn deref(&self) -> &Entry {
        &self.entry
    }
}

#[cfg(test)]
impl PartialEq<Entry> for Observation {
    fn eq(&self, other: &Entry) -> bool {
        self.entry == *other
    }
}

#[cfg(test)]
impl PartialEq<Observation> for Entry {
    fn eq(&self, other: &Observation) -> bool {
        *self == other.entry
    }
}

impl Observation {
    fn project(&self, maximum: Option<NonZeroU64>) -> Entry {
        let brightness = match (self.maximum, maximum) {
            (Some(original), Some(current)) => {
                let numerator = self.brightness as u128 * current.get() as u128;
                ((numerator + original.get() as u128 / 2) / original.get() as u128)
                    .min(u64::MAX as u128) as u64
            }
            _ => self.brightness,
        };
        Entry::new(self.als, self.luma, brightness)
    }

    fn stored(&self) -> StoredEntry {
        StoredEntry {
            als: StoredAls::Value(self.als),
            luma: self.luma,
            brightness: self.brightness,
            max_value: self.maximum,
        }
    }
}

#[derive(Serialize, Deserialize)]
struct StoredData {
    output_name: String,
    entries: StoredEntries,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    legacy_entries_v1: Option<Vec<LegacyEntryV1>>,
}

#[derive(Serialize)]
#[serde(untagged)]
enum StoredEntries {
    Legacy(Vec<StoredEntry>),
    Grouped(StoredEntryGroups),
}

impl<'de> Deserialize<'de> for StoredEntries {
    fn deserialize<D: serde::Deserializer<'de>>(
        deserializer: D,
    ) -> std::result::Result<Self, D::Error> {
        struct EntriesVisitor;

        impl<'de> serde::de::Visitor<'de> for EntriesVisitor {
            type Value = StoredEntries;

            fn expecting(&self, formatter: &mut std::fmt::Formatter) -> std::fmt::Result {
                formatter.write_str("a learned-entry list or grouped map")
            }

            fn visit_seq<A: serde::de::SeqAccess<'de>>(
                self,
                sequence: A,
            ) -> std::result::Result<Self::Value, A::Error> {
                Vec::deserialize(serde::de::value::SeqAccessDeserializer::new(sequence))
                    .map(StoredEntries::Legacy)
            }

            fn visit_map<A: serde::de::MapAccess<'de>>(
                self,
                map: A,
            ) -> std::result::Result<Self::Value, A::Error> {
                StoredEntryGroups::deserialize(serde::de::value::MapAccessDeserializer::new(map))
                    .map(StoredEntries::Grouped)
            }
        }

        deserializer.deserialize_any(EntriesVisitor)
    }
}

#[derive(Serialize, Deserialize, Default)]
#[serde(default)]
struct StoredEntryGroups {
    brightness: Vec<StoredEntry>,
    dim: Vec<StoredEntry>,
    temperature: Vec<StoredEntry>,
}

#[derive(Serialize, Deserialize)]
struct StoredEntry {
    #[serde(alias = "lux")]
    als: StoredAls,
    luma: u8,
    #[serde(rename = "value", alias = "brightness")]
    brightness: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    max_value: Option<NonZeroU64>,
}

impl StoredEntry {
    fn migrate(
        self,
        thresholds: &HashMap<u64, String>,
        scale: Scale,
        maximum: Option<NonZeroU64>,
    ) -> Result<(Option<Observation>, Option<LegacyEntryV1>, bool)> {
        ensure!(
            self.max_value.is_none() || maximum.is_some(),
            "max_value is only valid for hardware brightness"
        );
        let brightness = self.brightness;
        let denominator = self.max_value.or(maximum);
        let normalized = self.max_value.is_none() && maximum.is_some();
        let legacy = match &self.als {
            StoredAls::Value(_) => None,
            StoredAls::Profile(profile) => Some(LegacyEntryV1 {
                lux: profile.clone(),
                luma: self.luma,
                brightness,
            }),
        };
        let als = match self.als {
            StoredAls::Value(value) => value,
            StoredAls::Profile(profile) if profile == "none" => 0,
            StoredAls::Profile(profile) => {
                let mut thresholds = thresholds.iter().collect::<Vec<_>>();
                thresholds.sort_unstable_by_key(|(value, _)| **value);
                let Some(index) = thresholds
                    .iter()
                    .position(|(_, name)| name.as_str() == profile)
                else {
                    log::warn!("Dropping learned data with unknown ALS profile '{profile}'");
                    return Ok((None, legacy, normalized));
                };
                let lower = scale.coordinate(*thresholds[index].0);
                let upper = thresholds
                    .get(index + 1)
                    .map(|(value, _)| scale.coordinate(**value))
                    .or_else(|| {
                        index.checked_sub(1).map(|previous| {
                            lower + lower - scale.coordinate(*thresholds[previous].0)
                        })
                    })
                    .unwrap_or(lower);
                scale.value((lower + upper) / 2.0)
            }
        };
        Ok((
            Some(Observation {
                entry: Entry::new(als, self.luma, brightness),
                maximum: denominator,
            }),
            legacy,
            normalized,
        ))
    }
}

#[derive(Serialize, Deserialize)]
#[serde(untagged)]
enum StoredAls {
    Value(u64),
    Profile(String),
}

impl Data {
    pub fn new_kind(
        output_name: &str,
        kind: Kind,
        scale: Scale,
        thresholds: &HashMap<u64, String>,
        maximum: Option<NonZeroU64>,
    ) -> Self {
        Self {
            output_name: output_name.to_string(),
            entries: Vec::new(),
            legacy_entries_v1: None,
            kind,
            thresholds: thresholds.clone(),
            scale,
            maximum,
        }
    }

    pub fn observe(&self, entry: Entry) -> Observation {
        Observation {
            entry,
            maximum: self.maximum,
        }
    }

    pub fn projected(&self) -> Vec<Entry> {
        self.entries
            .iter()
            .map(|entry| entry.project(self.maximum))
            .collect()
    }

    pub fn load_kind(
        output_name: &str,
        kind: Kind,
        thresholds: &HashMap<u64, String>,
        scale: Scale,
        maximum: Option<NonZeroU64>,
    ) -> Result<Self> {
        let path = Self::path(output_name)?;
        let stored = match Self::read_file(&path)? {
            Some(stored) => stored,
            None => {
                return Ok(Self::new_kind(
                    output_name,
                    kind,
                    scale,
                    thresholds,
                    maximum,
                ))
            }
        };
        if stored.output_name != output_name {
            log::warn!(
                "Learned data for '{output_name}' contains output name '{}'",
                stored.output_name
            );
        }
        let (data, migrated) =
            Self::from_stored(output_name, kind, stored, thresholds, scale, maximum)?;
        if migrated {
            data.save()
                .context("Unable to save migrated learned data")?;
        }
        Ok(data)
    }

    fn from_stored(
        output_name: &str,
        kind: Kind,
        stored: StoredData,
        thresholds: &HashMap<u64, String>,
        scale: Scale,
        maximum: Option<NonZeroU64>,
    ) -> Result<(Self, bool)> {
        ensure!(
            maximum.is_some() == (kind == Kind::Brightness),
            "Hardware brightness requires a nonzero maximum; scalar values must not have one"
        );
        let mut migrated = false;
        let mut legacy_entries_v1 = stored.legacy_entries_v1;
        let stored_entries = match stored.entries {
            StoredEntries::Legacy(entries) => {
                migrated = true;
                if kind == Kind::Brightness {
                    entries
                } else {
                    Vec::new()
                }
            }
            StoredEntries::Grouped(groups) => match kind {
                Kind::Brightness => groups.brightness,
                Kind::Dim => groups.dim,
                Kind::Temperature => groups.temperature,
            },
        };
        let mut entries = Vec::new();
        for entry in stored_entries {
            let (entry, legacy, normalized) = entry.migrate(thresholds, scale, maximum)?;
            migrated |= normalized;
            if let Some(legacy) = legacy {
                migrated = true;
                legacy_entries_v1.get_or_insert_with(Vec::new).push(legacy);
            }
            entries.extend(entry);
        }
        Ok((
            Self {
                output_name: output_name.to_string(),
                entries,
                legacy_entries_v1,
                kind,
                thresholds: thresholds.clone(),
                scale,
                maximum,
            },
            migrated,
        ))
    }

    pub fn save(&self) -> Result<()> {
        self.save_to_existing(&Self::path(&self.output_name)?)
    }

    fn save_to_existing(&self, path: &Path) -> Result<()> {
        lazy_static::lazy_static! {
            static ref SAVE_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
        }
        let _lock = SAVE_LOCK
            .lock()
            .map_err(|_| anyhow::anyhow!("Learned-data save lock was poisoned"))?;
        let mut stored = Self::read_file(path)?.unwrap_or_else(|| self.empty_stored());
        self.merge(&mut stored);
        Self::save_to_path(&stored, path)
    }

    fn empty_stored(&self) -> StoredData {
        StoredData {
            output_name: self.output_name.clone(),
            entries: StoredEntries::Grouped(StoredEntryGroups::default()),
            legacy_entries_v1: None,
        }
    }

    fn merge(&self, stored: &mut StoredData) {
        if let StoredEntries::Legacy(entries) = &mut stored.entries {
            stored.entries = StoredEntries::Grouped(StoredEntryGroups {
                brightness: std::mem::take(entries),
                ..Default::default()
            });
        }
        let StoredEntries::Grouped(groups) = &mut stored.entries else {
            unreachable!()
        };
        let selected = match self.kind {
            Kind::Brightness => {
                stored.legacy_entries_v1.clone_from(&self.legacy_entries_v1);
                &mut groups.brightness
            }
            Kind::Dim => &mut groups.dim,
            Kind::Temperature => &mut groups.temperature,
        };
        *selected = self.entries.iter().map(Observation::stored).collect();
        stored.output_name.clone_from(&self.output_name);
    }

    fn save_to_path(saved: &StoredData, path: &Path) -> Result<()> {
        let file_name = path
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("state");
        let temporary = path.with_file_name(format!(".{file_name}.{}.tmp", std::process::id()));
        let result = (|| -> Result<()> {
            let mut file = OpenOptions::new()
                .create(true)
                .write(true)
                .truncate(true)
                .open(&temporary)?;
            yaml_serde::to_writer(&mut file, saved)?;
            file.sync_all()?;
            fs::rename(&temporary, path)?;
            if let Some(parent) = path.parent() {
                File::open(parent)?.sync_all()?;
            }
            Ok(())
        })();
        if result.is_err() {
            let _ = fs::remove_file(&temporary);
        }
        result
    }

    fn read_file(path: &Path) -> Result<Option<StoredData>> {
        match File::open(path) {
            Ok(file) => Ok(Some(yaml_serde::from_reader(file).with_context(|| {
                format!("Unable to parse learned data '{}'", path.display())
            })?)),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(error) => Err(error)
                .with_context(|| format!("Unable to read learned data '{}'", path.display())),
        }
    }

    fn path(output_name: &str) -> Result<PathBuf> {
        Ok(xdg::BaseDirectories::with_prefix("wluma")
            .create_state_directory("")?
            .join(format!("{output_name}.yaml")))
    }
}

impl Serialize for Data {
    fn serialize<S>(&self, serializer: S) -> std::result::Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        let mut stored = self.empty_stored();
        self.merge(&mut stored);
        stored.serialize(serializer)
    }
}

impl Entry {
    pub fn new(als: u64, luma: u8, brightness: u64) -> Self {
        Self {
            als,
            luma,
            brightness,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn maximum(value: u64) -> Option<NonZeroU64> {
        Some(NonZeroU64::new(value).unwrap())
    }

    fn load(yaml: &str, kind: Kind, max: u64) -> (Data, bool) {
        Data::from_stored(
            "panel",
            kind,
            yaml_serde::from_str(yaml).unwrap(),
            &HashMap::new(),
            Scale::Linear,
            (kind == Kind::Brightness).then(|| NonZeroU64::new(max).unwrap()),
        )
        .unwrap()
    }

    fn reload(data: &Data, max: u64) -> Data {
        load(&yaml_serde::to_string(data).unwrap(), data.kind, max).0
    }

    #[test]
    fn fractions_preserve_raw_values_and_round_half_up() {
        let (data, migrated) = load(
            "output_name: panel\nentries:\n- als: 42\n  luma: 20\n  brightness: 248\n",
            Kind::Brightness,
            255,
        );
        assert!(migrated);
        assert_eq!(vec![Entry::new(42, 20, 248)], data.projected());
        let other = reload(&data, 100);
        assert_eq!(97, other.projected()[0].brightness);
        assert_eq!(248, reload(&other, 255).projected()[0].brightness);
        assert!(
            !load(
                &yaml_serde::to_string(&other).unwrap(),
                Kind::Brightness,
                100
            )
            .1
        );
        let half = Observation {
            entry: Entry::new(0, 0, 1),
            maximum: maximum(2),
        };
        assert_eq!(1, half.project(maximum(1)).brightness);
    }

    #[test]
    fn all_unchanged_scale_values_are_exact() {
        for max in [1, 100, 255, 65535, u64::MAX] {
            for raw in [0, 1, 99, 248, 65535, u64::MAX] {
                let observation = Observation {
                    entry: Entry::new(1, 2, raw),
                    maximum: maximum(max),
                };
                assert_eq!(raw, observation.project(maximum(max)).brightness);
            }
        }
        let observation = Observation {
            entry: Entry::new(1, 2, u64::MAX),
            maximum: maximum(1),
        };
        assert_eq!(u64::MAX, observation.project(maximum(u64::MAX)).brightness);
    }

    #[test]
    fn mixed_scales_survive_unrelated_learning_and_saves() {
        let (original, _) = load(
            "output_name: panel\nentries:\n- als: 10\n  luma: 20\n  value: 248\n",
            Kind::Brightness,
            255,
        );
        let mut other = reload(&original, 100);
        other.entries.push(other.observe(Entry::new(90, 30, 33)));
        let restored = reload(&other, 255);
        assert_eq!(
            vec![Entry::new(10, 20, 248), Entry::new(90, 30, 84)],
            restored.projected()
        );
        assert_eq!(other.entries, reload(&restored, 100).entries);
    }

    #[test]
    fn profile_migration_preserves_archive() {
        let thresholds = [(10, "dark".to_string()), (20, "bright".to_string())]
            .into_iter()
            .collect();
        let stored = yaml_serde::from_str("output_name: panel\nentries:\n- lux: dark\n  luma: 20\n  brightness: 30\n- lux: unknown\n  luma: 30\n  brightness: 40\n").unwrap();
        let (data, migrated) = Data::from_stored(
            "panel",
            Kind::Brightness,
            stored,
            &thresholds,
            Scale::Linear,
            maximum(255),
        )
        .unwrap();
        assert!(migrated);
        assert_eq!(vec![Entry::new(15, 20, 30)], data.projected());
        assert_eq!(2, data.legacy_entries_v1.as_ref().unwrap().len());
        assert_eq!(data.legacy_entries_v1, reload(&data, 100).legacy_entries_v1);
    }

    #[test]
    fn scalar_saves_preserve_brightness_and_archives() {
        let (brightness, _) = load(
            "output_name: panel\nentries:\n- als: 10\n  luma: 20\n  value: 248\n",
            Kind::Brightness,
            255,
        );
        let mut stored =
            yaml_serde::from_str(&yaml_serde::to_string(&brightness).unwrap()).unwrap();
        for (kind, value) in [(Kind::Dim, 40), (Kind::Temperature, 4500)] {
            let mut data = Data::new_kind("panel", kind, Scale::Linear, &HashMap::new(), None);
            data.entries.push(data.observe(Entry::new(20, 30, value)));
            data.merge(&mut stored);
        }
        let yaml = yaml_serde::to_string(&stored).unwrap();
        assert_eq!(
            brightness.entries,
            load(&yaml, Kind::Brightness, 100).0.entries
        );
        assert_eq!(40, load(&yaml, Kind::Dim, 0).0.projected()[0].brightness);
        assert_eq!(
            4500,
            load(&yaml, Kind::Temperature, 0).0.projected()[0].brightness
        );
        let mut legacy: StoredData = yaml_serde::from_str("output_name: panel\nentries:\n- lux: dark\n  luma: 20\n  brightness: 248\nlegacy_entries_v1:\n- lux: old\n  luma: 1\n  brightness: 2\n").unwrap();
        load(&yaml, Kind::Dim, 0).0.merge(&mut legacy);
        let StoredEntries::Grouped(groups) = legacy.entries else {
            unreachable!()
        };
        assert!(matches!(groups.brightness[0].als, StoredAls::Profile(_)));
        assert_eq!(248, groups.brightness[0].brightness);
        assert!(groups.brightness[0].max_value.is_none());
        assert_eq!(1, legacy.legacy_entries_v1.unwrap().len());
    }

    #[test]
    fn rejects_invalid_fraction_without_reinterpreting_it_as_raw() {
        let invalid = yaml_serde::from_str::<StoredData>("output_name: panel\nentries:\n  brightness:\n  - als: 1\n    luma: 2\n    value: 3\n    max_value: 0\n");
        let error = invalid.err().unwrap().to_string();
        assert!(error.contains("nonzero"), "{error}");
        let stored = yaml_serde::from_str("output_name: panel\nentries:\n  dim:\n  - als: 1\n    luma: 2\n    value: 3\n    max_value: 100\n").unwrap();
        assert!(Data::from_stored(
            "panel",
            Kind::Dim,
            stored,
            &HashMap::new(),
            Scale::Linear,
            None
        )
        .is_err());
    }

    #[test]
    fn atomic_group_saves_preserve_fractions_and_archives() {
        let dir = std::env::temp_dir().join(format!(
            "wluma-rational-save-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir(&dir).unwrap();
        let path = dir.join("panel.yaml");
        let yaml = "output_name: panel\nentries:\n  brightness:\n  - als: 10\n    luma: 20\n    value: 248\n    max_value: 255\n  dim:\n  - als: 20\n    luma: 30\n    value: 40\n  temperature:\n  - als: 20\n    luma: 0\n    value: 4500\nlegacy_entries_v1:\n- lux: dark\n  luma: 20\n  brightness: 248\n";
        fs::write(&path, yaml).unwrap();
        let brightness = load(yaml, Kind::Brightness, 100).0;
        let mut dim = load(yaml, Kind::Dim, 0).0;
        dim.entries = vec![dim.observe(Entry::new(30, 40, 50))];
        let mut temperature = load(yaml, Kind::Temperature, 0).0;
        temperature.entries = vec![temperature.observe(Entry::new(30, 0, 4600))];
        std::thread::scope(|scope| {
            for data in [&brightness, &dim, &temperature] {
                let path = &path;
                scope.spawn(move || data.save_to_existing(path).unwrap());
            }
        });
        let saved = fs::read_to_string(&path).unwrap();
        let restored = load(&saved, Kind::Brightness, 255).0;
        assert_eq!(248, restored.projected()[0].brightness);
        assert_eq!(brightness.entries, restored.entries);
        assert_eq!(brightness.legacy_entries_v1, restored.legacy_entries_v1);
        assert_eq!(dim.entries, load(&saved, Kind::Dim, 0).0.entries);
        assert_eq!(
            temperature.entries,
            load(&saved, Kind::Temperature, 0).0.entries
        );
        assert_eq!(1, fs::read_dir(&dir).unwrap().count());
        let invalid = yaml.replace("max_value: 255", "max_value: 0");
        fs::write(&path, &invalid).unwrap();
        assert!(dim.save_to_existing(&path).is_err());
        assert_eq!(invalid, fs::read_to_string(&path).unwrap());
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn profile_midpoints_keep_native_scale_and_omit_empty_archives() {
        for (profile, expected) in [("none", 0), ("bright", 100)] {
            let stored = yaml_serde::from_str(&format!(
                "output_name: panel\nentries:\n- lux: {profile}\n  luma: 20\n  brightness: 30\n"
            ))
            .unwrap();
            let thresholds = [(20, "dim".to_string()), (80, "bright".to_string())]
                .into_iter()
                .collect();
            let (data, _) = Data::from_stored(
                "panel",
                Kind::Brightness,
                stored,
                &thresholds,
                Scale::Linear,
                maximum(255),
            )
            .unwrap();
            assert_eq!(expected, data.entries[0].als);
        }
        let data = Data::new_kind(
            "panel",
            Kind::Brightness,
            Scale::Linear,
            &HashMap::new(),
            maximum(255),
        );
        assert!(!yaml_serde::to_string(&data)
            .unwrap()
            .contains("legacy_entries_v1"));
    }

    #[test]
    fn legacy_overrange_values_use_current_maximum_without_guessing() {
        let (data, _) = load(
            "output_name: panel\nentries:\n- als: 1\n  luma: 2\n  value: 248\n",
            Kind::Brightness,
            100,
        );
        assert_eq!(248, data.projected()[0].brightness);
        assert_eq!(632, reload(&data, 255).projected()[0].brightness);
    }
}
