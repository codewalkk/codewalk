//! Size-scaled explore budgets — port of `getExploreBudget` /
//! `getExploreOutputBudget` (mcp/tools.ts). Output scales monotonically with
//! indexed file count and stays under the host's ~25K inline-result cap.

/// Recommended number of `codegraph_explore` calls for a project of `file_count`.
pub fn get_explore_budget(file_count: usize) -> usize {
    if file_count < 500 {
        1
    } else if file_count < 5000 {
        2
    } else if file_count < 15000 {
        3
    } else if file_count < 25000 {
        4
    } else {
        5
    }
}

/// Adaptive output budget for one `codegraph_explore` response.
#[derive(Debug, Clone, Copy)]
pub struct ExploreOutputBudget {
    pub max_output_chars: usize,
    pub default_max_files: usize,
    pub max_chars_per_file: usize,
    pub gap_threshold: u32,
    pub max_symbols_in_file_header: usize,
    pub max_edges_per_relationship_kind: usize,
    pub include_relationships: bool,
    pub include_completeness_signal: bool,
    pub include_budget_note: bool,
}

/// Port of `getExploreOutputBudget` — tiered by file count, capped ~24K so the
/// response is never externalized by the host.
pub fn get_explore_output_budget(file_count: usize) -> ExploreOutputBudget {
    if file_count < 150 {
        ExploreOutputBudget {
            max_output_chars: 13000,
            default_max_files: 4,
            max_chars_per_file: 3800,
            gap_threshold: 7,
            max_symbols_in_file_header: 5,
            max_edges_per_relationship_kind: 4,
            include_relationships: false,
            include_completeness_signal: false,
            include_budget_note: false,
        }
    } else if file_count < 500 {
        ExploreOutputBudget {
            max_output_chars: 18000,
            default_max_files: 5,
            max_chars_per_file: 3800,
            gap_threshold: 8,
            max_symbols_in_file_header: 6,
            max_edges_per_relationship_kind: 6,
            include_relationships: false,
            include_completeness_signal: false,
            include_budget_note: false,
        }
    } else if file_count < 5000 {
        ExploreOutputBudget {
            max_output_chars: 24000,
            default_max_files: 8,
            max_chars_per_file: 6500,
            gap_threshold: 12,
            max_symbols_in_file_header: 10,
            max_edges_per_relationship_kind: 10,
            include_relationships: true,
            include_completeness_signal: true,
            include_budget_note: true,
        }
    } else {
        // 5000+ (and 15000+): same ~24K ceiling; more files → more CALLS, not a bigger response.
        ExploreOutputBudget {
            max_output_chars: 24000,
            default_max_files: 8,
            max_chars_per_file: 7000,
            gap_threshold: 15,
            max_symbols_in_file_header: 15,
            max_edges_per_relationship_kind: 15,
            include_relationships: true,
            include_completeness_signal: true,
            include_budget_note: true,
        }
    }
}
