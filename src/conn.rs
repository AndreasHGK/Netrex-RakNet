use std::net::SocketAddr;
use std::time::SystemTime;
use std::sync::Arc;
use std::collections::{VecDeque};
use binary_utils::{BinaryStream, IBinaryStream, IBufferRead};
use crate::{IServerBound, IClientBound};
use crate::ack::{Ack, Record, queue::AckQueue, queue::NAckQueue};
use crate::frame::{Frame, FramePacket};
use crate::fragment::{Fragment, FragmentList, FragmentStore};
use crate::reliability::{Reliability, ReliabilityFlag};
use crate::protocol::offline::*;
use crate::online::{handle_online, OnlinePackets};
use crate::ack::is_ack_or_nack;

pub type RecievePacketFn = fn(&mut Connection, &mut BinaryStream);

pub trait ConnectionAPI {
     /// Called when a packet is recieved from raknet
     /// This is called on each **Frame**
     fn recieve_packet(&mut self, stream: &mut BinaryStream);

     // / Called when RakNet wants to generate a **Motd**
     // / for the server, if this fails, the `default_motd`
     // / function is called instead.
     // fn gen_motd(&mut self) -> Motd;
}

#[derive(Clone, PartialEq)]
pub enum ConnectionState {
     Connecting,
     Connected,
     Disconnected,
     Offline
}

impl ConnectionState {
     pub fn is_disconnected(&self) -> bool {
          match *self {
               Self::Disconnected => true,
               _ => false
          }
     }
     pub fn is_available(&self) -> bool {
          match *self {
               Self::Disconnected => false,
               _ => true
          }
     }
}

#[derive(Clone)]
pub struct Connection {
     /// The address the client is connected with.
     pub address: SocketAddr,
     /// The start time of the `RakNetServer`.
     pub time: SystemTime,
     /// The **Max transfer unit** for the client.
     /// Outbound buffers will be reduced to this unit.
     pub mtu_size: u16,
     /// The state of the given connection.
     /// States include:
     /// - **Connecting**: Client is not connected, but is performing connection sequence.
     /// - **Connected**: Client has performed connection sequence and is reliable.
     /// - **Disconnected**: The client is sending information, but is not connected to the server.
     /// - **Offline**: We have stopped recieving responses from the client.
     pub state: ConnectionState,
     /// A function that is called when the server recieves a
     /// `GamePacket: 0xfe` from the client.
     pub recv: Arc<RecievePacketFn>,
     /// A Vector of streams to be sent.
     /// This should almost always be a Frame, with exceptions
     /// to offline packets.
     pub send_queue: VecDeque<BinaryStream>,
     /// A list of buffers that exceed the MTU size
     /// This queue will be shortened into individual fragments,
     /// and sent to the client as fragmented frames.
     send_queue_large: VecDeque<BinaryStream>,
     /// Stores the fragmented frames by their
     /// `frame_index` value from a given packet.
     /// When a `FrameList` is ready from a `FragmentStore` it's assembled
     /// into a `FramePacket` which can then be added to the `send_queue`.
     fragmented: FragmentStore,
     /// Stores the next available fragment id.
     /// This variable will reset after the sequence
     /// containing the fragment id's we sent has been
     /// acknowledged by the client.
     ///
     /// However in the event this never occurs, fragment id will reset after
     /// it reaches `65535` as a value
     fragment_id: u16,
     /// The last recieved sequence id
     recv_seq: u32,
     /// The last send sequence id used
     send_seq: u32,
     /// The ACK queue (packets we got)
     ack: AckQueue,
     /// The NACK queue (Packets we didn't get)
     nack: NAckQueue
}

