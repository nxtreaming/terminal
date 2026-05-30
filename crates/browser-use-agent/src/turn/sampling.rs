//! Sampling driver implementation (stream + retry + transport fallback).
//!
//! The concrete `SamplingDriver` over `browser_use_llm::route::ModelClient` lands in
//! WP-B5; it delegates retry branching to `decision::retry`. Scaffold placeholder.
