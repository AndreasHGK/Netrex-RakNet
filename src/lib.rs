pub mod protocol;
pub mod server;
pub mod util;
pub mod conn;

pub const MAGIC: [u8; 16] = [0x00, 0xff, 0xff, 0x0, 0xfe, 0xfe, 0xfe, 0xfe, 0xfd, 0xfd, 0xfd, 0xfd, 0x12, 0x34, 0x56, 0x78];

pub use self::{
     protocol::*,
     util::*,
     server::*
};

#[cfg(test)]
mod tests {
     use crate::{ RakServer, IRakServer, RakEv, IRakEmit };

     #[test]
     fn rak_serv() {
          // pls work :(
          let mut serv = RakServer::new(String::from("0.0.0.0:19132"), 8);
          let channel = serv.start();
          channel.as_ref().register(Box::new(|ev| {
               match *ev {
                    RakEv::Recieve(_, _s) => println!("Got a buffer"),
                    _ => return
               }
          }));
     }
}