impl Connection {
     pub fn new(address: SocketAddr, start_time: SystemTime, recv: Arc<RecievePacketFn>) -> Self {
          Self {
               address,
               time: start_time,
               mtu_size: 0,
               state: ConnectionState::Disconnected,
               recv,
               send_queue: VecDeque::new(),
               send_queue_large: VecDeque::new(),
               fragmented: FragmentStore::new(),
               recv_seq: 0,
               send_seq: 0,
               fragment_id: 0,
               ack: AckQueue::new(),
               nack: NAckQueue::new(),
          }
     }

     /// Send a binary stream to the specified client.
     pub fn send(&mut self, stream: BinaryStream, instant: bool) {
          if instant {
               let mut frame_packet = FramePacket::new();
               let mut frame = Frame::init();
               frame.reliability = Reliability::new(ReliabilityFlag::Unreliable);
               frame.body = stream;
               frame_packet.seq = self.next_send_seq();
               frame_packet.frames.push(frame);
               self.send_queue.push_back(frame_packet.to());
          } else {
               self.send_queue_large.push_back(stream);
          }
     }

     /// The recieve handle for a connection.
     /// This is called when RakNet parses any given byte buffer from the socket.
     pub fn recv(&mut self, stream: &mut BinaryStream) {
          if self.state.is_disconnected() {
               let pk = OfflinePackets::recv(stream.read_byte());
               let handler = handle_offline(self, pk, stream);
               self.send_queue.push_back(handler);
          } else {
               // this packet is almost always a frame packet
               let online_packet = OnlinePackets::recv(stream.read_byte());

               if is_ack_or_nack(online_packet.to_byte()) {
                    stream.set_offset(0);
                    return self.handle_ack(stream);
               }

               if !match online_packet { OnlinePackets::FramePacket(_) => true, _ => false } {
                    return;
               }

               let mut frame_packet = FramePacket::recv(stream.clone());

               // todo Handle ack and nack!
               // todo REMOVE THIS HACK
               self.handle_ack(&mut Ack::new(0, false).to());
               self.handle_frames(&mut frame_packet);
          }
     }

     /// When the client sends an **Acknowledge**, we check:
     /// - If we have already recieved this packet.
     ///   If so, we respectfully ignore the packet.
     ///
     /// - The "records" in the acknowledge packet.
     ///   We iterate through the records, and if
     ///   any record sequence **does not exist**
     ///   we add this sequence number to the **Nack** queue,
     ///   which is then sent to the client when the connection ticks
     ///   to *hopefully* force the client to eventually send that packet.
     pub fn handle_ack(&mut self, packet: &mut BinaryStream) {
          let got = Ack::recv(packet.clone());

          for record in got.records {
               if record.is_single() {
                    let sequence = match record {
                         Record::Single(rec) => rec.sequence,
                         _ => continue
                    };

                    if !self.ack.has_seq(sequence) {
                         self.nack.push_seq(sequence);
                    }
               } else {
                    let range = match record {
                         Record::Range(rec) => rec,
                         _ => continue
                    };

                    let sequences = range.get_sequences();

                    for sequence in sequences {
                         if !self.ack.has_seq(sequence) {
                              self.nack.push_seq(sequence);
                         }
                    }
               }
          }

          if !self.ack.is_empty() {
               let respond_with = self.ack.make_ack();
               self.send_queue.push_back(respond_with.to());
               // println!("Sending ACK: {:?}", respond_with);
          }

          if !self.nack.is_empty() {
               let respond_with = self.nack.make_nack();
               self.send_queue.push_back(respond_with.to());
               // println!("Sending NACK: {:?}", respond_with);
          }
     }

