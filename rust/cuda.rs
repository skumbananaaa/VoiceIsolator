use std::{ffi::CStr, ptr, slice};

use anyhow::{Context, Result, bail, ensure};
use ort_sys::{OrtApi, OrtGetApiBase, OrtStatusPtr};

fn check(api: &OrtApi, status: OrtStatusPtr) -> Result<()> {
    if status.0.is_null() {
        return Ok(());
    }
    // ONNX Runtime owns the message until its status is released.
    let message = unsafe {
        let message = CStr::from_ptr((api.GetErrorMessage)(status.0))
            .to_string_lossy()
            .into_owned();
        (api.ReleaseStatus)(status.0);
        message
    };
    bail!("{message}")
}

pub(crate) fn ensure_ready() -> Result<()> {
    // Use the same loaded runtime and CUDA registration API as the pinned sherpa build.
    let api = unsafe {
        let base = OrtGetApiBase()
            .as_ref()
            .context("ONNX Runtime did not expose its C API")?;
        (base.GetApi)(ort_sys::ORT_API_VERSION).as_ref()
    }
    .context("The installed ONNX Runtime is incompatible with this build")?;

    let mut providers = ptr::null_mut();
    let mut count = 0;
    let has_cuda = unsafe {
        check(api, (api.GetAvailableProviders)(&mut providers, &mut count))?;
        let found = count > 0
            && !providers.is_null()
            && slice::from_raw_parts(providers, count as usize)
                .iter()
                .any(|&name| CStr::from_ptr(name) == c"CUDAExecutionProvider");
        check(api, (api.ReleaseAvailableProviders)(providers, count))?;
        found
    };
    ensure!(
        has_cuda,
        "CUDA is unavailable in this build. Install the CUDA-enabled runtime or select CPU."
    );

    // Listing CUDA is insufficient. Appending it also loads the provider and its
    // native dependencies. Sherpa treats this call's failure as a CPU fallback.
    unsafe {
        let mut session = ptr::null_mut();
        check(api, (api.CreateSessionOptions)(&mut session))?;
        let result = (|| {
            let mut cuda = ptr::null_mut();
            check(api, (api.CreateCUDAProviderOptions)(&mut cuda))?;
            let result = check(
                api,
                (api.SessionOptionsAppendExecutionProvider_CUDA_V2)(session, cuda),
            );
            (api.ReleaseCUDAProviderOptions)(cuda);
            result
        })();
        (api.ReleaseSessionOptions)(session);
        result.context("CUDA could not start. Check the NVIDIA driver, CUDA 12 and cuDNN 9 libraries, or select CPU")
    }
}
