use crate::als::Scale;
use anyhow::Result;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::fs::{self, File, OpenOptions};
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
    pub entries: Vec<Entry>,
    legacy_entries_v1: Option<Vec<LegacyEntryV1>>,
    kind: Kind,
    thresholds: HashMap<u64, String>,
    scale: Scale,
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

#[derive(Deserialize)]
struct StoredData {
    output_name: String,
    entries: StoredEntries,
    #[serde(default)]
    legacy_entries_v1: Option<Vec<LegacyEntryV1>>,
}

#[derive(Deserialize)]
#[serde(untagged)]
enum StoredEntries {
    Legacy(Vec<StoredEntry>),
    Grouped(StoredEntryGroups),
}

impl IntoIterator for StoredEntries {
    type Item = StoredEntry;
    type IntoIter = std::vec::IntoIter<StoredEntry>;

    fn into_iter(self) -> Self::IntoIter {
        match self {
            Self::Legacy(entries) => entries.into_iter(),
            Self::Grouped(groups) => groups.brightness.into_iter(),
        }
    }
}

#[derive(Deserialize, Default)]
#[serde(default)]
struct StoredEntryGroups {
    brightness: Vec<StoredEntry>,
    dim: Vec<StoredEntry>,
    temperature: Vec<StoredEntry>,
}

#[derive(Serialize)]
struct SavedData<'a> {
    output_name: &'a str,
    entries: SavedEntryGroups<'a>,
    #[serde(skip_serializing_if = "Option::is_none")]
    legacy_entries_v1: &'a Option<Vec<LegacyEntryV1>>,
}

#[derive(Serialize)]
struct SavedEntryGroups<'a> {
    brightness: &'a [Entry],
    dim: &'a [Entry],
    temperature: &'a [Entry],
}

#[derive(Deserialize)]
struct StoredEntry {
    #[serde(alias = "lux")]
    als: StoredAls,
    luma: u8,
    #[serde(alias = "value")]
    brightness: u64,
}

