//! Rendering the desired set for Terraform (WS-B Task B6).
//!
//! Terraform consumes `desired_nodes.auto.tfvars.json` and drives
//! `for_each` over the map it contains, keyed on `mm_node_id`. That makes this
//! file the most destructive artifact in the programme: **every key missing from
//! it is a machine Terraform destroys.**
//!
//! Three consequences shape the API.
//!
//! **The write is atomic.** Temp file in the same directory, then rename. A crash
//! or a full disk mid-write otherwise leaves Terraform reading a truncated JSON
//! object — and a truncated map is not a parse error in the interesting case, it
//! is a *shorter* map. The same filesystem matters: `rename(2)` is only atomic
//! within one, so the temp file cannot go to `/tmp`.
//!
//! **An empty set renders `{}`, never `null`.** `for_each = null` is an error;
//! `{}` means "destroy everything", which is the correct desired state when
//! nothing is desired.
//!
//! **A large shrink needs saying out loud.** This is the same failure as the
//! orphan sweeper's: a partial read looks exactly like an intentional teardown.
//! [`TfvarsWriter`] compares against what is already on disk and refuses a write
//! that removes most of the fleet unless the caller states it means to — which
//! the runner does only for `fleet=off`.

use std::collections::BTreeMap;
use std::io::Write;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::desired::DesiredRow;

/// One entry of the Terraform `for_each` map.
///
/// Field names are the contract with `terraform/fleet/variables.tf`; renaming one
/// here without renaming it there makes Terraform destroy and recreate every node.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TfNode {
    pub flavor: String,
    pub ownership: String,
    pub region: String,
    pub size: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub broadcast_id: Option<String>,
    /// RFC 3339. Present for rented nodes only — Terraform does not act on it, but
    /// it makes `terraform plan` output readable by a human deciding whether a
    /// destroy is expected.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub destroy_deadline: Option<String>,
}

/// The whole file. One key, so the JSON is a valid `.auto.tfvars.json`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Tfvars {
    /// BTreeMap, not HashMap: the file is diffed by humans and committed to
    /// nothing, but a map that reorders on every render makes every `terraform
    /// plan` look like a change.
    pub desired_nodes: BTreeMap<String, TfNode>,
}

impl Tfvars {
    pub fn from_rows(rows: &[DesiredRow]) -> Self {
        Self {
            desired_nodes: rows
                .iter()
                .map(|r| {
                    (
                        r.mm_node_id.as_str().to_string(),
                        TfNode {
                            flavor: r.flavor.as_str().to_string(),
                            ownership: r.ownership.as_str().to_string(),
                            region: r.region.clone(),
                            size: r.size.clone(),
                            broadcast_id: r.broadcast_id.clone(),
                            destroy_deadline: r.destroy_deadline.map(|d| d.to_rfc3339()),
                        },
                    )
                })
                .collect(),
        }
    }

    pub fn len(&self) -> usize {
        self.desired_nodes.len()
    }

    pub fn is_empty(&self) -> bool {
        self.desired_nodes.is_empty()
    }
}

#[derive(Debug, thiserror::Error)]
pub enum TfvarsError {
    /// The write would remove most of the fleet and the caller did not say it
    /// meant to. Refusing is the whole point: a partial database read and a
    /// deliberate drain produce the same file.
    #[error(
        "refusing to write {new} node(s) over {existing}: that destroys {removed} machine(s). \
         A partial read looks exactly like an intentional teardown — pass allow_shrink if this \
         is one"
    )]
    UnexpectedShrink {
        existing: usize,
        new: usize,
        removed: usize,
    },

    #[error("rendering tfvars failed: {0}")]
    Serialize(#[from] serde_json::Error),

    #[error("writing {path}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
}

/// Writes `desired_nodes.auto.tfvars.json` into a Terraform working directory.
pub struct TfvarsWriter {
    path: PathBuf,
    /// A write that removes more than this fraction of the existing entries needs
    /// `allow_shrink`. 0.5 by default: losing half the fleet in one tick is either
    /// a drain the operator asked for or a bug, and both deserve to be explicit.
    shrink_threshold: f64,
}

