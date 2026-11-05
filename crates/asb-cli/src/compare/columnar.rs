//! Bounded, offline long-form export of the same validated report inputs.
use super::*;
mod rows;
#[cfg(test)]
mod tests;
mod writer;
use rows::{cohorts, observations};
use writer::{Export, failure};

pub(super) fn publish(stage: &Path, prepared: &PreparedReport) -> Result<(), Error> {
    let mut export = Export::new(&stage.join("observations.parquet"))?;
    for benchmark in &prepared.data.rows {
        observations(&mut export, benchmark)?;
    }
    cohorts(
        &mut export,
        &serde_json::to_value(&prepared.cohort_analysis).map_err(failure)?,
    )?;
    export.close()?;
    let path = stage.join("report.html");
    let html = fs::read_to_string(&path).map_err(failure)?;
    let link = "<section class=\"explain\"><strong>Columnar observations</strong><p><a href=\"observations.parquet\">Download Parquet measurements and analysis</a>. Units, scope, availability, exact counters and evidence remain explicit. Time axes retain their collector-specific origins.</p></section>";
    if !html.contains("</main>") {
        return Err(failure("report template has no export insertion point"));
    }
    fs::write(path, html.replacen("</main>", &format!("{link}</main>"), 1)).map_err(failure)
}
