//! Pinned official SWE-ReBench instance catalog.

use super::{SetupExtension, WorkloadAdapter};
use crate::workloads::{InstallationContext, Trial};
use parquet::file::reader::{FileReader, SerializedFileReader};
use parquet::record::Field;
use serde::{Deserialize, Serialize};
use std::collections::BTreeSet;
use std::fs;
use std::io::Write;
use std::path::Path;

const REVISION: &str = "89cdfbab4ab1bd8f5a658bb212d1b63624f4f881";
const PART_ONE: &str = "https://huggingface.co/datasets/nebius/SWE-rebench/resolve/89cdfbab4ab1bd8f5a658bb212d1b63624f4f881/data/test-00000-of-00002.parquet?download=true";
const PART_TWO: &str = "https://huggingface.co/datasets/nebius/SWE-rebench/resolve/89cdfbab4ab1bd8f5a658bb212d1b63624f4f881/data/test-00001-of-00002.parquet?download=true";
const MARKER: &str = "nebius/SWE-rebench@89cdfbab4ab1bd8f5a658bb212d1b63624f4f881";
const LIMIT: u64 = 256 * 1024 * 1024;
const EXPECTED_TASKS: usize = 21_336;

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct Catalog {
    schema_version: u32,
    marker: String,
    tasks: Vec<Task>,
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct Task {
    id: String,
    description: String,
}

pub(super) struct SweRebenchAdapter;
pub(super) static ADAPTER: SweRebenchAdapter = SweRebenchAdapter;

impl WorkloadAdapter for SweRebenchAdapter {
    fn id(&self) -> &'static str {
        "swe-rebench"
    }
    fn source(&self) -> Option<&'static str> {
        Some(PART_ONE)
    }
    fn setup_extension(&self) -> Option<SetupExtension> {
        Some(SetupExtension::trials())
    }
    fn installed_available(&self, directory: &Path) -> Option<bool> {
        Some(read(directory).is_ok())
    }
    fn installed_trials(&self, directory: &Path) -> Result<Vec<Trial>, String> {
        read(directory)
    }
    fn trial_ids(&self) -> Vec<Trial> {
        Vec::new()
    }

    fn install(
        &self,
        context: &InstallationContext<'_>,
        output: &mut dyn Write,
        progress: &mut dyn FnMut(u8, crate::install::ProgressStage),
    ) -> crate::install::InstallResult<bool> {
        if !PART_ONE.contains(REVISION) || !PART_TWO.contains(REVISION) {
            return Err(crate::Error::Input(
                "SWE-ReBench installer has inconsistent pinned dataset URLs".into(),
            )
            .into());
        }
        let root = context.workloads_root.join(self.id());
        fs::create_dir_all(&root).map_err(|error| crate::Error::Io(error.to_string()))?;
        progress(20, crate::install::ProgressStage::Downloading);
        crate::install::install_source_bounded_in(
            context.process,
            PART_ONE,
            &root.join("test-00000-of-00002.parquet"),
            "SWE-ReBench official dataset part 1",
            LIMIT,
        )?;
        progress(60, crate::install::ProgressStage::Downloading);
        crate::install::install_source_bounded_in(
            context.process,
            PART_TWO,
            &root.join("test-00001-of-00002.parquet"),
            "SWE-ReBench official dataset part 2",
            LIMIT,
        )?;
        fs::write(root.join("ASB-DATASET"), format!("{MARKER}\n"))
            .map_err(|error| crate::Error::Io(error.to_string()))?;
        write_catalog(context.workloads_root).map_err(crate::Error::Input)?;
        fs::write(
            context.workloads_root.join("swe-rebench.txt"),
            "Use the selected installed SWE-ReBench instance in `external-workload`.\n",
        )
        .map_err(|error| crate::Error::Io(error.to_string()))?;
        crate::workloads::materialize_external_adapter(self.id(), context.workloads_root)
            .map_err(|error| crate::Error::Io(error.to_string()))?;
        crate::workloads::write_default_trial_set(self.id(), context.workloads_root)
            .map_err(|error| crate::Error::Io(error.to_string()))?;
        writeln!(
            output,
            "Installed SWE-ReBench official task catalog at {}",
            root.display()
        )
        .map_err(|error| crate::Error::Io(error.to_string()))?;
        progress(100, crate::install::ProgressStage::Ready);
        Ok(true)
    }

    fn prepare_source(&self, workloads: &Path, workspace: &Path) -> Result<usize, String> {
        super::prepare_external_source(self.id(), workloads, workspace)
    }
}

fn parse_part(path: &Path) -> Result<Vec<Trial>, String> {
    let metadata = fs::symlink_metadata(path).map_err(|error| error.to_string())?;
    if !metadata.is_file() || metadata.len() > LIMIT {
        return Err("dataset part is not a bounded regular file".into());
    }
    let reader =
        SerializedFileReader::new(fs::File::open(path).map_err(|error| error.to_string())?)
            .map_err(|error| format!("invalid official Parquet data: {error}"))?;
    let mut ids = BTreeSet::new();
    for row in reader
        .get_row_iter(None)
        .map_err(|error| format!("cannot read official Parquet rows: {error}"))?
    {
        let row = row.map_err(|error| format!("cannot decode official Parquet row: {error}"))?;
        let id = row
            .get_column_iter()
            .find_map(|(name, value)| {
                (name == "instance_id").then(|| match value {
                    Field::Str(id) => Ok(id.clone()),
                    _ => Err("official instance_id is not a string"),
                })
            })
            .ok_or("official Parquet data has no instance_id column")??;
        if !ids.insert(id.clone()) {
            return Err("official dataset part has duplicate instance ids".into());
        }
    }
    if ids.is_empty() {
        return Err("official dataset part contains no instances".into());
    }
    Ok(ids
        .into_iter()
        .map(|id| Trial {
            description: format!("SWE-ReBench official instance {id}"),
            id,
        })
        .collect())
}