impl TfvarsWriter {
    /// `dir` is the Terraform working directory. The file name is fixed, because
    /// `*.auto.tfvars.json` is loaded by Terraform automatically and a second file
    /// with a different name would be silently ignored.
    pub fn new(dir: impl AsRef<Path>) -> Self {
        Self {
            path: dir.as_ref().join("desired_nodes.auto.tfvars.json"),
            shrink_threshold: 0.5,
        }
    }

    pub fn with_shrink_threshold(mut self, fraction: f64) -> Self {
        self.shrink_threshold = fraction;
        self
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// What Terraform would read right now, or an empty set when the file does not
    /// exist yet.
    ///
    /// A file that exists but cannot be parsed is **not** treated as empty: that
    /// is the truncated-write case, and calling it empty would authorise
    /// destroying everything. It reports an error instead.
    pub fn read_current(&self) -> Result<Tfvars, TfvarsError> {
        match std::fs::read_to_string(&self.path) {
            Ok(text) => Ok(serde_json::from_str(&text)?),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Tfvars {
                desired_nodes: BTreeMap::new(),
            }),
            Err(source) => Err(TfvarsError::Io {
                path: self.path.clone(),
                source,
            }),
        }
    }

    /// Render and write atomically.
    ///
    /// `allow_shrink` bypasses the shrink guard. The runner passes it only for
    /// `fleet=off`, where removing the whole fleet is the instruction.
    pub fn write(&self, rows: &[DesiredRow], allow_shrink: bool) -> Result<Tfvars, TfvarsError> {
        let next = Tfvars::from_rows(rows);

        if !allow_shrink {
            let current = self.read_current()?;
            let removed = current
                .desired_nodes
                .keys()
                .filter(|k| !next.desired_nodes.contains_key(*k))
                .count();
            // A shrink is judged against what exists, not against zero: going from
            // 0 to 0 removes nothing and must not trip the guard.
            if removed > 0 && removed as f64 > current.len() as f64 * self.shrink_threshold {
                return Err(TfvarsError::UnexpectedShrink {
                    existing: current.len(),
                    new: next.len(),
                    removed,
                });
            }
        }

        // Pretty-printed with a trailing newline: this file ends up in `terraform
        // plan` output and in incident write-ups, and a single-line 8 KB JSON blob
        // is unreadable exactly when someone is trying to decide whether a destroy
        // is expected.
        let mut body = serde_json::to_string_pretty(&next)?;
        body.push('\n');

        let dir = self.path.parent().unwrap_or(Path::new("."));
        let tmp = dir.join(format!(
            ".desired_nodes.auto.tfvars.json.tmp-{}",
            std::process::id()
        ));

        let write_tmp = || -> std::io::Result<()> {
            let mut f = std::fs::File::create(&tmp)?;
            f.write_all(body.as_bytes())?;
            // Durability before the rename: without it a crash can leave the
            // renamed file present and empty, which is the truncated map again.
            f.sync_all()?;
            Ok(())
        };
        write_tmp().map_err(|source| TfvarsError::Io {
            path: tmp.clone(),
            source,
        })?;

        std::fs::rename(&tmp, &self.path).map_err(|source| TfvarsError::Io {
            path: self.path.clone(),
            source,
        })?;

        Ok(next)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::{TimeZone, Utc};
    use mm_core::fleet::{NodeFlavor, NodeId, Ownership};

    fn row(id: &str, ownership: Ownership, with_deadline: bool) -> DesiredRow {
        DesiredRow {
            mm_node_id: NodeId::new(id),
            flavor: NodeFlavor::Fanout,
            ownership,
            region: "eu-ams".into(),
            size: "small".into(),
            broadcast_id: Some("b1".into()),
            requested_at: Utc.timestamp_opt(1_700_000_000, 0).unwrap(),
            destroy_deadline: with_deadline
                .then(|| Utc.timestamp_opt(1_700_003_600, 0).unwrap()),
        }
    }

    fn tmpdir(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("mm-tfvars-{}-{}", name, std::process::id()));
        std::fs::create_dir_all(&d).expect("mkdir");
        d
    }

    #[test]
    fn the_rendered_json_round_trips() {
        let rows = [
            row("bc-b1-fanout-0", Ownership::Rented, true),
            row("bc-b1-fanout-1", Ownership::Rented, true),
        ];
        let rendered = Tfvars::from_rows(&rows);
        let text = serde_json::to_string(&rendered).expect("serialize");
        let back: Tfvars = serde_json::from_str(&text).expect("deserialize");
        assert_eq!(rendered, back);
        assert_eq!(back.len(), 2);
    }

