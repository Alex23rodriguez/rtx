use std::collections::HashMap;
use std::fmt::Debug;
use std::io::prelude::*;
use std::path::{Path, PathBuf};

use base64::prelude::*;
use color_eyre::eyre::Result;
use flate2::write::ZlibDecoder;
use flate2::write::ZlibEncoder;
use flate2::Compression;
use itertools::Itertools;
use serde_derive::{Deserialize, Serialize};

use crate::cmd;

#[derive(Default, Serialize, Deserialize)]
pub struct EnvDiff {
    #[serde(default)]
    pub old: HashMap<String, String>,
    #[serde(default)]
    pub new: HashMap<String, String>,
    #[serde(default)]
    pub path: Vec<PathBuf>,
}

#[derive(Debug)]
pub enum EnvDiffOperation {
    Add(String, String),
    Change(String, String),
    Remove(String),
}

pub type EnvDiffPatches = Vec<EnvDiffOperation>;

impl EnvDiff {
    pub fn new<T>(original: &HashMap<String, String>, additions: T) -> EnvDiff
    where
        T: IntoIterator<Item = (String, String)>,
    {
        let mut diff = EnvDiff::default();

        for (key, new_val) in additions.into_iter() {
            let key: String = key;
            match original.get(&key) {
                Some(original_val) => {
                    if original_val != &new_val {
                        diff.old.insert(key.clone(), original_val.into());
                        diff.new.insert(key, new_val);
                    }
                }
                None => {
                    diff.new.insert(key, new_val);
                }
            }
        }

        diff
    }

    pub fn from_bash_script<T, U, V>(script: &Path, env: T) -> Result<Self>
    where
        T: IntoIterator<Item = (U, V)>,
        U: Into<String>,
        V: Into<String>,
    {
        let env: HashMap<String, String> =
            env.into_iter().map(|(k, v)| (k.into(), v.into())).collect();
        let out = cmd!(
            "bash",
            "-c",
            indoc::formatdoc! {"
                set -e
                . {script}
                export -p
            ", script = script.display()}
        )
        .full_env(&env)
        .read()?;

        let mut additions = HashMap::new();
        let mut cur_key = None;
        for line in out.lines() {
            match line.strip_prefix("declare -x ") {
                Some(line) => {
                    let (k, v) = line.split_once('=').unwrap_or_default();
                    if valid_key(k) {
                        continue;
                    }
                    cur_key = Some(k.to_string());
                    additions.insert(k.to_string(), v.to_string());
                }
                None => {
                    if let Some(k) = &cur_key {
                        let v = format!("\n{}", line);
                        additions.get_mut(k).unwrap().push_str(&v);
                    }
                }
            }
        }
        for (k, v) in additions.clone().iter() {
            let v = if v.starts_with('"') && v.ends_with('"') {
                v[1..v.len() - 1].to_string()
            } else if v.starts_with("$'") && v.ends_with('\'') {
                v[2..v.len() - 1].to_string()
            } else {
                v.to_string()
            };
            let v = v.replace("\\'", "'");
            let v = v.replace("\\n", "\n");
            if let Some(orig) = env.get(k) {
                if &v == orig {
                    additions.remove(k);
                    continue;
                }
            }
            additions.insert(k.into(), v);
        }
        Ok(Self::new(&env, additions))
    }

    pub fn deserialize(raw: &str) -> Result<EnvDiff> {
        let mut writer = Vec::new();
        let mut decoder = ZlibDecoder::new(writer);
        let bytes = BASE64_STANDARD_NO_PAD.decode(raw)?;
        decoder.write_all(&bytes[..])?;
        writer = decoder.finish()?;
        Ok(rmp_serde::from_slice(&writer[..])?)
    }

    pub fn serialize(&self) -> Result<String> {
        let mut gz = ZlibEncoder::new(Vec::new(), Compression::fast());
        gz.write_all(&rmp_serde::to_vec_named(self)?)?;
        Ok(BASE64_STANDARD_NO_PAD.encode(gz.finish()?))
    }

    pub fn to_patches(&self) -> EnvDiffPatches {
        let mut patches = EnvDiffPatches::new();

        for k in self.old.keys() {
            match self.new.get(k) {
                Some(v) => patches.push(EnvDiffOperation::Change(k.into(), v.into())),
                None => patches.push(EnvDiffOperation::Remove(k.into())),
            };
        }
        for (k, v) in self.new.iter() {
            match self.old.contains_key(k) {
                false => patches.push(EnvDiffOperation::Add(k.into(), v.into())),
                true => {}
            };
        }

        patches
    }

    pub fn reverse(&self) -> EnvDiff {
        EnvDiff {
            old: self.new.clone(),
            new: self.old.clone(),
            path: self.path.clone(),
        }
    }
}

fn valid_key(k: &str) -> bool {
    k.is_empty()
        || k == "_"
        || k == "SHLVL"
        || k == "PATH"
        || k == "PWD"
        || k == "OLDPWD"
        || k == "HOME"
        || k == "USER"
        || k == "SHELL"
        || k == "SHELLOPTS"
        || k == "COMP_WORDBREAKS"
        || k == "PS1"
        // TODO: consider removing this
        // this is to make the ruby plugin compatible,
        // it causes ruby to attempt to call asdf to reshim the binaries
        // which we don't need or want to happen
        || k == "RUBYLIB"
        // following two ignores are for exported bash functions and exported bash
        // functions which are multiline, they appear in the environment as e.g.:
        // BASH_FUNC_exported-bash-function%%=() { echo "this is an"
        //  echo "exported bash function"
        //  echo "with multiple lines"
        // }
        || k.starts_with("BASH_FUNC_")
        || k.starts_with(' ')
}

impl Debug for EnvDiff {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let print_sorted = |hashmap: &HashMap<String, String>| {
            hashmap
                .iter()
                .map(|(k, v)| format!("{k}={v}"))
                .sorted()
                .collect::<Vec<_>>()
        };
        f.debug_struct("EnvDiff")
            .field("old", &print_sorted(&self.old))
            .field("new", &print_sorted(&self.new))
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use indexmap::indexmap;
    use insta::assert_debug_snapshot;

