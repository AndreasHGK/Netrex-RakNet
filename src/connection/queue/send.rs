use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::Arc;

#[cfg(feature = "async_std")]
use async_std::net::UdpSocket;
use binary_utils::Streamable;
#[cfg(feature = "async_tokio")]
use tokio::net::UdpSocket;

use crate::protocol::ack::{Ack, Ackable, Record, SingleRecord};
use crate::protocol::frame::{Frame, FramePacket};
use crate::protocol::packet::Packet;
use crate::protocol::reliability::Reliability;
use crate::protocol::RAKNET_HEADER_FRAME_OVERHEAD;
use crate::rakrs_debug;
use crate::util::{to_address_token, SafeGenerator};

use super::{FragmentQueue, FragmentQueueError, NetQueue, RecoveryQueue};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum SendQueueError {
    /// The packet is too large to be sent.
    PacketTooLarge,
    /// Parsing Error
    ParseError,
    /// Fragmentation error
    FragmentError(FragmentQueueError),
    /// Send queue error
    SendError,
}

/// This queue is used to prioritize packets being sent out
/// Packets that are old, are either dropped or requested again.
/// You can define this behavior with the `timeout` property.
#[derive(Debug, Clone)]
pub struct SendQueue {
    mtu_size: u16,

    /// The amount of time that needs to pass for a packet to be
    /// dropped or requested again.
    _timeout: u16,

    /// The amount of times we should retry sending a packet before
    /// dropping it from the queue. This is currently set to `5`.
    _max_tries: u16,

    /// The current sequence number. This is incremented every time
    /// a packet is sent reliably. We can resend these if they are
    /// NAcked.
    send_seq: SafeGenerator<u32>,

    /// The current reliable index number.
    /// a packet is sent reliably an sequenced.
    reliable_seq: SafeGenerator<u32>,

    /// The current recovery queue.
    ack: RecoveryQueue<FramePacket>,

    /// The fragment queue.
    fragment_queue: FragmentQueue,

    /// The ordered channels.
    /// (send_seq, reliable_seq)
    order_channels: HashMap<u8, (u32, u32)>,

    ready: Vec<Frame>,

    socket: Arc<UdpSocket>,

    address: SocketAddr,
}

impl SendQueue {
    pub fn new(
        mtu_size: u16,
        _timeout: u16,
        _max_tries: u16,
        socket: Arc<UdpSocket>,
        address: SocketAddr,
    ) -> Self {
        Self {
            mtu_size,
            _timeout,
            _max_tries,
            send_seq: SafeGenerator::new(),
            reliable_seq: SafeGenerator::new(),
            ack: RecoveryQueue::new(),
            fragment_queue: FragmentQueue::new(),
            order_channels: HashMap::new(),
            ready: Vec::new(),
            socket,
            address,
        }
    }

    /// Send a packet based on its reliability.
    /// Note, reliability will be set to `Reliability::ReliableOrd` if
    /// the buffer is larger than max MTU.
    pub async fn insert(
        &mut self,
        packet: Vec<u8>,
        reliability: Reliability,
        immediate: bool,
        channel: Option<u8>,
    ) -> Result<(), SendQueueError> {
        let reliable = if packet.len() > (self.mtu_size + RAKNET_HEADER_FRAME_OVERHEAD) as usize {
            Reliability::ReliableOrd
        } else {
            reliability
        };

        match reliability {
            Reliability::Unreliable => {
                // we can just send this packet out immediately.
                let frame = Frame::new(Reliability::Unreliable, Some(packet));
                self.send_frame(frame).await;
                return Ok(());
            }
            Reliability::Reliable => {
                // we need to send this packet out reliably.
                let frame = Frame::new(Reliability::Reliable, Some(packet));
                self.send_frame(frame).await;
                return Ok(());
            }
            _ => {}
        };

        // do another integrity check
        // this is to check to see if we really need to split this packet.
        if packet.len() > (self.mtu_size + RAKNET_HEADER_FRAME_OVERHEAD) as usize {
            // we need to split this packet!
            // pass the buffer to the fragment queue.
            let mut pk = FramePacket::new();
            pk.sequence = self.send_seq.next();
            pk.reliability = reliability;

            let fragmented = self.fragment_queue.split_insert(&packet, self.mtu_size);

            if fragmented.is_ok() {
                let frag_id = fragmented.unwrap();
                let (_, frames) = self.fragment_queue.get_mut(&frag_id).unwrap();
                let (ord_seq, ord_index) = self
                    .order_channels
                    .entry(channel.unwrap_or(0))
                    .or_insert((0, 0));
                *ord_index = ord_index.wrapping_add(1);
                *ord_seq = ord_seq.wrapping_add(1);

                for frame in frames.iter_mut() {
                    frame.reliability = reliability;
                    frame.sequence_index = Some(*ord_seq);
                    frame.order_channel = Some(channel.unwrap_or(0));
                    frame.order_index = Some(*ord_index);

                    if frame.reliability.is_reliable() {
                        frame.reliable_index = Some(self.reliable_seq.next());
                    }
                }

                // Add this frame packet to the recovery queue.
                if let Ok(p) = pk.parse() {
                    self.send_stream(&p[..]).await;
                    self.ack.insert_id(pk.sequence, pk);
                    return Ok(());
                } else {
                    return Err(SendQueueError::SendError);
                }
            } else {
                // we couldn't send this frame!
                return Err(SendQueueError::FragmentError(fragmented.unwrap_err()));
            }
        } else {
            // we're not gonna send this frame out yet!
            // we need to wait for the next tick.
            let mut frame = Frame::new(reliable, Some(packet));

            if frame.reliability.is_reliable() {
                frame.reliable_index = Some(self.reliable_seq.next());
            }

            if frame.reliability.is_ordered() {
                let (_, ord_index) = self
                    .order_channels
                    .entry(channel.unwrap_or(0))
                    .or_insert((0, 0));
                *ord_index = ord_index.wrapping_add(1);
                frame.order_index = Some(*ord_index);
                frame.sequence_index = Some(self.send_seq.get());
            } else if frame.reliability.is_sequenced() {
                let (seq_index, ord_index) = self
                    .order_channels
                    .entry(channel.unwrap_or(0))
                    .or_insert((0, 0));
                *seq_index = seq_index.wrapping_add(1);
                frame.order_index = Some(*ord_index);
                frame.sequence_index = Some(*seq_index);
            }

            if immediate {
                self.send_frame(frame).await;
            } else {
                self.ready.push(frame);
            }

            return Ok(());
        }
    }

