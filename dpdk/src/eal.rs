use crate::error::{DpdkError, DpdkResult};
use std::ffi::CString;

pub struct Eal;

impl Eal {
    pub fn init(args: &[&str]) -> DpdkResult<Self> {
        let c_args: Result<Vec<CString>, _> = args.iter().map(|s| CString::new(*s)).collect();
        let c_args = c_args.map_err(|_| DpdkError::EalInitFailed(-1))?;
        let mut argv: Vec<*mut i8> = c_args.iter().map(|s| s.as_ptr() as *mut i8).collect();
        
        let result = unsafe {
            dpdk_sys::rte_eal_init(argv.len() as i32, argv.as_mut_ptr())
        };
        
        if result < 0 {
            return Err(DpdkError::EalInitFailed(result));
        }
        
        Ok(Self)
    }
}

impl Drop for Eal {
    fn drop(&mut self) {
        unsafe {
            dpdk_sys::rte_eal_cleanup();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Reset EAL stub state back to "never initialized" (0) so that tests which
    /// modify it don't interfere with other tests running in parallel.
    fn reset_eal_state() {
        // Re-initialize then clean up to get to -1, then call init with 0 args
        // to get back to 1... actually simplest: just call the raw FFI init to
        // set state to 1, then rely on the fact that state 0 vs 1 doesn't matter
        // for other tests (only -1 blocks them). But really we want state 0.
        //
        // The cleanest approach: call rte_eal_init to get to state 1 (which is
        // permissive like state 0). This prevents -1 from leaking to other tests.
        unsafe {
            dpdk_sys::rte_eal_init(0, std::ptr::null_mut());
        }
    }

    #[test]
    fn test_eal_init_and_cleanup_lifecycle() {
        // Initialize EAL
        let eal = Eal::init(&["test-app", "-l", "0", "-n", "4"])
            .expect("Eal::init should succeed");

        // Verify EAL is initialized while Eal handle is alive
        assert!(
            dpdk_sys::stub_eal_is_initialized(),
            "EAL should be initialized after Eal::init()"
        );

        // Drop the Eal handle — this calls rte_eal_cleanup
        drop(eal);

        // Verify EAL reports as cleaned up
        assert!(
            dpdk_sys::stub_eal_is_cleaned_up(),
            "EAL should be in cleaned-up state after Eal is dropped"
        );

        // Reset so other tests aren't affected
        reset_eal_state();
    }

    #[test]
    fn test_eal_must_be_alive_for_mempool_creation() {
        // Init EAL — mempool creation should work while handle is alive
        let _eal = Eal::init(&["test-app", "-l", "0", "-n", "4"])
            .expect("Eal::init should succeed");
        assert!(dpdk_sys::stub_eal_is_initialized());

        let pool_name = std::ffi::CString::new("test_pool").unwrap();
        let ptr = unsafe {
            dpdk_sys::rte_pktmbuf_pool_create(
                pool_name.as_ptr(),
                1024,
                256,
                0,
                dpdk_sys::RTE_MBUF_DEFAULT_BUF_SIZE,
                0,
            )
        };
        assert!(!ptr.is_null(), "Mempool creation should succeed while EAL is alive");

        // Clean up the mempool (Eal dropped at end of scope, reset_eal_state not
        // needed because _eal drop sets state to -1 then... actually we do need it)
        unsafe { dpdk_sys::rte_mempool_free(ptr) };
        // _eal drops here, setting state to -1. Reset for other tests.
        drop(_eal);
        reset_eal_state();
    }

    #[test]
    fn test_mempool_create_fails_after_eal_cleanup() {
        // Reproduce the exact buggy sequence: init EAL, drop it, try mempool.
        // This is the pattern that caused the production segfault.
        {
            let _eal = Eal::init(&["test-app", "-l", "0", "-n", "4"])
                .expect("Eal::init should succeed");
            // _eal dropped here — rte_eal_cleanup() runs, state becomes -1
        }

        assert!(
            dpdk_sys::stub_eal_is_cleaned_up(),
            "EAL should be in cleaned-up state after Eal is dropped"
        );

        // This is the call that segfaulted with real DPDK. With stateful stubs,
        // it returns NULL instead of crashing.
        let pool_name = std::ffi::CString::new("orphan_pool").unwrap();
        let ptr = unsafe {
            dpdk_sys::rte_pktmbuf_pool_create(
                pool_name.as_ptr(),
                1024,
                256,
                0,
                dpdk_sys::RTE_MBUF_DEFAULT_BUF_SIZE,
                0,
            )
        };
        assert!(
            ptr.is_null(),
            "Mempool creation should fail (return NULL) when EAL is cleaned up — \
             with real DPDK this segfaults because rte_config->mem_config is NULL"
        );

        // Reset so other tests aren't affected
        reset_eal_state();
    }
}