    /// `for_each = null` is an error; `{}` is "destroy everything", which is the
    /// correct desired state when nothing is desired.
    #[test]
    fn an_empty_set_renders_an_empty_object_not_null() {
        let text = serde_json::to_string(&Tfvars::from_rows(&[])).expect("serialize");
        assert_eq!(text, r#"{"desired_nodes":{}}"#);
        assert!(!text.contains("null"), "for_each = null is a Terraform error");
    }

    #[test]
    fn keys_are_ordered_so_a_re_render_is_not_a_diff() {
        let a = Tfvars::from_rows(&[
            row("z", Ownership::Rented, true),
            row("a", Ownership::Rented, true),
            row("m", Ownership::Rented, true),
        ]);
        let first = serde_json::to_string(&a).expect("serialize");
        let second = serde_json::to_string(&Tfvars::from_rows(&[
            row("m", Ownership::Rented, true),
            row("z", Ownership::Rented, true),
            row("a", Ownership::Rented, true),
        ]))
        .expect("serialize");
        assert_eq!(first, second, "map order leaked into the file");
    }

    #[test]
    fn a_non_rented_node_carries_no_deadline_field_at_all() {
        let text =
            serde_json::to_string(&Tfvars::from_rows(&[row("own", Ownership::Owned, false)]))
                .expect("serialize");
        assert!(
            !text.contains("destroy_deadline"),
            "an absent deadline must be absent, not null: {text}"
        );
    }

    #[test]
    fn writing_creates_the_file_with_the_fixed_terraform_name() {
        let dir = tmpdir("create");
        let w = TfvarsWriter::new(&dir);
        assert!(w.path().ends_with("desired_nodes.auto.tfvars.json"));

        w.write(&[row("n0", Ownership::Rented, true)], false)
            .expect("write");
        let text = std::fs::read_to_string(w.path()).expect("read back");
        assert!(text.ends_with('\n'), "file must end with a newline");
        assert_eq!(w.read_current().expect("reread").len(), 1);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn no_temp_files_survive_a_successful_write() {
        let dir = tmpdir("notmp");
        let w = TfvarsWriter::new(&dir);
        w.write(&[row("n0", Ownership::Rented, true)], false)
            .expect("write");

        let leftovers: Vec<String> = std::fs::read_dir(&dir)
            .expect("readdir")
            .filter_map(|e| e.ok())
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .filter(|n| n.contains(".tmp-"))
            .collect();
        assert!(leftovers.is_empty(), "temp files left behind: {leftovers:?}");
        std::fs::remove_dir_all(&dir).ok();
    }

    /// THE GUARD. A partial database read and a deliberate drain produce the same
    /// file, and Terraform destroys every key that is missing from it.
    #[test]
    fn a_large_shrink_is_refused_unless_the_caller_says_it_means_it() {
        let dir = tmpdir("shrink");
        let w = TfvarsWriter::new(&dir);
        let four: Vec<DesiredRow> = (0..4)
            .map(|i| row(&format!("n{i}"), Ownership::Rented, true))
            .collect();
        w.write(&four, false).expect("initial write");

        // Down to one: three of four removed, past the 50% threshold.
        let err = w
            .write(&four[..1], false)
            .expect_err("a 75% shrink must be refused");
        assert!(
            matches!(err, TfvarsError::UnexpectedShrink { removed: 3, existing: 4, .. }),
            "got {err}"
        );
        assert_eq!(
            w.read_current().expect("reread").len(),
            4,
            "a refused write must leave the file untouched"
        );

        // Same write, stated as intentional.
        w.write(&four[..1], true).expect("allow_shrink must permit it");
        assert_eq!(w.read_current().expect("reread").len(), 1);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn a_small_shrink_passes_without_ceremony() {
        let dir = tmpdir("small");
        let w = TfvarsWriter::new(&dir);
        let four: Vec<DesiredRow> = (0..4)
            .map(|i| row(&format!("n{i}"), Ownership::Rented, true))
            .collect();
        w.write(&four, false).expect("initial");
        w.write(&four[..3], false)
            .expect("one of four removed is ordinary scale-down");
        assert_eq!(w.read_current().expect("reread").len(), 3);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn growing_from_nothing_is_not_a_shrink() {
        let dir = tmpdir("grow");
        let w = TfvarsWriter::new(&dir);
        w.write(&[], false).expect("empty over nothing removes nothing");
        w.write(
            &(0..3)
                .map(|i| row(&format!("n{i}"), Ownership::Rented, true))
                .collect::<Vec<_>>(),
            false,
        )
        .expect("growth is never a shrink");
        assert_eq!(w.read_current().expect("reread").len(), 3);
        std::fs::remove_dir_all(&dir).ok();
    }

    /// A file that exists but will not parse is the truncated-write case. Treating
    /// it as empty would compute "nothing is being removed" and authorise
    /// destroying the entire fleet on the next write.
    #[test]
    fn an_unparseable_existing_file_is_an_error_not_an_empty_set() {
        let dir = tmpdir("corrupt");
        let w = TfvarsWriter::new(&dir);
        std::fs::write(w.path(), "{\"desired_nodes\": {\"n0\": {\"flav")
            .expect("write a truncated file");

        let err = w.read_current().expect_err("a truncated map must not read as empty");
        assert!(matches!(err, TfvarsError::Serialize(_)), "got {err}");

        // And a guarded write over it refuses rather than proceeding.
        assert!(w.write(&[], false).is_err());
        std::fs::remove_dir_all(&dir).ok();
    }

    /// The cross-language contract, pinned by a golden file that **Terraform
    /// itself validates**.
    ///
    /// `TfNode`'s field names are `variables.tf`'s attribute names. Renaming one on
    /// either side alone does not fail to compile and does not fail to parse — it
    /// makes Terraform see a different `for_each` value shape and destroy and
    /// recreate every node in the fleet. A golden file is the only thing that fails
    /// when exactly one side changes: this test catches a Rust-side rename, and
    /// `terraform validate` on the same file catches an HCL-side one.
    #[test]
    fn the_rendered_shape_matches_the_terraform_golden() {
        let golden_path = concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../terraform/fleet/testdata/example.tfvars.json"
        );
        let golden = std::fs::read_to_string(golden_path)
            .expect("the golden file must exist — terraform validate runs against it");

        // Built explicitly rather than via `row()`, so the golden is representative:
        // one rented fan-out tied to a broadcast, and one owned origin tied to none.
        let rows = [
            DesiredRow {
                mm_node_id: NodeId::new("bc-b1-fanout-0"),
                flavor: NodeFlavor::Fanout,
                ownership: Ownership::Rented,
                region: "eu-ams".into(),
                size: "small".into(),
                broadcast_id: Some("b1".into()),
                requested_at: Utc.timestamp_opt(1_700_000_000, 0).unwrap(),
                destroy_deadline: Some(Utc.timestamp_opt(1_700_003_600, 0).unwrap()),
            },
            DesiredRow {
                mm_node_id: NodeId::new("origin-1"),
                flavor: NodeFlavor::Origin,
                ownership: Ownership::Owned,
                region: "eu-ams".into(),
                size: "2u".into(),
                broadcast_id: None,
                requested_at: Utc.timestamp_opt(1_700_000_000, 0).unwrap(),
                destroy_deadline: None,
            },
        ];
        let mut rendered = serde_json::to_string_pretty(&Tfvars::from_rows(&rows))
            .expect("serialize");
        rendered.push('\n');

        assert_eq!(
            rendered, golden,
            "the renderer and terraform/fleet/testdata/example.tfvars.json disagree. \
             If this was a deliberate shape change, update variables.tf AND the golden \
             together — a rename on one side alone destroys and recreates the fleet."
        );
    }

    #[test]
    fn the_terraform_field_names_are_pinned() {
        // These names are the contract with terraform/fleet/variables.tf. Renaming
        // one here without renaming it there makes Terraform destroy and recreate
        // every node in the fleet.
        let text = serde_json::to_string(&Tfvars::from_rows(&[row(
            "n0",
            Ownership::Rented,
            true,
        )]))
        .expect("serialize");
        for field in [
            "desired_nodes",
            "flavor",
            "ownership",
            "region",
            "size",
            "broadcast_id",
            "destroy_deadline",
        ] {
            assert!(text.contains(field), "missing field `{field}` in {text}");
        }
    }
}
