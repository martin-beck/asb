pub(crate) mod evaluation;

use super::PromptAdapter;

/// One adapter covers the bundled prompt family. Its matching and materialized
/// fixture behavior are generic over the configured prompt identifier.
pub(super) static ADAPTER: PromptAdapter = PromptAdapter;

#[cfg(test)]
mod tests {
    use crate::workloads::adapters::find;
    use std::fs;

    #[test]
    fn response_review_declares_the_graded_answer_not_workspace_changes() {
        use crate::workloads::contracts::SubmissionKind;
        let response = crate::workloads::descriptor("repository-navigation").unwrap();
        assert_eq!(response.protocol.submission, SubmissionKind::ResponseText);
        assert_eq!(
            crate::workloads::descriptor("bug-fix")
                .unwrap()
                .protocol
                .submission,
            SubmissionKind::Workspace
        );
    }

    #[test]
    fn prompt_family_routes_prompt_population_fixture_setup_and_trials() {
        let root = std::env::temp_dir().join(format!("asb-prompt-adapter-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(&root).unwrap();
        let adapter = find("bug-fix").expect("built-in prompt adapter");
        assert_eq!(adapter.id(), "prompt");
        assert!(adapter.populate_prompts(&root).unwrap() > 0);
        assert!(root.join("bug-fix.txt").is_file());
        let workspace = root.join("workspace");
        assert!(adapter.prepare_fixture("bug-fix", &workspace).unwrap() > 0);
        assert!(workspace.join("tests/test_parser.py").is_file());
        assert_eq!(adapter.trial_ids().len(), 3);
        let _ = fs::remove_dir_all(root);
    }
}
