//! Services: the parts of the domain that talk to the machine.

pub mod editor;
pub mod images;
pub mod runtime;

pub use images::{
    DEFAULT_MAX_IMAGE_BYTES, ImageError, ImageFormat, ImagePolicy, MAX_CONCURRENT_IMAGE_READS,
    PreparedImage, PreparedImageResult, prepare, prepare_async, prepare_async_with, prepare_bytes,
    prepare_bytes_with, prepare_many, prepare_with, sniff,
};
pub use runtime::{
    BinaryHealth, BinarySpec, DiscoveryError, RuntimeDiscovery, RuntimeLaunch, Validation,
    all_specs, build_launch_environment, claude_spec, codex_spec, detect_quarantine, expand_glob,
};
