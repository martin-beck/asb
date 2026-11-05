//! Offline contract tests for the draft VM recipe.
//!
//! The recipe is not wired into ASB execution yet. Keeping its tests in the
//! ordinary Cargo test tree ensures future recipe changes run through CI.
#[path = "../src/emulation/recipe.rs"]
mod recipe;