    use crate::dirs;

    use super::*;

    #[test]
    fn test_diff() {
        let diff = EnvDiff::new(&new_from_hashmap(), new_to_hashmap());
        assert_debug_snapshot!(diff.to_patches());
    }

    #[test]
    fn test_reverse() {
        let diff = EnvDiff::new(&new_from_hashmap(), new_to_hashmap());
        let patches = diff.reverse().to_patches();
        let to_remove = patches
            .iter()
            .filter_map(|p| match p {
                EnvDiffOperation::Remove(k) => Some(k),
                _ => None,
            })
            .collect::<Vec<_>>();
        assert_debug_snapshot!(to_remove, @r###"
        [
            "c",
        ]
        "###);
        let to_add = patches
            .iter()
            .filter_map(|p| match p {
                EnvDiffOperation::Add(k, v) => Some((k, v)),
                _ => None,
            })
            .collect::<Vec<_>>();
        assert_debug_snapshot!(to_add, @"[]");
        let to_change = patches
            .iter()
            .filter_map(|p| match p {
                EnvDiffOperation::Change(k, v) => Some((k, v)),
                _ => None,
            })
            .collect::<Vec<_>>();
        assert_debug_snapshot!(to_change, @r###"
        [
            (
                "b",
                "2",
            ),
        ]
        "###);
    }

    fn new_from_hashmap() -> HashMap<String, String> {
        HashMap::from([("a", "1"), ("b", "2")].map(|(k, v)| (k.into(), v.into())))
    }

    fn new_to_hashmap() -> HashMap<String, String> {
        HashMap::from([("a", "1"), ("b", "3"), ("c", "4")].map(|(k, v)| (k.into(), v.into())))
    }

    #[test]
    fn test_serialize() {
        let diff = EnvDiff::new(&new_from_hashmap(), new_to_hashmap());
        let serialized = diff.serialize().unwrap();
        let deserialized = EnvDiff::deserialize(&serialized).unwrap();
        assert_debug_snapshot!(deserialized.to_patches());
    }

    #[test]
    fn test_from_bash_script() {
        let path = dirs::HOME.join("fixtures/exec-env");
        let orig = indexmap! {
            "UNMODIFIED_VAR" => "unmodified",
            "MODIFIED_VAR" => "original",
        }
        .into_iter()
        .map(|(k, v)| (k.into(), v.into()))
        .collect::<Vec<(String, String)>>();
        let ed = EnvDiff::from_bash_script(path.as_path(), orig).unwrap();
        assert_debug_snapshot!(ed);
    }
}