fn source_tasks(root: &Path) -> Result<Vec<Trial>, String> {
    let mut tasks = parse_part(&root.join("test-00000-of-00002.parquet"))?;
    tasks.extend(parse_part(&root.join("test-00001-of-00002.parquet"))?);
    if tasks.len() != EXPECTED_TASKS {
        return Err(format!(
            "official SWE-ReBench dataset has {} instances, expected {EXPECTED_TASKS}",
            tasks.len()
        ));
    }
    if tasks
        .iter()
        .map(|task| &task.id)
        .collect::<BTreeSet<_>>()
        .len()
        != tasks.len()
    {
        return Err("official SWE-ReBench dataset has duplicate instance ids".into());
    }
    Ok(tasks)
}

fn write_catalog(directory: &Path) -> Result<(), String> {
    let root = directory.join("swe-rebench");
    let tasks = source_tasks(&root)?;
    let bytes = serde_json::to_vec_pretty(&Catalog {
        schema_version: 1,
        marker: MARKER.into(),
        tasks: tasks
            .into_iter()
            .map(|task| Task {
                id: task.id,
                description: task.description,
            })
            .collect(),
    })
    .map_err(|error| error.to_string())?;
    fs::write(root.join("catalog.json"), bytes).map_err(|error| error.to_string())
}

fn read(directory: &Path) -> Result<Vec<Trial>, String> {
    let root = directory.join("swe-rebench");
    if fs::read_to_string(root.join("ASB-DATASET"))
        .ok()
        .as_deref()
        .map(str::trim)
        != Some(MARKER)
    {
        return Err(
            "SWE-ReBench has no pinned installed dataset; run `asb workload install swe-rebench`"
                .into(),
        );
    }
    let catalog: Catalog = serde_json::from_slice(
        &fs::read(root.join("catalog.json")).map_err(|_| "SWE-ReBench catalog missing")?,
    )
    .map_err(|_| "SWE-ReBench catalog invalid")?;
    let tasks = catalog
        .tasks
        .into_iter()
        .map(|task| Trial {
            id: task.id,
            description: task.description,
        })
        .collect::<Vec<_>>();
    if catalog.schema_version != 1
        || catalog.marker != MARKER
        || tasks.iter().map(|task| &task.id).collect::<Vec<_>>()
            != source_tasks(&root)?
                .iter()
                .map(|task| &task.id)
                .collect::<Vec<_>>()
    {
        return Err("SWE-ReBench catalog differs from its installed dataset; reinstall".into());
    }
    Ok(tasks)
}

#[cfg(test)]
mod tests {
    use super::*;
    use parquet::data_type::{ByteArray, ByteArrayType};
    use parquet::file::writer::SerializedFileWriter;
    use parquet::schema::parser::parse_message_type;
    use std::sync::Arc;

    fn write_part(path: &Path, start: usize, count: usize) {
        let schema = Arc::new(
            parse_message_type("message tasks { REQUIRED BINARY instance_id (UTF8); }").unwrap(),
        );
        let mut writer =
            SerializedFileWriter::new(fs::File::create(path).unwrap(), schema, Default::default())
                .unwrap();
        let mut group = writer.next_row_group().unwrap();
        let mut column = group.next_column().unwrap().unwrap();
        let values = (start..start + count)
            .map(|index| ByteArray::from(format!("project__issue-{index}").into_bytes()))
            .collect::<Vec<_>>();
        column
            .typed::<ByteArrayType>()
            .write_batch(&values, None, None)
            .unwrap();
        column.close().unwrap();
        group.close().unwrap();
        writer.close().unwrap();
    }

    #[test]
    fn local_parquet_fixtures_enumerate_and_load_without_download() {
        let root = std::env::temp_dir().join(format!("asb-swe-rebench-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        let dataset = root.join("swe-rebench");
        fs::create_dir_all(&dataset).unwrap();
        write_part(
            &dataset.join("test-00000-of-00002.parquet"),
            0,
            EXPECTED_TASKS / 2,
        );
        write_part(
            &dataset.join("test-00001-of-00002.parquet"),
            EXPECTED_TASKS / 2,
            EXPECTED_TASKS / 2,
        );
        fs::write(dataset.join("ASB-DATASET"), format!("{MARKER}\n")).unwrap();
        write_catalog(&root).unwrap();
        let tasks = read(&root).unwrap();
        assert_eq!(tasks.len(), EXPECTED_TASKS);
        assert_eq!(tasks[0].id, "project__issue-0");
        assert_eq!(tasks[EXPECTED_TASKS - 1].id, "project__issue-21335");
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn pin_is_present_in_both_primary_dataset_urls() {
        assert!(PART_ONE.contains(REVISION));
        assert!(PART_TWO.contains(REVISION));
    }
}