     /// Iterates over every `Frame` of the `FramePacket` and does the following checks:
     /// - Checks if the frame is fragmented, if it is,
     ///   we check if all fragments have been sent to the server.
     ///   If all packets have been sent, we "re-assemble" them.
     ///   If not, we simply add the fragment to a fragment list,
     ///   and continue to the next frame
     ///
     /// - If it is not fragmented, we handle the frames body. (Which should contain a valid RakNet payload)
     pub fn handle_frames(&mut self, frame_packet: &mut FramePacket) {
          self.ack.push_seq(frame_packet.seq, frame_packet.to());
          for frame in frame_packet.frames.iter_mut() {
               if frame.fragment_info.is_some() {
                    // the frame is fragmented!
                    self.fragmented.add_frame(frame.clone());
                    let frag_list = &self.fragmented.get(frame.fragment_info.unwrap().fragment_id);

                    if frag_list.is_some() {
                         let mut list = frag_list.clone().unwrap();
                         let pk = list.reassemble_frame();
                         if pk.is_some() {
                              self.handle_full_frame(&mut pk.unwrap());
                              self.fragmented.remove(frame.fragment_info.unwrap().fragment_id.into());
                         }
                    }
                    continue;
               } else {
                    self.handle_full_frame(frame);
               }
          }
     }

     /// Handles the full frame from the client.
     fn handle_full_frame(&mut self, frame: &mut Frame) {
          // todo Check if the frames should be recieved, if not purge them
          // todo EG: add implementation for ordering and sequenced frames!
          let online_packet = OnlinePackets::recv(frame.body.clone().read_byte());

          if online_packet == OnlinePackets::GamePacket {
               self.recv.as_ref()(self, &mut frame.body);
          } else {
               let mut response = handle_online(self, online_packet.clone(), &mut frame.body);

               if response.get_length() != 0 {
                    if response.get_length() as u16 > self.mtu_size {
                         self.fragment(&mut response)
                    } else {
                         let mut new_framepk = FramePacket::new();
                         let mut new_frame = Frame::init();

                         new_frame.body = response.clone();
                         new_frame.reliability = Reliability::new(ReliabilityFlag::Unreliable);
                         new_framepk.frames.push(new_frame);
                         new_framepk.seq = self.send_seq;
                         self.send_queue.push_back(new_framepk.to());
                         self.send_seq = self.send_seq + 1;
                    }
               }

               // println!("\nSent: {:?}", response.clone());
               // self.send_queue.push_back(response);
          }
     }

     pub fn next_send_seq(&mut self) -> u32 {
          let old = self.send_seq.clone();
          self.send_seq += 1;
          old
     }

     /// Called when RakNet is ready to "tick" this client.
     /// Each "tick" the following things are done:
     ///
     /// - Send all **Ack** and **Nack** queues to the client.
     ///
     /// - Fragments everything in the `send_queue_large` queue,
     ///   and then appends all of these "buffers" or "binarystreams"
     ///   to be sent by raknet on the next iteration.
     pub fn do_tick(&mut self) {
          // does a tick
     }

     /// Automatically fragment the stream based on the clients mtu
     /// size and add the frames to the handler queue.
     /// todo FIX THIS
     pub fn fragment(&mut self, stream: &mut BinaryStream) {
          let usable_id = self.fragment_id + 1;

          if usable_id == 65535 {
               self.fragment_id = 0;
          }

          let mut fragment_list = FragmentList::new();
          let mut index: i32 = 0;
          let mut offset: usize = stream.get_length();

          loop {
               if offset == 0 {
                    break;
               }

               let mut next = BinaryStream::init(&stream.get_buffer());

               if stream.get_length() > self.mtu_size as usize {
                    next = stream.slice(0, Some(self.mtu_size as usize));
                    offset -= self.mtu_size as usize;
               } else {
                    offset -= stream.get_length();
               }

               let frag = Fragment::new(index as i32, next.get_buffer());

               fragment_list.add_fragment(frag);
               index += 1;
          }

          let _packets = fragment_list.assemble(self.mtu_size as i16, usable_id);
          // if packets.is_some() {
          //      for packet in packets.unwrap().iter_mut() {
          //           packet.seq = self.send_seq + 1;

          //           self.send_queue.push_back(packet.to());
          //      }
          // }

          self.fragment_id += 1;
     }
}