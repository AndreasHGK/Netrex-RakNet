use binary_utils::stream::*;
use binary_utils::{ IBufferRead, IBufferWrite };
use std::net::{ SocketAddr, IpAddr };
use crate::MAGIC;

// Raknet utilities
pub trait IPacketStreamWrite {
     fn write_magic(&mut self);

     fn write_address(&mut self, add: SocketAddr);
}

pub trait IPacketStreamRead {
     fn read_magic(&mut self) -> Vec<u8>;

     fn read_address(&mut self) -> SocketAddr;
}

impl IPacketStreamWrite for BinaryStream {
     fn write_magic(&mut self) {
          self.write_slice(&MAGIC);
     }

     fn write_address(&mut self, add: SocketAddr) {
          if add.is_ipv4() {
               self.write_byte(4);
          } else {
               self.write_byte(6);
          }

          let ipst = add.ip().to_string();
          let ipts: Vec<&str> = ipst.split(".").collect();

          for p in ipts {
               let byte = u8::from_str_radix(p, 10).unwrap();
               self.write_byte(byte);
          }
          self.write_ushort(add.port());
     }
}

impl IPacketStreamRead for BinaryStream {
     fn read_magic(&mut self) -> Vec<u8> {
          self.read_slice(Some(16))
     }

     fn read_address(&mut self) -> SocketAddr {
          let addr_type = self.read_byte();
          if addr_type == 4 {
               let parts = self.read_slice(Some(4 as usize));
               let port = self.read_ushort();
               SocketAddr::new(IpAddr::from([parts[0], parts[1], parts[2], parts[3]]), port)
          } else {
               SocketAddr::new(IpAddr::from([0,0,0,0]), 0)
          }
     }
}

pub fn tokenize_addr(remote: SocketAddr) -> String {
     let mut address = remote.ip().to_string();
     address.push_str(":");
     address.push_str(remote.port().to_string().as_str());
     return address;
}

pub fn from_tokenized(remote: String) -> SocketAddr {
     let parsed: SocketAddr = remote.parse().expect("Could not retrieve address from token.");
     parsed
}