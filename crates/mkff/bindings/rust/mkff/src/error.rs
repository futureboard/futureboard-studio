use std::ffi::CStr;
use std::fmt;

/// Wraps an `MKFF_Result` error code. `MKFF_RESULT_OK` (and the
/// non-error sentinels `NOT_READY`/`END_OF_STREAM`, which callers see
/// through [`crate::decoder::ReceiveOutcome`] instead) never appear
/// here.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MkffError(pub mkff_sys::MKFF_Result);

impl fmt::Display for MkffError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let msg = unsafe {
            let ptr = mkff_sys::mkff_result_to_string(self.0);
            if ptr.is_null() {
                "unknown result".to_string()
            } else {
                CStr::from_ptr(ptr).to_string_lossy().into_owned()
            }
        };
        write!(f, "{msg}")
    }
}

impl std::error::Error for MkffError {}

pub type Result<T> = std::result::Result<T, MkffError>;

/// Converts a raw `MKFF_Result` into `Ok(())` / `Err(MkffError)`.
/// `NOT_READY` and `END_OF_STREAM` are treated as errors here since
/// this helper is only used by calls that don't have their own
/// tri-state outcome type.
pub(crate) fn check(result: mkff_sys::MKFF_Result) -> Result<()> {
    if result == mkff_sys::MKFF_Result::MKFF_RESULT_OK {
        Ok(())
    } else {
        Err(MkffError(result))
    }
}
