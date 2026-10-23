pub(crate) mod assemble;
pub(crate) mod cut;
pub(crate) mod diff;
pub(crate) mod redact;

pub(crate) const BUNDLE_FORMAT: &str = "witnessed-bundle";
pub(crate) const BUNDLE_FORMATS_ACCEPTED: &[&str] = &["witnessed-bundle", "agentrecorder-bundle"];
pub(crate) const BUNDLE_VERSION: u32 = 1;
