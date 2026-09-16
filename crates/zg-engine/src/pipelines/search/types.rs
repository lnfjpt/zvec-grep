//! Search routes and evidence shared with the public API.

use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct SearchRoute {
    pub mode: SearchRouteMode,
    pub query: String,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, Hash, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum SearchRouteMode {
    Fts,
    Vector,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum MatchedBy {
    Fts,
    Vector,
    #[serde(rename = "fts+vector")]
    FtsAndVector,
    Lexical,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct SearchHitTrace {
    pub recall: Vec<SearchRecallTrace>,
    pub fusion: SearchFusionTrace,
    pub final_selection: SearchFinalTrace,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct SearchRecallTrace {
    pub path: SearchRouteMode,
    pub route_id: String,
    pub query: String,
    pub found: bool,
    pub rank: Option<usize>,
    pub score: Option<f64>,
    pub forced: bool,
    pub reason: Option<String>,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct SearchFusionTrace {
    pub rank: usize,
    pub score: f64,
    pub forced: bool,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct SearchFinalTrace {
    pub returned_by_limit: bool,
    pub cutoff_rank: usize,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct TimingEntry {
    pub name: String,
    pub duration_micros: u64,
    pub count: Option<u64>,
}
