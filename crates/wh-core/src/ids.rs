use serde::{Deserialize, Serialize};
use std::{fmt, str::FromStr};
use uuid::Uuid;

macro_rules! id_type {
    ($name:ident, $prefix:literal) => {
        #[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
        #[serde(transparent)]
        pub struct $name(pub Uuid);

        impl $name {
            /// Prefix of the event stream owned by this aggregate, e.g. `task-<uuid>`.
            pub const STREAM_PREFIX: &'static str = $prefix;

            pub fn new() -> Self {
                Self(Uuid::now_v7())
            }

            pub fn stream_id(&self) -> String {
                format!("{}-{}", $prefix, self.0)
            }

            /// Parse an id back out of a stream id produced by [`Self::stream_id`].
            pub fn from_stream_id(stream_id: &str) -> Option<Self> {
                stream_id
                    .strip_prefix(concat!($prefix, "-"))
                    .and_then(|s| s.parse().ok())
                    .map(Self)
            }
        }

        impl Default for $name {
            fn default() -> Self {
                Self::new()
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                self.0.fmt(f)
            }
        }

        impl FromStr for $name {
            type Err = uuid::Error;
            fn from_str(s: &str) -> Result<Self, Self::Err> {
                Ok(Self(s.parse()?))
            }
        }
    };
}

id_type!(FactoryId, "factory");
id_type!(TaskId, "task");
id_type!(RunId, "run");
id_type!(SessionId, "session");
id_type!(ProposalId, "proposal");
id_type!(ScoreId, "score");
id_type!(BenchmarkRunId, "bench");

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stream_id_round_trips() {
        let id = TaskId::new();
        assert_eq!(TaskId::from_stream_id(&id.stream_id()), Some(id));
        assert_eq!(TaskId::from_stream_id(&RunId::new().stream_id()), None);
    }
}
