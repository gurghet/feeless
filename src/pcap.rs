use crate::bytes::Bytes;
use crate::node::header::{Header, MessageType};
use crate::node::messages::confirm_ack::ConfirmAck;
use crate::node::messages::confirm_req::ConfirmReq;
use crate::node::messages::empty::Empty;
use crate::node::messages::frontier_req::FrontierReq;
use crate::node::messages::handshake::Handshake;
use crate::node::messages::keepalive::Keepalive;
use crate::node::messages::publish::Publish;
use crate::node::messages::telemetry_ack::TelemetryAck;
use crate::node::messages::telemetry_req::TelemetryReq;
use crate::node::wire::Wire;
use crate::{to_hex, DEFAULT_PORT};
use ansi_term;
use ansi_term::Color;
use anyhow::{anyhow, Context, Error};
use etherparse::{InternetSlice, SlicedPacket};
use etherparse::{Ipv4HeaderSlice, TcpHeaderSlice, TransportSlice};
use pcarp::Capture;
use std::collections::{HashMap, HashSet};
use std::fs::File;
use std::io::{BufRead, BufReader};
use std::net::{IpAddr, Ipv4Addr};
use std::str::FromStr;
use tracing::{debug, error, info, trace, warn};

/// Subject is the focused peer that we act as "us", when showing if we're sending or
/// receiving.
//
// TODO: Infer peers or by valid header?
// Might be nicer if it can learn peers from the dump in case there are other port used.
// Another option is to just parse every packet and if the header is not valid, just
// ignore it.
// Or... just assume the first packet sent is from the subject.
#[derive(Debug, PartialEq, Eq)]
pub enum Subject {
    AutoFirstSource,
    Specified(Ipv4Addr),
}

enum Direction {
    Send,
    Recv,
}

pub struct PcapDump {
    /// Storage to continue a TCP payload for the next packet in a stream.
    stream_cont: HashMap<String, (usize, Vec<u8>)>,

    /// Frontier connections
    frontiers: HashSet<String>,

    pub expanded: bool,
    pub start_at: Option<usize>,
    pub end_at: Option<usize>,
    pub filter_addr: Option<Ipv4Addr>,
    pub abort_on_error: bool,

    subject: Subject,
    found_subject: Option<Ipv4Addr>,
}

impl PcapDump {
    pub fn new(subject: Subject) -> Self {
        let found_subject = match subject {
            Subject::Specified(s) => Some(s),
            _ => None,
        };

        PcapDump {
            stream_cont: HashMap::new(),
            frontiers: HashSet::new(),
            subject,
            found_subject,
            expanded: false,
            start_at: None,
            end_at: None,
            filter_addr: None,
            abort_on_error: false,
        }
    }

