use crate::{utils::parse_duration_from_secs, version::P2P_VERSION};
use clap::{builder::RangedU64ValueParser, Args};
use std::time::Duration;

/// Parameters for configuring the Payload Builder
#[derive(Debug, Args, PartialEq, Default)]
pub struct PayloadBuilderArgs {
    /// Extra block data set by the builder.
    #[arg(long = "builder.extradata", help_heading = "Builder", default_value = P2P_VERSION)]
    pub extradata: String,

    /// Target gas ceiling for built blocks.
    #[arg(
        long = "builder.gaslimit",
        help_heading = "Builder",
        default_value = "30000000",
        value_name = "GAS_LIMIT"
    )]
    pub max_gas_limit: u64,

    /// The interval at which the job should build a new payload after the last (in seconds).
    #[arg(long = "builder.interval", help_heading = "Builder", value_parser = parse_duration_from_secs, default_value = "1", value_name = "SECONDS")]
    pub interval: Duration,

    /// The deadline for when the payload builder job should resolve.
    #[arg(long = "builder.deadline", help_heading = "Builder", value_parser = parse_duration_from_secs, default_value = "12", value_name = "SECONDS")]
    pub deadline: Duration,

    /// Maximum number of tasks to spawn for building a payload.
    #[arg(long = "builder.max-tasks", help_heading = "Builder", default_value = "3", value_parser = RangedU64ValueParser::<usize>::new().range(1..))]
    pub max_payload_tasks: usize,
}

#[cfg(test)]
mod tests {

    use super::*;
    use clap::{Args, Parser};

    /// A helper type to parse Args more easily
    #[derive(Parser)]
    struct CommandParser<T: Args> {
        #[clap(flatten)]
        args: T,
    }

    #[test]
    fn test_args_with_valid_max_tasks() {
        let args =
            CommandParser::<PayloadBuilderArgs>::parse_from(["reth", "--builder.max-tasks", "1"])
                .args;
        assert_eq!(args.max_payload_tasks, 1)
    }

    #[test]
    fn test_args_with_invalid_max_tasks() {
        assert!(CommandParser::<PayloadBuilderArgs>::try_parse_from([
            "reth",
            "--builder.max-tasks",
            "0"
        ])
        .is_err());
    }
}