    /// A wrapper to send a single frame over the wire.
    /// While also reliabily tracking it.
    async fn send_frame(&mut self, mut frame: Frame) {
        let mut pk = FramePacket::new();
        pk.sequence = self.send_seq.next();
        pk.reliability = frame.reliability;

        if pk.reliability.is_reliable() {
            frame.reliable_index = Some(self.reliable_seq.next());
        }

        pk.frames.push(frame);

        if pk.reliability.is_reliable() {
            // this seems redundant, but we need to insert the packet into the ACK queue
            self.ack.insert_id(self.reliable_seq.get(), pk.clone());
        }

        if let Ok(buf) = pk.parse() {
            self.send_stream(&buf[..]).await;
        }
    }

    pub(crate) async fn send_stream(&mut self, packet: &[u8]) {
        if let Err(e) = self.socket.send_to(packet, &self.address).await {
            // we couldn't sent the packet!
            rakrs_debug!(
                true,
                "[{}] Failed to send packet! {:?}",
                to_address_token(self.address),
                e
            );
        }
    }

    pub async fn send_packet(
        &mut self,
        packet: Packet,
        reliability: Reliability,
        immediate: bool,
    ) -> Result<(), SendQueueError> {
        // parse the packet
        if let Ok(buf) = packet.parse() {
            if let Err(e) = self.insert(buf, reliability, immediate, None).await {
                rakrs_debug!(
                    true,
                    "[{}] Failed to insert packet into send queue: {:?}",
                    to_address_token(self.address),
                    e
                );
                return Err(e);
            }
            return Ok(());
        } else {
            return Err(SendQueueError::ParseError);
        }
    }

    pub async fn update(&mut self) {
        // send all the ready packets
        // TODO batch these packets together
        // TODO by lengths
        for frame in self.ready.drain(..).collect::<Vec<Frame>>() {
            self.send_frame(frame).await;
        }

        // Flush ACK
        // check to see if we need to resend any packets.
        // TODO actually implement this
        let resend_queue = self.ack.flush().unwrap();
        // let mut resend_queue = Vec::<FramePacket>::new();

        // for (seq, packet) in self.ack.get_all() {
        //     if packet.resend_time < Instant::now() {
        //         resend_queue.push(packet.clone());
        //     }
        // }

        for packet in resend_queue.iter() {
            if let Ok(buf) = packet.parse() {
                self.send_stream(&buf[..]).await;
            }
        }
    }
}

impl Ackable for SendQueue {
    type NackItem = FramePacket;

    fn ack(&mut self, ack: Ack) {
        if ack.is_nack() {
            return;
        }

        // these packets are acknowledged, so we can remove them from the queue.
        for record in ack.records.iter() {
            match record {
                Record::Single(SingleRecord { sequence }) => {
                    if let Ok(_) = self.ack.remove(*sequence) {};
                }
                Record::Range(ranged) => {
                    for i in ranged.start..ranged.end {
                        if let Ok(_) = self.ack.remove(i) {};
                    }
                }
            }
        }
    }

    fn nack(&mut self, nack: Ack) -> Vec<FramePacket> {
        if !nack.is_nack() {
            return Vec::new();
        }

        let mut resend_queue = Vec::<FramePacket>::new();

        // we need to get the packets to resend.
        for record in nack.records.iter() {
            match record {
                Record::Single(single) => {
                    if let Ok(packet) = self.ack.get(single.sequence) {
                        resend_queue.push(packet.clone());
                    }
                }
                Record::Range(ranged) => {
                    for i in ranged.start..ranged.end {
                        if let Ok(packet) = self.ack.get(i) {
                            resend_queue.push(packet.clone());
                        }
                    }
                }
            }
        }

        return resend_queue;
    }
}