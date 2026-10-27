//! Re-export of the engine configuration, so call sites say `config::Config`
//! without knowing it lives beside the router.

pub use crate::router::{ChooserConfig, ChooserKind, Config, Defaults};
