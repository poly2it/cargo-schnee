use cargo::ops::CompileFilter;

/// The targets of the selected packages that a command compiles, taken
/// from the same flags as Cargo's.
#[derive(clap::Args, Clone, Debug, Default, PartialEq, Eq)]
pub struct TargetSelection {
    /// Select only this package's library
    #[arg(long)]
    pub lib: bool,
    /// Select only the specified binary (can be specified multiple times)
    #[arg(long = "bin", value_name = "NAME")]
    pub bins: Vec<String>,
    /// Select all binaries
    #[arg(long = "bins")]
    pub all_bins: bool,
    /// Select only the specified test target (can be specified multiple times)
    #[arg(long = "test", value_name = "NAME")]
    pub tests: Vec<String>,
    /// Select all test targets
    #[arg(long = "tests")]
    pub all_tests: bool,
    /// Select only the specified example (can be specified multiple times)
    #[arg(long = "example", value_name = "NAME")]
    pub examples: Vec<String>,
    /// Select all examples
    #[arg(long = "examples")]
    pub all_examples: bool,
    /// Select only the specified bench target (can be specified multiple times)
    #[arg(long = "bench", value_name = "NAME")]
    pub benches: Vec<String>,
    /// Select all bench targets
    #[arg(long = "benches")]
    pub all_benches: bool,
    /// Select all targets, which are the library, binaries, tests, examples
    /// and benches
    #[arg(long)]
    pub all_targets: bool,
}

impl TargetSelection {
    pub fn bins(bins: impl IntoIterator<Item = String>) -> Self {
        Self {
            bins: bins.into_iter().collect(),
            ..Self::default()
        }
    }

    /// The filter Cargo's own command line builds from these flags. Without
    /// a target flag it skips targets whose required features are off,
    /// where `CompileOptions::new`'s default filter fails the plan on them.
    pub fn compile_filter(&self) -> CompileFilter {
        CompileFilter::from_raw_arguments(
            self.lib,
            self.bins.clone(),
            self.all_bins,
            self.tests.clone(),
            self.all_tests,
            self.examples.clone(),
            self.all_examples,
            self.benches.clone(),
            self.all_benches,
            self.all_targets,
        )
    }

    /// The flags that select these targets, in a fixed order, so that the
    /// unit-graph key and a `compute-graph` invocation name a selection
    /// identically.
    pub fn args(&self) -> Vec<String> {
        let flag = |on: bool, name: &str| on.then(|| name.to_string());
        let named = |values: &[String], name: &str| {
            values
                .iter()
                .flat_map(|v| [name.to_string(), v.clone()])
                .collect::<Vec<_>>()
        };
        flag(self.lib, "--lib")
            .into_iter()
            .chain(named(&self.bins, "--bin"))
            .chain(flag(self.all_bins, "--bins"))
            .chain(named(&self.tests, "--test"))
            .chain(flag(self.all_tests, "--tests"))
            .chain(named(&self.examples, "--example"))
            .chain(flag(self.all_examples, "--examples"))
            .chain(named(&self.benches, "--bench"))
            .chain(flag(self.all_benches, "--benches"))
            .chain(flag(self.all_targets, "--all-targets"))
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::Parser;

    #[derive(Parser)]
    struct Cli {
        #[command(flatten)]
        selection: TargetSelection,
    }

    /// `args` must parse back to the selection it came from, or a
    /// `compute-graph` run would key its graph differently from the build
    /// that asked for it.
    #[test]
    fn args_round_trip() {
        let selection = TargetSelection {
            lib: true,
            bins: vec!["a-b".into(), "c".into()],
            tests: vec!["t".into()],
            all_examples: true,
            all_benches: true,
            ..TargetSelection::default()
        };
        let parsed = Cli::parse_from(std::iter::once("x".to_string()).chain(selection.args()));
        assert_eq!(parsed.selection, selection);
    }

    #[test]
    fn no_flags_keeps_required_features_filterable() {
        assert!(matches!(
            TargetSelection::default().compile_filter(),
            CompileFilter::Default {
                required_features_filterable: true
            }
        ));
    }
}
