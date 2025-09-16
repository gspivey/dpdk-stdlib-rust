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
