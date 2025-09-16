use crate::error::{DpdkError, DpdkResult};

pub struct Port {
    pub port_id: u16,
}

impl Port {
    pub fn new(port_id: u16) -> DpdkResult<Self> {
        Ok(Self { port_id })
    }

    pub fn start(&self) -> DpdkResult<()> {
        println!("Starting port {}", self.port_id);
        Ok(())
    }

    pub fn receive_burst(&self, _max_packets: u16) -> DpdkResult<Vec<Vec<u8>>> {
        Ok(vec![])
    }

    pub fn send_burst(&self, packets: &[Vec<u8>]) -> DpdkResult<u16> {
        println!("Sending {} packets on port {}", packets.len(), self.port_id);
        Ok(packets.len() as u16)
    }
}