impl StoredEntry {
    fn migrate(
        self,
        thresholds: &HashMap<u64, String>,
        scale: Scale,
    ) -> (Option<Entry>, Option<LegacyEntryV1>) {
        let legacy = match &self.als {
            StoredAls::Value(_) => None,
            StoredAls::Profile(profile) => Some(LegacyEntryV1 {
                lux: profile.clone(),
                luma: self.luma,
                brightness: self.brightness,
            }),
        };
        let als = match self.als {
            StoredAls::Value(value) => {
                return (Some(Entry::new(value, self.luma, self.brightness)), legacy)
            }
            StoredAls::Profile(profile) if profile == "none" => 0,
            StoredAls::Profile(profile) => {
                let mut thresholds = thresholds.iter().collect::<Vec<_>>();
                thresholds.sort_unstable_by_key(|(value, _)| **value);
                let Some(index) = thresholds
                    .iter()
                    .position(|(_, name)| name.as_str() == profile)
                else {
                    log::warn!("Dropping learned data with unknown ALS profile '{profile}'");
                    return (None, legacy);
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
        (Some(Entry::new(als, self.luma, self.brightness)), legacy)
    }
}

#[derive(Deserialize)]
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
    ) -> Self {
        Self {
            output_name: output_name.to_string(),
            entries: Vec::new(),
            legacy_entries_v1: None,
            kind,
            thresholds: thresholds.clone(),
            scale,
        }
    }

    pub fn load_kind(
        output_name: &str,
        kind: Kind,
        thresholds: &HashMap<u64, String>,
        scale: Scale,
    ) -> Self {
        let (data, migrated) = Self::load_inner(output_name, kind, thresholds, scale);
        if migrated {
            data.save().expect("Unable to save migrated learned data");
        }
        data
    }

    fn load_inner(
        output_name: &str,
        kind: Kind,
        thresholds: &HashMap<u64, String>,
        scale: Scale,
    ) -> (Self, bool) {
        let empty = || Self::new_kind(output_name, kind, scale, thresholds);
        let path = match Self::path(output_name) {
            Ok(path) if path.exists() => path,
            Ok(_) => return (empty(), false),
            Err(error) => {
                log::warn!("Unable to locate learned data for '{output_name}': {error}");
                return (empty(), false);
            }
        };
        let stored = match Self::read_file(path)
            .and_then(|file| serde_yaml::from_reader::<_, StoredData>(file).map_err(Into::into))
        {
            Ok(stored) => stored,
            Err(error) => {
                log::warn!("Unable to load learned data for '{output_name}': {error}");
                return (empty(), false);
            }
        };
        if stored.output_name != output_name {
            log::warn!(
                "Learned data for '{output_name}' contains output name '{}'",
                stored.output_name
            );
        }
        Self::from_stored(output_name, kind, stored, thresholds, scale)
    }

    fn from_stored(
        output_name: &str,
        kind: Kind,
        stored: StoredData,
        thresholds: &HashMap<u64, String>,
        scale: Scale,
    ) -> (Self, bool) {
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
        let entries = stored_entries
            .into_iter()
            .filter_map(|entry| {
                let (entry, legacy) = entry.migrate(thresholds, scale);
                if let Some(legacy) = legacy {
                    migrated = true;
                    legacy_entries_v1.get_or_insert_with(Vec::new).push(legacy);
                }
                entry
            })
            .collect();
        (
            Self {
                output_name: output_name.to_string(),
                entries,
                legacy_entries_v1,
                kind,
                thresholds: thresholds.clone(),
                scale,
            },
            migrated,
        )
    }

    pub fn save(&self) -> Result<()> {
        lazy_static::lazy_static! {
            static ref SAVE_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
        }
        let _lock = SAVE_LOCK.lock().unwrap();
        let path = Self::path(&self.output_name)?;
        let mut groups = [Kind::Brightness, Kind::Dim, Kind::Temperature]
            .map(|kind| Self::load_inner(&self.output_name, kind, &self.thresholds, self.scale).0);
        let saved = self.merge(&mut groups);
        Self::save_to_path(&saved, &path)
    }

    fn merge<'a>(&'a self, groups: &'a mut [Self; 3]) -> SavedData<'a> {
        groups[self.kind as usize].entries.clone_from(&self.entries);
        let legacy_entries_v1 = if self.kind == Kind::Brightness {
            &self.legacy_entries_v1
        } else {
            &groups[Kind::Brightness as usize].legacy_entries_v1
        };
        SavedData {
            output_name: &self.output_name,
            entries: SavedEntryGroups {
                brightness: &groups[Kind::Brightness as usize].entries,
                dim: &groups[Kind::Dim as usize].entries,
                temperature: &groups[Kind::Temperature as usize].entries,
            },
            legacy_entries_v1,
        }
    }

    fn save_to_path(saved: &SavedData<'_>, path: &Path) -> Result<()> {
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
            serde_yaml::to_writer(&mut file, saved)?;
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

    fn read_file(path: PathBuf) -> Result<File> {
        Ok(File::open(path)?)
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
        let empty = Vec::new();
        SavedData {
            output_name: &self.output_name,
            entries: SavedEntryGroups {
                brightness: if self.kind == Kind::Brightness {
                    &self.entries
                } else {
                    &empty
                },
                dim: if self.kind == Kind::Dim {
                    &self.entries
                } else {
                    &empty
                },
                temperature: if self.kind == Kind::Temperature {
                    &self.entries
                } else {
                    &empty
                },
            },
            legacy_entries_v1: &self.legacy_entries_v1,
        }
        .serialize(serializer)
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

    #[test]
    fn migrates_profile_to_bucket_midpoint() {
        let stored: StoredData = serde_yaml::from_str(
            "output_name: panel\nentries:\n  - lux: dark\n    luma: 20\n    brightness: 30\n",
        )
        .unwrap();
        let thresholds = [(10, "dark".to_string()), (20, "dark".to_string())]
            .into_iter()
            .collect();
        let (entry, legacy) = stored
            .entries
            .into_iter()
            .next()
            .unwrap()
            .migrate(&thresholds, Scale::Linear);
        assert_eq!(Some(Entry::new(15, 20, 30)), entry);
        assert_eq!(
            Some(LegacyEntryV1 {
                lux: "dark".to_string(),
                luma: 20,
                brightness: 30,
            }),
            legacy
        );
    }

    #[test]
    fn accepts_numeric_als() {
        let stored: StoredData = serde_yaml::from_str(
            "output_name: panel\nentries:\n  - als: 42\n    luma: 20\n    brightness: 30\n",
        )
        .unwrap();
        let (entry, legacy) = stored
            .entries
            .into_iter()
            .next()
            .unwrap()
            .migrate(&HashMap::new(), Scale::Linear);
        assert_eq!(Some(Entry::new(42, 20, 30)), entry);
        assert_eq!(None, legacy);
    }

    #[test]
    fn drops_unknown_profiles_as_migrated() {
        let stored: StoredData = serde_yaml::from_str(
            "output_name: panel\nentries:\n  - lux: unknown\n    luma: 20\n    brightness: 30\n",
        )
        .unwrap();
        let (entry, legacy) = stored
            .entries
            .into_iter()
            .next()
            .unwrap()
            .migrate(&HashMap::new(), Scale::Linear);
        assert_eq!(None, entry);
        assert_eq!(
            Some(LegacyEntryV1 {
                lux: "unknown".to_string(),
                luma: 20,
                brightness: 30,
            }),
            legacy
        );
    }

    #[test]
    fn keeps_linear_migration_in_native_domain() {
        let stored: StoredData = serde_yaml::from_str(
            "output_name: panel\nentries:\n  - lux: bright\n    luma: 20\n    brightness: 30\n",
        )
        .unwrap();
        let thresholds = [(80, "bright".to_string()), (20, "dim".to_string())]
            .into_iter()
            .collect();
        let (entry, _) = stored
            .entries
            .into_iter()
            .next()
            .unwrap()
            .migrate(&thresholds, Scale::Linear);
        assert_eq!(Some(Entry::new(100, 20, 30)), entry);
    }

    #[test]
    fn preserves_legacy_entries_across_reloads() {
        let stored = serde_yaml::from_str(
            "output_name: panel\nentries:\n  - lux: dark\n    luma: 20\n    brightness: 30\n",
        )
        .unwrap();
        let thresholds = [(10, "dark".to_string()), (20, "bright".to_string())]
            .into_iter()
            .collect();

        let (migrated, was_migrated) = Data::from_stored(
            "panel",
            Kind::Brightness,
            stored,
            &thresholds,
            Scale::Linear,
        );
        assert!(was_migrated);
        assert_eq!(vec![Entry::new(15, 20, 30)], migrated.entries);

        let stored = serde_yaml::from_str(&serde_yaml::to_string(&migrated).unwrap()).unwrap();
        let (reloaded, was_migrated) = Data::from_stored(
            "panel",
            Kind::Brightness,
            stored,
            &thresholds,
            Scale::Linear,
        );
        assert!(!was_migrated);
        assert_eq!(migrated, reloaded);
    }

    #[test]
    fn groups_values_by_kind() {
        let mut data = Data::new_kind("panel", Kind::Temperature, Scale::Linear, &HashMap::new());
        data.entries.push(Entry::new(10, 20, 4500));
        let yaml = serde_yaml::to_string(&data).unwrap();
        assert!(yaml.contains("brightness: []"));
        assert!(yaml.contains("dim: []"));
        assert!(yaml.contains("temperature:"));
        assert!(yaml.contains("value: 4500"));
    }

    #[test]
    fn updating_one_kind_preserves_the_other_kinds() {
        let thresholds = HashMap::new();
        let mut groups = [Kind::Brightness, Kind::Dim, Kind::Temperature]
            .map(|kind| Data::new_kind("panel", kind, Scale::Linear, &thresholds));
        groups[Kind::Brightness as usize].entries = vec![Entry::new(10, 20, 30)];
        groups[Kind::Dim as usize].entries = vec![Entry::new(10, 20, 40)];
        groups[Kind::Temperature as usize].entries = vec![Entry::new(10, 0, 4500)];
        let mut update = Data::new_kind("panel", Kind::Dim, Scale::Linear, &thresholds);
        update.entries = vec![Entry::new(20, 30, 50)];

        let yaml = serde_yaml::to_string(&update.merge(&mut groups)).unwrap();
        let stored: StoredData = serde_yaml::from_str(&yaml).unwrap();
        let StoredEntries::Grouped(groups) = stored.entries else {
            unreachable!()
        };

        assert_eq!(groups.brightness.len(), 1);
        assert_eq!(groups.brightness[0].brightness, 30);
        assert_eq!(groups.dim.len(), 1);
        assert_eq!(groups.dim[0].brightness, 50);
        assert_eq!(groups.temperature.len(), 1);
        assert_eq!(groups.temperature[0].brightness, 4500);
    }

    #[test]
    fn omits_legacy_field_for_new_data() {
        let yaml = serde_yaml::to_string(&Data::new_kind(
            "panel",
            Kind::Brightness,
            Scale::Linear,
            &HashMap::new(),
        ))
        .unwrap();
        assert!(!yaml.contains("legacy_entries_v1"));
    }
}
