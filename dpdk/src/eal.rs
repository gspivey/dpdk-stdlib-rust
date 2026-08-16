use crate::error::{DpdkError, DpdkResult};
use std::ffi::CString;

pub struct Eal;

impl Eal {
    pub fn init(args: &[&str]) -> DpdkResult<Self> {
        let c_args: Result<Vec<CString>, _> = args.iter().map(|s| CString::new(*s)).collect();
        let c_args = c_args.map_err(|_| DpdkError::EalInitFailed(-1))?;
        let mut argv: Vec<*mut std::os::raw::c_char> = c_args.iter().map(|s| s.as_ptr() as *mut std::os::raw::c_char).collect();

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
    use serial_test::serial;

    #[test]
    #[serial(eal)]
    fn test_eal_init_and_cleanup_lifecycle() {
        dpdk_sys::stub_eal_reset();

        let eal = Eal::init(&["test-app", "-l", "0", "-n", "4"])
            .expect("Eal::init should succeed");

        assert!(
            dpdk_sys::stub_eal_is_initialized(),
            "EAL should be initialized after Eal::init()"
        );

        drop(eal);

        assert!(
            dpdk_sys::stub_eal_is_cleaned_up(),
            "EAL should be in cleaned-up state after Eal is dropped"
        );

        dpdk_sys::stub_eal_reset();
    }

    #[test]
    #[serial(eal)]
    fn test_eal_must_be_alive_for_mempool_creation() {
        dpdk_sys::stub_eal_reset();

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
        unsafe { dpdk_sys::rte_mempool_free(ptr) };

        drop(_eal);
        dpdk_sys::stub_eal_reset();
    }

    #[test]
    #[serial(eal)]
    fn test_mempool_create_fails_after_eal_cleanup() {
        dpdk_sys::stub_eal_reset();

        // Reproduce the exact buggy sequence: init EAL, drop it, try mempool.
        // This is the pattern that caused the production segfault.
        {
            let _eal = Eal::init(&["test-app", "-l", "0", "-n", "4"])
                .expect("Eal::init should succeed");
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

        dpdk_sys::stub_eal_reset();
    }
}