    pub fn dump(&mut self, path: &str) -> anyhow::Result<()> {
        info!("Loading dump: {}", path);

        let file = File::open(path).with_context(|| format!("Opening file {}", path))?;

        let recv_color = Color::Green.normal();
        let send_color = Color::Blue.bold();
        let direction_marker_color = Color::White.bold();
        let error_color = Color::Red;

        let mut has_started = false;
        let mut reader =
            Capture::new(&file).with_context(|| format!("Reading capture file {:?}", &file))?;
        let mut packet_idx = 0;
        'next_packet: loop {
            packet_idx += 1; // 1 based packet numbering because wireshark uses it.

            let packet = reader
                .next()
                .transpose()
                .with_context(|| format!("Reading next packet: {}", packet_idx))?;
            let packet = if packet.is_none() {
                // EOF
                return Ok(());
            } else {
                packet.unwrap()
            };
            let packet = match SlicedPacket::from_ethernet(&packet.data).with_context(|| {
                format!("Parsing packet data to ethernet for packet {}", packet_idx)
            }) {
                Ok(p) => p,
                Err(err) => {
                    warn!("Packet was no parsed correctly because: {:?}", err);
                    continue 'next_packet;
                }
            };
            let (ip, tcp, data) = match Self::process_packet(&packet) {
                Some(r) => r,
                None => continue,
            };

            // Work out direction based on subject
            if self.subject == Subject::AutoFirstSource && self.found_subject.is_none() {
                self.found_subject = Some(ip.source_addr());
            }
            let subject = self.found_subject.expect("a subject to be set by now");
            let direction = if ip.destination_addr() == subject {
                Direction::Recv
            } else if ip.source_addr() == subject {
                Direction::Send
            } else {
                warn!("Unknown direction for {} and {:?}", subject, ip);
                Direction::Recv
            };

            // Start and end packet happens after the subject code, so we can still use the
            // first source from the first packet.
            if !has_started {
                match self.start_at {
                    Some(start_at) => {
                        if start_at >= packet_idx {
                            has_started = true;
                        } else {
                            continue;
                        }
                    }
                    None => has_started = true,
                }
            }
            if let Some(end_at) = self.end_at {
                if packet_idx > end_at {
                    return Ok(());
                }
            }

            if data.len() == 0 {
                continue;
            }

            // Only look at port 7075.
            if tcp.destination_port() != DEFAULT_PORT && tcp.source_port() != DEFAULT_PORT {
                continue;
            }

            if let Some(addr) = self.filter_addr {
                if ip.source_addr() != addr && ip.destination_addr() != addr {
                    continue;
                }
            }

            let stream_id = format!(
                "{}:{}->{}:{}",
                ip.source_addr(),
                tcp.source_port(),
                ip.destination_addr(),
                tcp.destination_port()
            );

            let mut connection_id = vec![
                ip.source_addr().to_string(),
                tcp.source_port().to_string(),
                ip.destination_addr().to_string(),
                tcp.destination_port().to_string(),
            ];
            connection_id.sort();
            let connection_id = connection_id.join("-");

            debug!(
                "Packet: #{} size: {} {}",
                &packet_idx,
                data.len(),
                &stream_id
            );

            let mut v = vec![];
            let bytes = match self.stream_cont.get(&stream_id) {
                Some((other_packet_idx, b)) => {
                    // We have some left over data from a previous packet.
                    trace!(
                        "Prepending {} bytes from packet #{}.",
                        b.len(),
                        other_packet_idx
                    );
                    v.extend_from_slice(&b);
                    v.extend_from_slice(data);
                    self.stream_cont.remove(&stream_id);
                    v.as_slice()
                }
                None => {
                    trace!("Payload: {}", to_hex(data));
                    data
                }
            };

            if self.frontiers.contains(&connection_id) {
                // At this point we're only going to receive frontier messages which do not have a
                // header.

                todo!();

                continue;
            }

            let mut bytes = Bytes::new(bytes);
            while !bytes.eof() {
                if bytes.remain() < Header::LEN {
                    let remaining = Vec::from(bytes.slice(bytes.remain())?);
                    self.stream_cont
                        .insert(stream_id.clone(), (packet_idx, remaining));
                    continue 'next_packet;
                }

                let header_bytes = match bytes
                    .slice(Header::LEN)
                    .with_context(|| format!("Slicing header on packet #{}", packet_idx))
                {
                    Ok(h) => h,
                    Err(err) => {
                        if self.abort_on_error {
                            return Err(err);
                        } else {
                            error!("Error processing header: {:?}", err);
                            continue 'next_packet;
                        }
                    }
                };

                let header = match Header::deserialize(None, header_bytes)
                    .with_context(|| format!("Deserializing header on packet #{}", packet_idx))
                {
                    Ok(header) => header,
                    Err(err) => {
                        if self.abort_on_error {
                            return Err(err);
                        } else {
                            error!("Error processing header: {:?}", err);
                            continue 'next_packet;
                        }
                    }
                };
                let (direction_text, color) = match direction {
                    Direction::Send => (
                        format!(">>> {}:{}", ip.destination_addr(), tcp.destination_port()),
                        send_color,
                    ),
                    Direction::Recv => (
                        format!("<<< {}:{}", ip.source_addr(), tcp.source_port()),
                        recv_color,
                    ),
                };

                let func = match header.message_type() {
                    MessageType::Handshake => payload::<Handshake>,
                    MessageType::ConfirmReq => payload::<ConfirmReq>,
                    MessageType::ConfirmAck => payload::<ConfirmAck>,
                    MessageType::Keepalive => payload::<Keepalive>,
                    MessageType::TelemetryReq => payload::<TelemetryReq>,
                    MessageType::TelemetryAck => payload::<TelemetryAck>,
                    MessageType::Publish => payload::<Publish>,
                    MessageType::FrontierReq => payload::<FrontierReq>,
                    _ => {
                        error!("Unhandled message type {:?}", header);
                        if self.abort_on_error {
                            return Ok(());
                        }
                        continue 'next_packet;
                    }
                };
                let decoded_result = func(Some(&header), &mut bytes)
                    .with_context(|| format!("Decoding packet #{}", &packet_idx));
                let maybe_decoded = match decoded_result {
                    Ok(m) => m,
                    Err(err) => {
                        if self.abort_on_error {
                            return Err(err);
                        } else {
                            error!("Error processing header: {:?}", err);
                            continue 'next_packet;
                        }
                    }
                };

                let decoded = match maybe_decoded {
                    Some(p) => p,
                    None => {
                        bytes.seek(-(Header::LEN as i64))?;
                        let remaining = Vec::from(bytes.slice(bytes.remain())?);
                        self.stream_cont
                            .insert(stream_id.clone(), (packet_idx, remaining));
                        continue 'next_packet;
                    }
                };

                let dbg = if self.expanded {
                    format!("{:#?}", decoded.as_ref())
                } else {
                    format!("{:?}", decoded.as_ref())
                };
                println!(
                    "{} {}",
                    direction_marker_color.paint(direction_text),
                    color.paint(dbg)
                );

                if header.message_type() == MessageType::FrontierReq {
                    self.frontiers.insert(connection_id.clone());
                }
            }
        }
    }

    fn process_packet<'a>(
        packet: &'a SlicedPacket,
    ) -> Option<(&'a Ipv4HeaderSlice<'a>, &'a TcpHeaderSlice<'a>, &'a [u8])> {
        // TODO: Support IPv6
        let ip = if let Some(InternetSlice::Ipv4(ip)) = &packet.ip {
            ip
        } else {
            return None;
        };

        let tcp = if let Some(TransportSlice::Tcp(tcp)) = &packet.transport {
            tcp
        } else {
            return None;
        };

        let data_len = ip.payload_len() as usize - tcp.slice().len() as usize;
        Some((ip, tcp, &packet.payload[..data_len]))
    }
}

pub fn payload<T: 'static + Wire>(
    header: Option<&Header>,
    bytes: &mut Bytes,
) -> anyhow::Result<Option<Box<dyn Wire>>> {
    let len = T::len(header)?;

    if bytes.remain() < len {
        trace!(
            "Not enough bytes left to process. Needs {} more. Will prepend {} bytes in next packet.",
            len - bytes.remain(),
            bytes.remain()
        );
        return Ok(None);
    }

    let data = bytes.slice(len)?;
    let payload: T = T::deserialize(header, data).context("Deserializing payload")?;
    Ok(Some(Box::new(payload)))
}